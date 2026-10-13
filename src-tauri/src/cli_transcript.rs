//! 会话从 CLI 收回时，读出移交期间 CLI 里新增的对话，补进同席的聊天记录。
//! 各成员的原生会话库格式不同：Claude 的 ~/.claude/projects/*/<id>.jsonl、Codex 的
//! ~/.codex/sessions/**/rollout-*-<id>.jsonl、Hermes 的 state.db、DSH 的 zstd 分帧 jsonl。
//! 这里只取“人说的话”和“成员的回复/思考”，工具调用、注入的系统上下文都不搬。
use serde_json::Value;
use std::path::{Path, PathBuf};

type Result<T> = std::result::Result<T, String>;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Turn {
    pub user: bool,
    pub text: String,
    pub thought: String,
    /// 毫秒时间戳。
    pub at: i64,
}

/// 连续的成员输出（一轮里的多段文字、思考）合成一条回复；空回复丢掉。
fn push(turns: &mut Vec<Turn>, user: bool, text: &str, thought: &str, at: i64) {
    let (text, thought) = (text.trim(), thought.trim());
    if user {
        if !text.is_empty() {
            turns.push(Turn {
                user,
                text: text.into(),
                thought: String::new(),
                at,
            });
        }
        return;
    }
    if text.is_empty() && thought.is_empty() {
        return;
    }
    if let Some(last) = turns.last_mut().filter(|last| !last.user) {
        for (target, piece) in [(&mut last.text, text), (&mut last.thought, thought)] {
            if !piece.is_empty() {
                if !target.is_empty() {
                    target.push_str("\n\n");
                }
                target.push_str(piece);
            }
        }
        return;
    }
    turns.push(Turn {
        user,
        text: text.into(),
        thought: thought.into(),
        at,
    });
}

/// 注入的环境/指令文本不是人说的话。
fn injected(text: &str) -> bool {
    let text = text.trim_start();
    text.starts_with('<')
        || text.starts_with("# AGENTS.md")
        || text.contains("<INSTRUCTIONS>")
        || text.starts_with("Current runtime context")
        || text.starts_with("Caveat:")
}

/// "2026-10-13T15:22:53.014Z" → 毫秒。
pub(crate) fn iso_ms(value: &str) -> Option<i64> {
    let (date, time) = value.trim_end_matches('Z').split_once('T')?;
    let mut date = date.split('-').map(|part| part.parse::<i64>().ok());
    let (year, month, day) = (date.next()??, date.next()??, date.next()??);
    let (clock, fraction) = time.split_once('.').unwrap_or((time, "0"));
    let mut clock = clock.split(':').map(|part| part.parse::<i64>().ok());
    let (hour, minute, second) = (clock.next()??, clock.next()??, clock.next()??);
    let millis: i64 = format!("{fraction:0<3}")[..3].parse().ok()?;
    let (y, m) = if month <= 2 {
        (year - 1, month + 9)
    } else {
        (year, month - 3)
    };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400 + hour * 3600 + minute * 60 + second) * 1000) + millis)
}

fn blocks_text(content: &Value, kinds: &[&str], field: &str) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter(|item| kinds.contains(&item["type"].as_str().unwrap_or("")))
            .filter_map(|item| item[field].as_str())
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

/// Claude Code：每行一条记录，user/assistant 带 ISO 时间。
pub(crate) fn claude_turns(jsonl: &str, since: i64) -> Vec<Turn> {
    let mut turns = Vec::new();
    for line in jsonl.lines() {
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(at) = row["timestamp"].as_str().and_then(iso_ms) else {
            continue;
        };
        if at < since || row["isSidechain"] == true || row["isMeta"] == true {
            continue;
        }
        let content = &row["message"]["content"];
        match row["type"].as_str() {
            Some("user") => {
                let text = blocks_text(content, &["text"], "text");
                if !injected(&text) {
                    push(&mut turns, true, &text, "", at);
                }
            }
            Some("assistant") => push(
                &mut turns,
                false,
                &blocks_text(content, &["text"], "text"),
                &blocks_text(content, &["thinking"], "thinking"),
                at,
            ),
            _ => {}
        }
    }
    turns
}

/// Codex：rollout 里的 response_item（message / reasoning）。
pub(crate) fn codex_turns(jsonl: &str, since: i64) -> Vec<Turn> {
    let mut turns = Vec::new();
    for line in jsonl.lines() {
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if row["type"] != "response_item" {
            continue;
        }
        let Some(at) = row["timestamp"].as_str().and_then(iso_ms) else {
            continue;
        };
        if at < since {
            continue;
        }
        let payload = &row["payload"];
        match (payload["type"].as_str(), payload["role"].as_str()) {
            (Some("message"), Some("user")) => {
                let text = blocks_text(&payload["content"], &["input_text"], "text");
                if !injected(&text) {
                    push(&mut turns, true, &text, "", at);
                }
            }
            (Some("message"), Some("assistant")) => push(
                &mut turns,
                false,
                &blocks_text(&payload["content"], &["output_text"], "text"),
                "",
                at,
            ),
            (Some("reasoning"), _) => push(
                &mut turns,
                false,
                "",
                &blocks_text(&payload["summary"], &["summary_text"], "text"),
                at,
            ),
            _ => {}
        }
    }
    turns
}

