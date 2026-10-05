//! 阿尔比恩本地服务（Pi，`/root/albion-pi`）的 OpenAI 兼容通路：本机 `127.0.0.1:8650`。
//!
//! 只连本机明文 HTTP，所以自己开 TcpStream，不引 HTTP 依赖。
//! ponytail: 只处理本机 HTTP/1.1 + Content-Length / chunked / 读到 EOF 三种响应体；
//! 要跨机或走 TLS 就该换 reqwest/ureq，别在这上面加补丁。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

pub const DEFAULT_ENDPOINT: &str = "127.0.0.1:8650";

/// 端点覆盖（验收脚本指向别处时用）。
pub fn endpoint() -> String {
    std::env::var("AGENT_HUB_ALBION_ENDPOINT").unwrap_or_else(|_| DEFAULT_ENDPOINT.to_string())
}

fn connect(endpoint: &str) -> Result<TcpStream, String> {
    let stream = TcpStream::connect(endpoint)
        .map_err(|error| format!("连不上本地服务 {endpoint}：{error}"))?;
    // 一轮耗时按分钟算（读文档、写扩展），读超时给足。
    stream.set_read_timeout(Some(Duration::from_secs(900))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(30))).ok();
    Ok(stream)
}

/// 本地服务的 `.env`（`API_SERVER_KEY` 的单一来源，不复制凭据）。
fn env_file() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("AGENT_HUB_ALBION_ENV") {
        return Some(PathBuf::from(path));
    }
    [
        r"\\wsl.localhost\Ubuntu\root\albion-pi\agent\.env",
        r"\\wsl.localhost\Ubuntu\root\.hermes\profiles\albion\.env",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|path| path.is_file())
}

/// 脑要的 key：环境变量优先，否则读她脑自己的 `.env`。只进内存，绝不写日志。
fn token() -> Option<String> {
    if let Ok(value) = std::env::var("AGENT_HUB_ALBION_TOKEN") {
        let value = value.trim().to_string();
        if !value.is_empty() {
            return Some(value);
        }
    }
    let text = std::fs::read_to_string(env_file()?).ok()?;
    text.lines()
        .find_map(|line| line.trim().strip_prefix("API_SERVER_KEY="))
        .map(|value| {
            value
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string()
        })
        .filter(|value| !value.is_empty())
}

/// 只给本机端点带凭据：端点被指向远端时绝不外泄 key。
fn is_local(endpoint: &str) -> bool {
    let host = endpoint
        .rsplit_once(':')
        .map(|(host, _)| host)
        .unwrap_or(endpoint);
    matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "::1")
}

/// 发一个请求，把响应体按到达顺序喂给 `on_chunk`，返回 (状态码, 完整正文)。
fn request(
    endpoint: &str,
    method: &str,
    path: &str,
    body: Option<&str>,
    mut on_chunk: impl FnMut(&[u8]),
) -> Result<(u16, String), String> {
    let mut stream = connect(endpoint)?;
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {endpoint}\r\nConnection: close\r\n");
    if is_local(endpoint) {
        if let Some(token) = token() {
            head.push_str(&format!("Authorization: Bearer {token}\r\n"));
        }
    }
    if let Some(body) = body {
        head.push_str("Content-Type: application/json\r\n");
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .and_then(|_| match body {
            Some(body) => stream.write_all(body.as_bytes()),
            None => Ok(()),
        })
        .map_err(|error| format!("请求本地服务失败：{error}"))?;

    let mut reader = BufReader::new(stream);
    let status = read_status(&mut reader)?;
    let mut chunked = false;
    let mut length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .map_err(|error| format!("读响应头失败：{error}"))?;
        if read == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("transfer-encoding:") {
            chunked = value.contains("chunked");
        } else if let Some(value) = lower.strip_prefix("content-length:") {
            length = value.trim().parse::<usize>().ok();
        }
    }

    let mut raw: Vec<u8> = Vec::new();
    match (chunked, length) {
        (false, Some(size)) => {
            // 定长：按块读到够，边读边给。
            let mut remaining = size;
            let mut buffer = [0u8; 8192];
            while remaining > 0 {
                let want = remaining.min(buffer.len());
                let read = reader
                    .read(&mut buffer[..want])
                    .map_err(|error| format!("读响应体失败：{error}"))?;
                if read == 0 {
                    break;
                }
                raw.extend_from_slice(&buffer[..read]);
                on_chunk(&buffer[..read]);
                remaining -= read;
            }
        }
        (true, _) => loop {
            let mut size_line = String::new();
            if reader
                .read_line(&mut size_line)
                .map_err(|error| format!("读分块长度失败：{error}"))?
                == 0
            {
                break;
            }
            let size = usize::from_str_radix(size_line.trim().split(';').next().unwrap_or("0"), 16)
                .map_err(|_| format!("分块长度无法解析：{}", size_line.trim()))?;
            if size == 0 {
                break;
            }
            let mut chunk = vec![0u8; size];
            reader
                .read_exact(&mut chunk)
                .map_err(|error| format!("读分块失败：{error}"))?;
            raw.extend_from_slice(&chunk);
            on_chunk(&chunk);
            let mut tail = [0u8; 2];
            reader.read_exact(&mut tail).ok();
        },
        // 无长度、非分块：读到连接关闭为止。
        _ => {
            let mut buffer = [0u8; 8192];
            loop {
                let read = reader
                    .read(&mut buffer)
                    .map_err(|error| format!("读响应体失败：{error}"))?;
                if read == 0 {
                    break;
                }
                raw.extend_from_slice(&buffer[..read]);
                on_chunk(&buffer[..read]);
            }
        }
    }
    Ok((status, String::from_utf8_lossy(&raw).into_owned()))
}

