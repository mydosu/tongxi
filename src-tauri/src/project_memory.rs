use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// OpenViking 的 Python：先看 AGENT_HUB_OPENVIKING_PYTHON，再找 PATH 上 `ov` / `openviking` 旁边的 python.exe。
fn openviking_python() -> Option<PathBuf> {
    std::env::var_os("AGENT_HUB_OPENVIKING_PYTHON").map(PathBuf::from).filter(|path| path.is_file()).or_else(|| {
        crate::service_install::on_path_find(&["ov.exe", "openviking.exe"], |shell| {
            Some(shell.parent()?.join("python.exe")).filter(|python| python.is_file())
        })
    })
}

pub fn uses_openviking() -> bool {
    openviking_python().is_some()
}

/// 装了 OpenViking 就用它；没装就退回项目里 `.tongxi/记忆/` 下的 markdown。
/// 装了但连不上照样报错，不悄悄换地方存，免得记忆分成两半。
pub fn call(project: &str, root: &Path, agent: &str, tool: &str, args: Value) -> Result<Value, String> {
    match openviking_python() {
        Some(python) => openviking(&python, project, agent, tool, args),
        None => local(&root.join(".tongxi").join("记忆"), agent, tool, &args),
    }
}

fn openviking(python: &Path, project: &str, agent: &str, tool: &str, args: Value) -> Result<Value, String> {
    let mut command = Command::new(python);
    command.env("PYTHONIOENCODING", "utf-8");
    // 不继承启动同席那个环境里的 OpenViking 身份（例如某个 agent 终端注入的 OPENVIKING_API_KEY），
    // 否则记忆会悄悄写成那个 agent；身份只来自下面指定的配置文件。
    for (name, _) in std::env::vars_os() {
        if name
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("OPENVIKING_")
        {
            command.env_remove(name);
        }
    }
    // 同席的记忆工具没有 harness 身份：用中立 tooling 身份的专属配置，不依赖共享 ovcli.conf 的顶层 key
    // （那个顶层 key 会抢在各 harness 自己的 key 前面，留空才能保证各 agent 不被解析成别人）。
    if let Some(config) = std::env::var_os("AGENT_HUB_OPENVIKING_CONFIG")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .map(|home| {
                    PathBuf::from(home)
                        .join(".openviking")
                        .join("ovcli.tooling.conf")
                })
                .filter(|path| path.is_file())
        })
    {
        command.env("OPENVIKING_CLI_CONFIG_FILE", config);
    }
    command
        .args(["-u", "-c", include_str!("project_memory.py")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command.spawn().map_err(|_| "项目记忆客户端启动失败")?;
    let _scope = crate::process_scope::ProcessScope::attach(&child)?;
    let payload = json!({"project":project,"agent":agent,"tool":tool,"args":args});
    child
        .stdin
        .take()
        .ok_or("项目记忆输入不可用")?
        .write_all(payload.to_string().as_bytes())
        .map_err(|_| "项目记忆请求失败")?;
    let output = child
        .wait_with_output()
        .map_err(|_| "项目记忆客户端未完成")?;
    let reply: Value = serde_json::from_slice(&output.stdout).map_err(|_| "项目记忆响应无效")?;
    if let Some(error) = reply["error"].as_str() {
        return Err(error.into());
    }
    Ok(reply["result"].clone())
}

// ---- 没装 OpenViking 时：项目里的 markdown，规则和 project_memory.py 一致 ----

const SHARED: &str = "共享";
const PRIVATE: &str = "私有";

fn note_name(value: Option<&str>) -> Result<String, String> {
    let refuse = || "笔记名只能用字母、数字、中文、下划线、点和连字符（1-80 字），不要带 .md".to_string();
    let name = value.ok_or_else(refuse)?;
    let fine = (1..=80).contains(&name.chars().count())
        && !name.starts_with('.')
        && !name.ends_with(".md")
        && name.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '-'));
    fine.then(|| name.to_owned()).ok_or_else(refuse)
}

/// `prefix` 后面紧跟至少 `min` 个 `allowed` 字符。
fn token(text: &str, prefix: &str, min: usize, allowed: fn(char) -> bool) -> bool {
    text.match_indices(prefix).any(|(at, _)| text[at + prefix.len()..].chars().take_while(|c| allowed(*c)).count() >= min)
}