/// DSH：解压后的事件流（user/message、assistant/message，毫秒时间）。
pub(crate) fn dsh_turns(jsonl: &str, since: i64) -> Vec<Turn> {
    let mut turns = Vec::new();
    for line in jsonl.lines() {
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(at) = row["time"].as_i64() else {
            continue;
        };
        if at < since {
            continue;
        }
        match row["type"].as_str() {
            Some("user/message") => {
                let text = blocks_text(&row["data"]["content"], &["text"], "text");
                if !injected(&text) {
                    push(&mut turns, true, &text, "", at);
                }
            }
            Some("assistant/message") => {
                let content = &row["data"]["message"]["content"];
                push(
                    &mut turns,
                    false,
                    &blocks_text(content, &["text"], "text"),
                    &blocks_text(content, &["reasoning"], "text"),
                    at,
                );
            }
            _ => {}
        }
    }
    turns
}

fn find_file(root: &Path, matches: &dyn Fn(&str) -> bool, depth: usize) -> Option<PathBuf> {
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if depth > 0 {
                if let Some(found) = find_file(&path, matches, depth - 1) {
                    return Some(found);
                }
            }
        } else if matches(&name) {
            return Some(path);
        }
    }
    None
}

fn home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

fn hermes_turns(session: &str, since: i64) -> Result<Vec<Turn>> {
    let db = std::env::var_os("AGENT_HUB_HERMES_SESSION_DB")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("LOCALAPPDATA")
                .map(|dir| PathBuf::from(dir).join("hermes").join("state.db"))
        })
        .ok_or("找不到 Hermes 会话库")?;
    let connection =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|_| "Hermes 会话库打不开")?;
    let mut statement = connection
        .prepare("SELECT role,COALESCE(content,''),COALESCE(reasoning,reasoning_content,''),timestamp FROM messages WHERE session_id=?1 AND timestamp>=?2 AND role IN ('user','assistant') ORDER BY id")
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map(rusqlite::params![session, since as f64 / 1000.0], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, f64>(3)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    let mut turns = Vec::new();
    for (role, content, reasoning, at) in rows.flatten() {
        let at = (at * 1000.0) as i64;
        if role == "user" {
            if !injected(&content) {
                push(&mut turns, true, &content, "", at);
            }
        } else {
            push(&mut turns, false, &content, &reasoning, at);
        }
    }
    Ok(turns)
}

/// DSH 的会话文件是多个 zstd 帧拼接；用 Node 自带的 zstd 逐帧解开。
fn dsh_text(file: &Path) -> Result<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let node = crate::dsh::discover().ok_or("找不到 Node 运行时")?;
    let script = r#"const z=require('zlib'),fs=require('fs');const b=fs.readFileSync(process.argv[1]);const m=Buffer.from([0x28,0xb5,0x2f,0xfd]);const at=[];let i=0;while((i=b.indexOf(m,i))!==-1){at.push(i);i+=4}at.push(b.length);let s=at[0],out=[];for(let k=1;k<at.length;k++){try{out.push(z.zstdDecompressSync(b.subarray(s,at[k])).toString());s=at[k]}catch{}}process.stdout.write(out.join('\n'))"#;
    let output = std::process::Command::new(node)
        .args(["-e", script])
        .arg(file)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|_| "DSH 会话解压失败")?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// 读出某条原生会话在 `since`（毫秒）之后新增的对话。
