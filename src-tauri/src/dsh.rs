//! DSH 成员：受管安装目录里的 `@deepseek-ai/dsh-acp`，经 `dsh_bridge.mjs` 接入共用的 ACP 运行时（hermes.rs）。
//! 这里只留 DSH 自己的东西：配置项解析、会话日志里的用量、会话标题、权限表。
use crate::models::ModelOption;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

type Result<T> = std::result::Result<T, String>;

/// 状态身份：和别的 ACP 成员共用运行时类型，按这层包装区分；方法经 Deref 直接用。
pub struct Runtime(Arc<crate::hermes::Runtime>);
impl Runtime {
    pub fn new(app: tauri::AppHandle, store: Arc<Mutex<crate::store::Store>>, directory: PathBuf) -> Arc<Self> {
        Arc::new(Self(crate::hermes::Runtime::with_target(app, store, directory, crate::acp_transport::Target::Dsh)))
    }
}
impl std::ops::Deref for Runtime {
    type Target = Arc<crate::hermes::Runtime>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub(crate) fn current_option(response: &Value, id: &str) -> Option<String> {
    response["configOptions"]
        .as_array()?
        .iter()
        .find(|option| option["id"] == id)?["currentValue"]
        .as_str()
        .map(String::from)
}

pub(crate) fn models_from_response(response: &Value) -> Vec<ModelOption> {
    let Some(options) = response["configOptions"].as_array() else {
        return vec![];
    };
    let Some(model) = options.iter().find(|option| option["id"] == "model") else {
        return vec![];
    };
    let mut choices = vec![];
    fn collect(
        value: &Value,
        provider_id: Option<&str>,
        provider_name: Option<&str>,
        result: &mut Vec<ModelOption>,
    ) {
        if let Some(children) = value.as_array() {
            for child in children {
                collect(child, provider_id, provider_name, result);
            }
        } else if let Some(id) = value["value"].as_str() {
            let route_provider = serde_json::from_str::<Value>(id).ok().and_then(|route| {
                route
                    .as_array()
                    .and_then(|parts| parts.first())
                    .and_then(Value::as_str)
                    .map(String::from)
            });
            let provider_id = if provider_id == Some("provider") {
                route_provider.as_deref().or(provider_id)
            } else {
                provider_id
            };
            result.push(ModelOption {
                id: id.into(),
                name: value["name"].as_str().unwrap_or(id).into(),
                provider_id: provider_id.map(String::from),
                provider_name: provider_name.map(String::from),
                efforts: vec![],
                default_effort: None,
            });
        } else if value.is_object() {
            let group_id = value["group"].as_str().or(provider_id);
            let group_name = value["name"].as_str().or(provider_name);
            collect(&value["options"], group_id, group_name, result);
        }
    }
    collect(&model["options"], None, None, &mut choices);
    if let Some(selected) = choices
        .iter_mut()
        .find(|choice| Some(choice.id.as_str()) == model["currentValue"].as_str())
    {
        if let Some(effort) = options
            .iter()
            .find(|option| option["id"] == "reasoning_effort")
        {
            selected.efforts = effort["options"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|entry| {
                    entry["value"]
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .map(String::from)
                })
                .collect();
            selected.default_effort = effort["currentValue"]
                .as_str()
                .filter(|value| !value.is_empty())
                .map(String::from);
        }
    }
    choices
}
/// DSH 家目录：与桥侧解析一致——外层设了 AGENT_HUB_DSH_HOME 就用（测试隔离），
/// 否则用真实 ~/.dsh（必须已存在）；都没有返回 None，调用方回落到同席隔离目录。
pub(crate) fn resolved_dsh_home() -> Option<PathBuf> {
    std::env::var_os("AGENT_HUB_DSH_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .map(|value| PathBuf::from(value).join(".dsh"))
                .filter(|path| path.is_dir())
        })
}