fn secret(text: &str) -> bool {
    (text.contains("-----BEGIN ") && text.contains("PRIVATE KEY-----"))
        || token(text, "sk-", 20, |c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        || token(text, "ghp_", 20, |c| c.is_ascii_alphanumeric())
        || token(text, "AKIA", 16, |c| c.is_ascii_digit() || c.is_ascii_uppercase())
}

fn body(agent: &str, name: &str, scope: &str, content: Option<&str>, shared_from: Option<&str>) -> Result<String, String> {
    let content = content
        .filter(|text| !text.trim().is_empty() && text.chars().count() <= 16000)
        .ok_or("笔记内容不能为空，且不超过 16000 字")?;
    if secret(content) {
        return Err("笔记内容疑似包含凭据，已拒绝保存".into());
    }
    let from = shared_from.map(|uri| format!("\nShared-From: {uri}")).unwrap_or_default();
    Ok(format!("# {name}\n\nAuthor: {agent}\nScope: {scope}{from}\n\n{content}"))
}

fn author_of(text: &str) -> Option<&str> {
    text.lines().find_map(|line| line.strip_prefix("Author: ")).map(str::trim)
}

/// 去掉 `# 标题`、空行、`Key: value` 头和其后的空行，只留正文。
fn content_of(text: &str) -> &str {
    let Some((_, mut rest)) = text.strip_prefix("# ").and_then(|rest| rest.split_once("\n\n")) else {
        return text;
    };
    while let Some((line, next)) = rest.split_once('\n') {
        if line.is_empty() {
            return next;
        }
        let header = line
            .split_once(": ")
            .is_some_and(|(key, _)| !key.is_empty() && key.chars().all(|c| c.is_ascii_alphabetic() || c == '-'));
        if !header {
            return text;
        }
        rest = next;
    }
    text
}

fn local(memory: &Path, agent: &str, tool: &str, args: &Value) -> Result<Value, String> {
    let shared = format!(".tongxi/记忆/{SHARED}/");
    let private = format!(".tongxi/记忆/{PRIVATE}/{agent}/");
    let file = |uri: &str| memory.join(uri.trim_start_matches(".tongxi/记忆/"));
    // 名字校验挡掉了 `/` 和开头的 `.`，所以拼出来的路径出不了这两个目录。
    let readable = |uri: &str| {
        [&shared, &private].iter().any(|prefix| {
            uri.strip_prefix(prefix.as_str())
                .and_then(|rest| rest.strip_suffix(".md"))
                .is_some_and(|name| note_name(Some(name)).is_ok())
        })
    };
    let scopes = |value: Option<&str>| -> Result<Vec<&String>, String> {
        Ok(match value.unwrap_or("all") {
            "all" => vec![&shared, &private],
            "shared" => vec![&shared],
            "private" => vec![&private],
            _ => return Err("scope 只能是 all、shared 或 private".into()),
        })
    };
    let notes = |prefixes: &[&String]| -> Vec<(String, String)> {
        let mut found = vec![];
        for prefix in prefixes {
            for entry in std::fs::read_dir(file(prefix)).into_iter().flatten().flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.ends_with(".md") {
                    found.push((format!("{prefix}{name}"), std::fs::read_to_string(entry.path()).unwrap_or_default()));
                }
            }
        }
        found.sort();
        found
    };
    let scope_of = |uri: &str| if uri.starts_with(&shared) { "shared" } else { "private" };
    let write = |uri: &str, text: &str| -> Result<(), String> {
        let path = file(uri);
        std::fs::create_dir_all(path.parent().unwrap()).map_err(|_| "项目记忆目录创建失败")?;
        std::fs::write(path, text).map_err(|_| "项目记忆写入失败".to_string())
    };
    let put_shared = |name: &str, text: &str| -> Result<String, String> {
        let uri = format!("{shared}{name}.md");
        if let Ok(current) = std::fs::read_to_string(file(&uri)) {
            if author_of(&current) != Some(agent) {
                return Err(format!(
                    "共享区已有 {} 写的同名笔记，请换一个 name；共享笔记只由作者本人修改",
                    author_of(&current).unwrap_or("其他成员")
                ));
            }
        }
        write(&uri, text)?;
        Ok(uri)
    };
    let text = |key: &str| args[key].as_str();
    match tool {
        "hub_memory_write" => {
            let name = note_name(text("name"))?;
            let scope = text("scope").unwrap_or("private");
            if !matches!(scope, "private" | "shared") {
                return Err("scope 只能是 private 或 shared".into());
            }
            let content = body(agent, &name, scope, text("content"), None)?;
            let uri = if scope == "shared" {
                put_shared(&name, &content)?
            } else {
                let uri = format!("{private}{name}.md");
                write(&uri, &content)?;
                uri
            };
            Ok(json!({"uri":uri,"scope":scope,"stored":true}))
        }
        "hub_memory_read" => {
            let uri = text("uri").filter(|uri| readable(uri)).ok_or("只能读取本项目的共享笔记或你自己的私有笔记")?;
            let content = std::fs::read_to_string(file(uri)).map_err(|_| "没有这条笔记")?;
            Ok(json!({"uri":uri,"content":content}))
        }
        "hub_memory_search" => {
            let query = text("query").map(str::trim).filter(|query| !query.is_empty()).ok_or("请给出检索内容")?.to_lowercase();
            let terms: Vec<&str> = query.split_whitespace().collect();
            let mut scored: Vec<(usize, String, String)> = notes(&scopes(text("scope"))?)
                .into_iter()
                .filter_map(|(uri, content)| {
                    let haystack = format!("{uri}\n{content}").to_lowercase();
                    let score: usize = terms.iter().map(|term| haystack.matches(term).count()).sum();
                    (score > 0).then(|| (score, uri, content_of(&content).chars().take(240).collect()))
                })
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            let found: Vec<Value> = scored
                .into_iter()
                .take(8)
                .map(|(_, uri, excerpt)| json!({"uri":uri,"scope":scope_of(&uri),"abstract":excerpt}))
                .collect();
            Ok(json!({"shared_uri":shared,"private_uri":private,"notes":found,"backend":"markdown"}))
        }
        "hub_memory_list" => {
            let entries: Vec<Value> = notes(&scopes(text("scope"))?)
                .into_iter()
                .take(200)
                .map(|(uri, _)| json!({"uri":uri,"scope":scope_of(&uri)}))
                .collect();
            Ok(json!({"shared_uri":shared,"private_uri":private,"notes":entries,"backend":"markdown"}))
        }
        "hub_memory_share" => {
            let source = text("uri")
                .filter(|uri| readable(uri) && uri.starts_with(&private))
                .ok_or("只能分享你自己的私有笔记（private 区的 URI）")?;
            let original = std::fs::read_to_string(file(source)).map_err(|_| "没有这条笔记")?;
            let fallback = source.rsplit('/').next().unwrap_or("").trim_end_matches(".md");
            let name = note_name(text("name").or(Some(fallback)))?;
            let mut content = content_of(&original).to_owned();
            if let Some(note) = text("note").map(str::trim).filter(|note| !note.is_empty()) {
                content = format!("{note}\n\n{content}");
            }
            let uri = put_shared(&name, &body(agent, &name, "shared", Some(&content), Some(source))?)?;
            Ok(json!({"uri":uri,"shared_from":source,"stored":true,"private_kept":true}))
        }
        _ => Err("不支持的项目记忆工具".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::local;
    use serde_json::json;

    #[test]
    fn markdown_memory_keeps_shared_and_private_rules() {
        let root = std::env::temp_dir().join(format!("tongxi-memory-{}", uuid::Uuid::new_v4()));
        let call = |agent: &str, tool: &str, args: serde_json::Value| local(&root, agent, tool, &args);
        let mine = call("codex-win", "hub_memory_write", json!({"name":"构建坑","content":"Windows 上 Python 写回会变 CRLF"})).unwrap();
        let uri = mine["uri"].as_str().unwrap();
        assert_eq!(uri, ".tongxi/记忆/私有/codex-win/构建坑.md");
        assert!(call("claude-win", "hub_memory_read", json!({"uri":uri})).is_err(), "别人的私有笔记读不到");
        assert!(call("claude-win", "hub_memory_search", json!({"query":"CRLF"})).unwrap()["notes"].as_array().unwrap().is_empty());
        let shared = call("codex-win", "hub_memory_share", json!({"uri":uri,"note":"大家注意"})).unwrap();
        let read = call("claude-win", "hub_memory_read", json!({"uri":shared["uri"]})).unwrap();
        let content = read["content"].as_str().unwrap();
        assert!(content.contains("Author: codex-win") && content.contains("大家注意\n\nWindows") && !content.contains("Scope: private"), "{content}");
        let found = call("claude-win", "hub_memory_search", json!({"query":"crlf python"})).unwrap();
        assert_eq!(found["notes"][0]["scope"], "shared");
        let taken = call("claude-win", "hub_memory_write", json!({"name":"构建坑","content":"覆盖","scope":"shared"}));
        assert!(taken.unwrap_err().contains("codex-win"), "共享笔记只由作者改");
        assert!(call("codex-win", "hub_memory_write", json!({"name":"../逃出去","content":"x"})).is_err());
        assert!(call("codex-win", "hub_memory_read", json!({"uri":".tongxi/记忆/共享/../私有/codex-win/构建坑.md"})).is_err());
        let key = call("codex-win", "hub_memory_write", json!({"name":"key","content":"sk-abcdefghijklmnopqrstuvwxyz"}));
        assert!(key.unwrap_err().contains("凭据"));
        assert_eq!(call("codex-win", "hub_memory_list", json!({"scope":"all"})).unwrap()["notes"].as_array().unwrap().len(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }
}

/// 本机实连：`cargo test -- --ignored memory_backend`。设了 AGENT_HUB_OPENVIKING_PYTHON 走 OpenViking，没设退回 markdown。
#[cfg(test)]
#[test]
#[ignore]
fn memory_backend_answers_a_listing() {
    let root = std::env::temp_dir().join(format!("tongxi-backend-{}", uuid::Uuid::new_v4()));
    let listed = call("tongxi-selftest", &root, "codex-win", "hub_memory_list", json!({})).unwrap();
    println!("python={:?} backend={}", openviking_python(), listed["backend"]);
    assert_eq!(listed["backend"] == "markdown", openviking_python().is_none());
}
