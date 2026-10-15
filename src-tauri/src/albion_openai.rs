//! 伴侣的 OpenAI 兼容通路：`/models` 取模型目录，`/chat/completions` 收流式正文。
//!
//! 地址、Key、模型、人设都来自用户的伴侣配置（见 `companion.rs`），http 与 https 都支持。

use crate::companion::Link;
use std::io::{BufRead, BufReader};
use std::time::Duration;

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        // 一轮耗时按分钟算（读文档、写扩展），读超时给足。
        .timeout_read(Duration::from_secs(900))
        .build()
}

fn authorized(request: ureq::Request, link: &Link) -> ureq::Request {
    match &link.key {
        Some(key) => request.set("Authorization", &format!("Bearer {key}")),
        None => request,
    }
}

fn failure(link: &Link, error: ureq::Error) -> String {
    match error {
        ureq::Error::Status(code, response) => {
            let body = response.into_string().unwrap_or_default();
            let hint = match code {
                401 | 403 => "（Key 不对或没有权限）",
                404 => "（地址不对？大多数服务以 /v1 结尾）",
                _ => "",
            };
            format!("伴侣服务回了 HTTP {code}{hint}：{}", body.trim().chars().take(300).collect::<String>())
        }
        ureq::Error::Transport(transport) => format!("连不上伴侣服务 {}：{transport}", link.base),
    }
}

/// 模型目录。
pub fn models(link: &Link) -> Result<Vec<String>, String> {
    let response = authorized(agent().get(&format!("{}/models", link.base)), link)
        .call()
        .map_err(|error| failure(link, error))?;
    let parsed: serde_json::Value = response.into_json().map_err(|error| format!("模型目录无法解析：{error}"))?;
    let list = parsed.get("data").and_then(|value| value.as_array()).ok_or("模型目录缺少 data")?;
    Ok(list
        .iter()
        .filter_map(|item| item.get("id").and_then(|id| id.as_str()))
        .map(str::to_string)
        .collect())
}

/// 一轮对话：把 `messages` 发过去，边收边把增量交给 `on_delta`，返回完整正文。
/// 配了人设就作为第一条 system 消息；历史由调用方每轮重发。
pub fn chat(link: &Link, model: &str, messages: &serde_json::Value, mut on_delta: impl FnMut(&str)) -> Result<String, String> {
    let mut all = Vec::new();
    if let Some(persona) = &link.persona {
        all.push(serde_json::json!({ "role": "system", "content": persona }));
    }
    all.extend(messages.as_array().cloned().unwrap_or_default());
    let payload = serde_json::json!({ "model": model, "messages": all, "stream": true });
    let response = authorized(agent().post(&format!("{}/chat/completions", link.base)), link)
        .set("Content-Type", "application/json")
        .send_string(&payload.to_string())
        .map_err(|error| failure(link, error))?;
    let mut text = String::new();
    for line in BufReader::new(response.into_reader()).lines() {
        let line = line.map_err(|error| format!("读取伴侣回复中断：{error}"))?;
        // SSE 一行为一条；不是 data: 的行（注释、空行、event:）都跳过。
        let Some(data) = line.trim_end_matches('\r').strip_prefix("data:") else {
            continue;
        };
        if let Some(delta) = delta_text(data.trim()) {
            text.push_str(&delta);
            on_delta(&delta);
        }
    }
    if text.trim().is_empty() {
        return Err("伴侣服务没有给出正文".to_string());
    }
    Ok(text)
}

/// 从一条 `data:` 负载里取增量文字；`[DONE]`、坏 JSON、空增量都返回 None。
fn delta_text(data: &str) -> Option<String> {
    if data == "[DONE]" {
        return None;
    }
    let parsed: serde_json::Value = serde_json::from_str(data).ok()?;
    let content = parsed.get("choices")?.as_array()?.first()?.get("delta")?.get("content")?.as_str()?;
    (!content.is_empty()).then(|| content.to_string())
}

#[cfg(test)]
mod tests {
    use super::delta_text;

    /// 只验纯解析：增量取值 + 该忽略的三类（完成标记、坏 JSON、空增量）。
    #[test]
    fn delta_text_只取该取的() {
        assert_eq!(delta_text(r#"{"choices":[{"delta":{"content":"你好"}}]}"#).as_deref(), Some("你好"));
        assert_eq!(delta_text("[DONE]"), None);
        assert_eq!(delta_text("{不是 JSON"), None);
        assert_eq!(delta_text(r#"{"choices":[{"delta":{"content":""}}]}"#), None);
        assert_eq!(delta_text(r#"{"choices":[{"delta":{}}]}"#), None);
    }

    /// 本地起一个假的 OpenAI 兼容服务，验证取模型、带 Key、收流式正文、人设作为 system 发出。
    #[test]
    fn talks_to_an_openai_compatible_service() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buffer = vec![0u8; 16384];
                let mut request = String::new();
                loop {
                    let read = stream.read(&mut buffer).unwrap();
                    request.push_str(&String::from_utf8_lossy(&buffer[..read]));
                    let Some(head_end) = request.find("\r\n\r\n") else { continue };
                    let length = request[..head_end]
                        .lines()
                        .find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap()))
                        .unwrap_or(0);
                    if request.len() >= head_end + 4 + length || read == 0 {
                        break;
                    }
                }
                let body = if request.starts_with("GET /v1/models") {
                    r#"{"data":[{"id":"companion-1"}]}"#.to_string()
                } else {
                    "data: {\"choices\":[{\"delta\":{\"content\":\"在\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"呢\"}}]}\n\ndata: [DONE]\n\n".to_string()
                };
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                seen.push(request);
            }
            seen
        });
        let link = crate::companion::Link {
            base: format!("http://127.0.0.1:{port}/v1"),
            key: Some("sk-x".into()),
            model: None,
            persona: Some("你是温柔的伴侣".into()),
        };
        assert_eq!(super::models(&link).unwrap(), vec!["companion-1".to_string()]);
        let mut deltas = 0;
        let text = super::chat(&link, "companion-1", &serde_json::json!([{"role":"user","content":"在吗"}]), |_| deltas += 1).unwrap();
        assert_eq!((text.as_str(), deltas), ("在呢", 2));
        let requests = server.join().unwrap();
        assert!(requests.iter().all(|request| request.contains("Bearer sk-x")));
        assert!(requests[1].contains("你是温柔的伴侣") && requests[1].contains("\"system\""));
    }
}