/// DSH 的 ACP 回包不带用量，只报上下文占用；但它的会话日志里每条 `assistant/message` 都记着这次模型调用的
/// 真实用量（未命中缓存的输入、缓存读写、输出）。把 since（毫秒）之后的加起来，就是这一回合的用量，
/// 按 ACP 回包的样子返回，和别家一样交给 record_usage。
/// 日志在 `<DSH 家目录>/sessions/<目录>/<会话>/session.v4.jsonl[.zstd]`，zstd 版是一帧帧追加的。
/// 回合结束后 DSH 要过一两秒才把最后一次调用落盘（实测），所以等这一回合的 `turn/end` 写进去（最多 10 秒）再算，
/// 不然短回合读不到、长回合少算最后一步。会阻塞，放后台调。
pub(crate) fn session_usage(session_id: &str, since: i64) -> Option<Value> {
    let sessions = resolved_dsh_home()?.join("sessions");
    let folder = std::fs::read_dir(&sessions)
        .ok()?
        .flatten()
        .map(|entry| entry.path().join(session_id))
        .find(|path| path.is_dir())?;
    let read = || match std::fs::read(folder.join("session.v4.jsonl.zstd")) {
        Ok(bytes) => Some(decode_frames(&bytes)),
        Err(_) => std::fs::read_to_string(folder.join("session.v4.jsonl")).ok(),
    };
    let mut text = read()?;
    for _ in 0..50 {
        if turn_ended(&text, since) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
        text = read()?;
    }
    usage_since(&text, since)
}

fn turn_ended(text: &str, since: i64) -> bool {
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .any(|event| event["type"] == "turn/end" && event["time"].as_i64().is_some_and(|time| time >= since))
}

/// 解开一帧帧追加的 zstd 数据；坏帧跳过。
fn decode_frames(bytes: &[u8]) -> String {
    use std::io::Read;
    const MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];
    let starts: Vec<usize> = bytes
        .windows(4)
        .enumerate()
        .filter(|(_, window)| *window == MAGIC)
        .map(|(index, _)| index)
        .collect();
    let mut text = String::new();
    for (index, start) in starts.iter().enumerate() {
        let end = starts.get(index + 1).copied().unwrap_or(bytes.len());
        let Ok(mut decoder) = ruzstd::decoding::StreamingDecoder::new(&bytes[*start..end]) else {
            continue;
        };
        let mut frame = Vec::new();
        if decoder.read_to_end(&mut frame).is_ok() {
            text.push_str(&String::from_utf8_lossy(&frame));
        }
    }
    text
}

fn usage_since(text: &str, since: i64) -> Option<Value> {
    let (mut input, mut read, mut write, mut output, mut context) = (0, 0, 0, 0, 0);
    let mut found = false;
    for event in text.lines().filter_map(|line| serde_json::from_str::<Value>(line).ok()) {
        if event["type"] != "assistant/message" || event["time"].as_i64().is_none_or(|time| time < since) {
            continue;
        }
        let item = &event["data"]["usage"];
        if item.is_null() {
            continue;
        }
        let n = |key: &str| item[key].as_u64().unwrap_or(0);
        found = true;
        read += n("cacheReadTokens");
        write += n("cacheWriteTokens");
        input += n("inputTokens") + n("cacheReadTokens") + n("cacheWriteTokens");
        output += n("outputTokens");
        // 最后一次调用的总量就是这时的上下文大小。
        context = n("totalTokens");
    }
    found.then(|| {
        json!({"usage": {"inputTokens": input, "cachedReadTokens": read, "cachedWriteTokens": write, "outputTokens": output},
               "_meta": {"contextTokens": context}})
    })
}