pub(crate) fn since(agent: &str, session: &str, since: i64) -> Result<Vec<Turn>> {
    match agent {
        "claude-win" => {
            let root = std::env::var_os("CLAUDE_CONFIG_DIR")
                .map(PathBuf::from)
                .or_else(|| home().map(|home| home.join(".claude")))
                .ok_or("找不到 Claude 目录")?
                .join("projects");
            let name = format!("{session}.jsonl");
            let file = find_file(&root, &|candidate| candidate == name, 1)
                .ok_or("找不到 Claude 会话文件")?;
            Ok(claude_turns(
                &std::fs::read_to_string(file).map_err(|e| e.to_string())?,
                since,
            ))
        }
        "codex-win" => {
            let root = std::env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .or_else(|| home().map(|home| home.join(".codex")))
                .ok_or("找不到 Codex 目录")?
                .join("sessions");
            let suffix = format!("-{session}.jsonl");
            let file = find_file(
                &root,
                &|candidate| candidate.starts_with("rollout-") && candidate.ends_with(&suffix),
                4,
            )
            .ok_or("找不到 Codex 会话文件")?;
            Ok(codex_turns(
                &std::fs::read_to_string(file).map_err(|e| e.to_string())?,
                since,
            ))
        }
        "hermes-win" => hermes_turns(session, since),
        "dsh-win" => {
            let root = crate::dsh::resolved_dsh_home()
                .ok_or("找不到 DSH 目录")?
                .join("sessions");
            let uuid = session.strip_prefix("session-").unwrap_or(session);
            let directory = std::fs::read_dir(&root)
                .map_err(|_| "找不到 DSH 会话目录")?
                .flatten()
                .map(|entry| entry.path().join(uuid))
                .find(|path| path.is_dir())
                .ok_or("找不到 DSH 会话文件")?;
            let file = ["session.jsonl.zstd", "session.jsonl"]
                .iter()
                .map(|name| directory.join(name))
                .find(|path| path.is_file())
                .ok_or("找不到 DSH 会话文件")?;
            let text = if file.extension().is_some_and(|ext| ext == "zstd") {
                dsh_text(&file)?
            } else {
                std::fs::read_to_string(&file).map_err(|e| e.to_string())?
            };
            Ok(dsh_turns(&text, since))
        }
        _ => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_iso_timestamps() {
        assert_eq!(iso_ms("2026-10-13T15:22:53.014Z"), Some(1_791_904_973_014));
        assert_eq!(iso_ms("1970-01-01T00:00:01Z"), Some(1000));
    }

    #[test]
    fn claude_keeps_only_new_spoken_turns() {
        let jsonl = [
            r#"{"type":"user","timestamp":"2026-10-13T00:00:00.000Z","message":{"content":"旧的"}}"#,
            r#"{"type":"user","timestamp":"2026-10-13T00:00:10.000Z","message":{"content":"暗号改成红狐"}}"#,
            r#"{"type":"user","timestamp":"2026-10-13T00:00:10.500Z","isMeta":true,"message":{"content":"meta"}}"#,
            r#"{"type":"assistant","timestamp":"2026-10-13T00:00:11.000Z","message":{"content":[{"type":"thinking","thinking":"想一下"}]}}"#,
            r#"{"type":"assistant","timestamp":"2026-10-13T00:00:12.000Z","message":{"content":[{"type":"text","text":"改好了"}]}}"#,
            r#"{"type":"user","timestamp":"2026-10-13T00:00:13.000Z","message":{"content":[{"type":"tool_result","content":"x"}]}}"#,
        ]
        .join("\n");
        let since = iso_ms("2026-10-13T00:00:05.000Z").unwrap();
        let turns = claude_turns(&jsonl, since);
        assert_eq!(turns.len(), 2);
        assert!(turns[0].user && turns[0].text == "暗号改成红狐");
        assert_eq!(
            (turns[1].text.as_str(), turns[1].thought.as_str()),
            ("改好了", "想一下")
        );
    }

    #[test]
    fn codex_skips_injected_context_and_merges_reasoning() {
        let jsonl = [
            r#"{"timestamp":"2026-10-13T00:00:10.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>x</environment_context>"}]}}"#,
            r#"{"timestamp":"2026-10-13T00:00:10.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"你好"}]}}"#,
            r#"{"timestamp":"2026-10-13T00:00:11.000Z","type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"想"}]}}"#,
            r#"{"timestamp":"2026-10-13T00:00:12.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"在"}]}}"#,
        ]
        .join("\n");
        let turns = codex_turns(&jsonl, 0);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].text, "你好");
        assert_eq!(
            (turns[1].text.as_str(), turns[1].thought.as_str()),
            ("在", "想")
        );
    }

    #[test]
    fn dsh_reads_events_after_the_handoff() {
        let jsonl = [
            r#"{"type":"user/message","time":5,"data":{"content":[{"type":"text","text":"早先"}]}}"#,
            r#"{"type":"user/message","time":20,"data":{"content":[{"type":"text","text":"继续"}]}}"#,
            r#"{"type":"user/message","time":20,"data":{"content":[{"type":"text","text":"Current runtime context. x"}]}}"#,
            r#"{"type":"assistant/message","time":30,"data":{"message":{"role":"assistant","content":[{"type":"reasoning","text":"嗯"},{"type":"text","text":"好的"}]}}}"#,
        ]
        .join("\n");
        let turns = dsh_turns(&jsonl, 10);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].text, "继续");
        assert_eq!(
            (turns[1].text.as_str(), turns[1].thought.as_str()),
            ("好的", "嗯")
        );
    }
}