fn read_status(reader: &mut BufReader<TcpStream>) -> Result<u16, String> {
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|error| format!("读状态行失败：{error}"))?;
    line.split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| format!("状态行无法解析：{}", line.trim()))
}

/// 模型目录（`/v1/models`）。
pub fn models(endpoint: &str) -> Result<Vec<String>, String> {
    let (status, body) = request(endpoint, "GET", "/v1/models", None, |_| {})?;
    if status != 200 {
        return Err(format!("本地服务回了 HTTP {status}：{}", body.trim()));
    }
    let parsed: serde_json::Value =
        serde_json::from_str(&body).map_err(|error| format!("模型目录无法解析：{error}"))?;
    let list = parsed
        .get("data")
        .and_then(|value| value.as_array())
        .ok_or("模型目录缺少 data")?;
    Ok(list
        .iter()
        .filter_map(|item| item.get("id").and_then(|id| id.as_str()))
        .map(str::to_string)
        .collect())
}

/// 一轮对话：把 `messages` 发过去，边收边把增量交给 `on_delta`，返回完整正文。
///
/// 工具、记忆、说话守卫都在她脑里，这里只当 OpenAI 兼容客户端；历史由调用方每轮重发。
pub fn chat(
    endpoint: &str,
    messages: &serde_json::Value,
    mut on_delta: impl FnMut(&str),
) -> Result<String, String> {
    let payload = serde_json::json!({
        "model": "albion",
        "messages": messages,
        "stream": true,
    });
    let body = serde_json::to_string(&payload).map_err(|error| error.to_string())?;
    let mut pending = String::new();
    let mut text = String::new();
    let (status, _) = request(
        endpoint,
        "POST",
        "/v1/chat/completions",
        Some(&body),
        |chunk| {
            pending.push_str(&String::from_utf8_lossy(chunk));
            // SSE 一行为一条：不完整的那截留在 pending 里等下一块。
            while let Some(cut) = pending.find('\n') {
                let line = pending[..cut].trim_end_matches('\r').to_string();
                pending.drain(..cut + 1);
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                if let Some(delta) = delta_text(data.trim()) {
                    text.push_str(&delta);
                    on_delta(&delta);
                }
            }
        },
    )?;
    if status != 200 {
        return Err(format!("本地服务回了 HTTP {status}"));
    }
    if text.trim().is_empty() {
        return Err("本地服务没有给出正文".to_string());
    }
    Ok(text)
}

/// 从一条 `data:` 负载里取增量文字；`[DONE]`、坏 JSON、空增量都返回 None。
fn delta_text(data: &str) -> Option<String> {
    if data == "[DONE]" {
        return None;
    }
    let parsed: serde_json::Value = serde_json::from_str(data).ok()?;
    let content = parsed
        .get("choices")?
        .as_array()?
        .first()?
        .get("delta")?
        .get("content")?
        .as_str()?;
    (!content.is_empty()).then(|| content.to_string())
}

#[cfg(test)]
mod tests {
    use super::delta_text;

    /// 只验纯解析：增量取值 + 该忽略的三类（完成标记、坏 JSON、空增量）。
    #[test]
    fn delta_text_只取该取的() {
        assert_eq!(
            delta_text(r#"{"choices":[{"delta":{"content":"阿尔比恩"}}]}"#).as_deref(),
            Some("阿尔比恩")
        );
        assert_eq!(delta_text("[DONE]"), None);
        assert_eq!(delta_text("{不是 JSON"), None);
        assert_eq!(
            delta_text(r#"{"choices":[{"delta":{"content":""}}]}"#),
            None
        );
        assert_eq!(delta_text(r#"{"choices":[{"delta":{}}]}"#), None);
    }

    /// 真机自检：本地服务在 127.0.0.1:8650 时必须能取模型、能收流式正文。
    /// 默认忽略（cargo test 不该依赖外部脑在线）：`cargo test -- --ignored pi_真机`
    #[test]
    #[ignore]
    fn pi_真机取模型与流式对话() {
        let endpoint = super::endpoint();
        let list = super::models(&endpoint).expect("取模型目录");
        assert!(!list.is_empty(), "模型目录不该为空");
        let mut seen = 0usize;
        let messages = serde_json::json!([{"role": "user", "content": "只回复两个字：正常"}]);
        let text = super::chat(&endpoint, &messages, |_delta| seen += 1).expect("一轮对话");
        println!("模型={list:?} 增量={seen} 正文={text}");
        assert!(seen > 0, "没有收到任何流式增量");
        assert!(!text.trim().is_empty(), "正文不该为空");
    }
}