/// 从 DSH 的会话投影缓存里取会话标题：优先会话已定下来的标题，
/// 没有就用会话里第一句用户输入；都没有返回 None（不编、不猜）。
/// 会话 id 两种写法（`session-<uuid>` / 裸 `<uuid>`）都指向同一个 `session-<uuid>.json`。
fn projected_title(home: &Path, session_id: &str) -> Option<String> {
    let uuid = session_id.strip_prefix("session-").unwrap_or(session_id);
    if uuid.is_empty() || uuid.contains('/') || uuid.contains('\\') {
        return None;
    }
    let path = home
        .join("storages")
        .join("session_projcache")
        .join("sessions")
        .join(format!("session-{uuid}.json"));
    let bytes = std::fs::read(path).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    let rows = &value["record"]["rows"];
    let title = rows["title"]["val"].as_str().map(str::trim);
    if let Some(title) = title.filter(|title| !title.is_empty()) {
        return Some(title.to_owned());
    }
    let first = rows["titleInput"]["val"]["first"]["text"].as_str()?.trim();
    if first.is_empty() {
        None
    } else {
        Some(first.to_owned())
    }
}

pub(crate) fn discover() -> Option<PathBuf> {
    let path = std::env::var_os("AGENT_HUB_DSH_NODE")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::split_paths(&std::env::var_os("PATH")?)
                .map(|directory| directory.join("node.exe"))
                .find(|path| path.is_file())
        })?;
    (path.is_absolute()
        && path.is_file()
        && path
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("node.exe")))
    .then_some(path)
}

/// 面板标题：尽量用 DSH 自己的投影缓存补上（已定标题 > 首句用户输入），读不到就保持原样，绝不编造。
/// 隔离模式下 home 是隔离目录，不碰真实 ~/.dsh。
pub(crate) fn fill_titles(directory: &Path, sessions: &mut [crate::codex::NativeSession]) {
    let home = resolved_dsh_home().unwrap_or_else(|| directory.join("dsh-native"));
    for session in sessions {
        if session.title.as_deref().is_none_or(|title| title.trim().is_empty()) {
            session.title = projected_title(&home, &session.id);
        }
    }
}

/// 桥按 dsh-native/permissions.json 收紧会话的沙箱：只读、只改工作树档写进去，全权和私聊不写（照旧不限）。
pub(crate) fn permit(directory: &Path, session_id: &str, access: Option<(&str, Option<String>)>) -> Result<()> {
    static WRITING: Mutex<()> = Mutex::new(());
    let _guard = WRITING.lock().unwrap();
    let table = directory.join("dsh-native").join("permissions.json");
    let mut rules: serde_json::Map<String, Value> =
        std::fs::read_to_string(&table).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default();
    let rule = match access {
        Some(("read", _)) => Some(json!({"mode":"read-only"})),
        Some(("write", worktree)) => Some(json!({"mode":"workspace-write","root":worktree})),
        _ => None,
    };
    if rules.get(session_id) == rule.as_ref() {
        return Ok(());
    }
    match rule {
        Some(rule) => rules.insert(session_id.into(), rule),
        None => rules.remove(session_id),
    };
    std::fs::write(&table, Value::Object(rules).to_string()).map_err(|_| "DSH 权限档写入失败".into())
}

#[cfg(test)]
mod session_usage_tests {
    #[test]
    fn sums_assistant_usage_after_the_turn_started() {
        let log = [
            r#"{"type":"session","version":4,"id":"s"}"#,
            r#"{"type":"assistant/message","time":100,"data":{"usage":{"inputTokens":10184,"outputTokens":433,"totalTokens":10617}}}"#,
            r#"{"type":"tool/result","time":150,"data":{}}"#,
            r#"{"type":"assistant/message","time":200,"data":{"usage":{"inputTokens":713,"outputTokens":179,"totalTokens":11004,"cacheReadTokens":10112}}}"#,
            r#"{"type":"assistant/message","time":300,"data":{"usage":{"inputTokens":50,"outputTokens":20,"totalTokens":11100,"cacheReadTokens":11000,"cacheWriteTokens":30}}}"#,
        ]
        .join("
");
        let usage = super::usage_since(&log, 150).unwrap();
        // 只算这一回合（time ≥ 150）的两次调用；input 含缓存读写。
        assert_eq!(usage["usage"]["inputTokens"], 713 + 10112 + 50 + 11000 + 30);
        assert_eq!(usage["usage"]["cachedReadTokens"], 21112);
        assert_eq!(usage["usage"]["cachedWriteTokens"], 30);
        assert_eq!(usage["usage"]["outputTokens"], 199);
        assert_eq!(usage["_meta"]["contextTokens"], 11100);
        assert!(super::usage_since(&log, 1000).is_none());
        assert!(!super::turn_ended(&log, 150));
        assert!(super::turn_ended(&format!("{log}
{}", r#"{"type":"turn/end","time":310}"#), 150));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `projected_title` 的取值顺序：已定标题 > 会话说第一句 > None；带路径分隔的 id 一律拒绝。
    #[test]
    fn projected_title_prefers_stored_title_then_first_input() {
        let root = std::env::temp_dir().join(format!("dsh-projcache-{}", uuid::Uuid::new_v4()));
        let dir = root
            .join("storages")
            .join("session_projcache")
            .join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        let write = |id: &str, body: &str| {
            std::fs::write(dir.join(format!("session-{id}.json")), body).unwrap()
        };
        write("set-1", r#"{"record":{"rows":{"title":{"val":"标题一"}}}}"#);
        write(
            "fallback-2",
            r#"{"record":{"rows":{"title":{"val":null},"titleInput":{"val":{"first":{"text":"只回两个字：在的。"}}}}}}"#,
        );
        write(
            "blank-3",
            r#"{"record":{"rows":{"title":{"val":"   "},"titleInput":{"val":{"first":{"text":"第一句"}}}}}}"#,
        );
        assert_eq!(
            projected_title(&root, "session-set-1").as_deref(),
            Some("标题一")
        );
        assert_eq!(
            projected_title(&root, "fallback-2").as_deref(),
            Some("只回两个字：在的。")
        );
        assert_eq!(projected_title(&root, "blank-3").as_deref(), Some("第一句"));
        assert_eq!(projected_title(&root, "missing"), None);
        assert_eq!(projected_title(&root, "a/b"), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn native_catalog_keeps_opaque_routes_and_model_dependent_efforts() {
        let response = json!({"configOptions":[
            {"id":"model","currentValue":"[\"provider\",\"flash\"]","options":[{"group":"provider","options":[{"value":"[\"provider\",\"flash\"]","name":"Flash"},{"value":"[\"provider\",\"pro\"]","name":"Pro"},{"value":"[\"provider-two\",\"flash\"]","name":"Other Flash"}]},{"group":"provider","options":[{"value":"[\"provider-two\",\"pro\"]","name":"Other Pro"}]}]},
            {"id":"reasoning_effort","currentValue":"high","options":[{"value":"off"},{"value":"low"},{"value":"high"}]}
        ]});
        let models = models_from_response(&response);
        assert_eq!(models.len(), 4);
        assert_eq!(models[0].provider_id.as_deref(), Some("provider"));
        assert_eq!(models[2].provider_id.as_deref(), Some("provider-two"));
        assert_eq!(models[0].efforts, ["off", "low", "high"]);
        assert!(models[1].efforts.is_empty());
        assert_eq!(models[0].default_effort.as_deref(), Some("high"));
        assert_eq!(
            current_option(&response, "model").as_deref(),
            Some("[\"provider\",\"flash\"]")
        );
    }

    #[test]
    fn bridge_uses_only_dsh_own_models_and_keys() {
        let bridge = include_str!("dsh_bridge.mjs");
        // 模型、Key 只来自 DSH 自己的配置，不读 Hermes，也不替服务商设输出上限。
        assert!(!bridge.to_lowercase().contains("hermes"));
        assert!(!bridge.contains("apiKey ="));
        assert!(!bridge.contains("maxTokens: 16384"));
        assert!(!bridge.contains("max_tokens: 16384"));
    }
}
