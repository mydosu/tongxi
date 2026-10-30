//! 运行状态事件的瘦身：快照里的模型表几十到上百 KB，流式回复时每个事件都带一份，桌面和手机都白白序列化、传输、解析。
//! 模型表和上一次发出去的一样就不带，前端 adoptRuntime 缺省时沿用上一份；变了照常带上。
//! 前端启动时先调 `<key>_status` 拿到完整快照，之后收到的事件只补变化的部分。
use serde::Serialize;
use serde_json::Value;
use std::collections::{hash_map::DefaultHasher, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, OnceLock};

static SENT: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();

/// 要发的快照（带模型表）→ 事件载荷（模型表没变就去掉）。`event` 区分各个成员，各自记各自上次发的。
pub fn slim<S: Serialize>(event: &str, snapshot: &S) -> Value {
    let mut value = serde_json::to_value(snapshot).unwrap_or(Value::Null);
    if let Ok(mut sent) = SENT.get_or_init(Default::default).lock() {
        drop_unchanged_models(&mut sent, event, &mut value);
    }
    value
}

fn drop_unchanged_models(sent: &mut HashMap<String, u64>, event: &str, value: &mut Value) {
    let Some(object) = value.as_object_mut() else { return };
    let Some(models) = object.get("models") else { return };
    let mut hasher = DefaultHasher::new();
    models.to_string().hash(&mut hasher);
    let hash = hasher.finish();
    if sent.get(event) == Some(&hash) {
        object.remove("models");
    } else {
        sent.insert(event.to_string(), hash);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn models_are_sent_once_until_they_change() {
        let mut sent = HashMap::new();
        let mut snapshot = |models: Value| { let mut value = json!({ "revision": 1, "models": models, "runs": [] }); drop_unchanged_models(&mut sent, "claude-state", &mut value); value };
        assert!(snapshot(json!([{ "id": "a" }])).get("models").is_some(), "第一次要带上");
        let again = snapshot(json!([{ "id": "a" }]));
        assert!(again.get("models").is_none() && again.get("runs").is_some(), "没变就去掉模型表，其余照旧");
        assert!(snapshot(json!([{ "id": "a" }, { "id": "b" }])).get("models").is_some(), "变了要重新带上");
        assert!(snapshot(json!([{ "id": "a" }, { "id": "b" }])).get("models").is_none());
        let mut other = json!({ "models": [{ "id": "a" }, { "id": "b" }] });
        drop_unchanged_models(&mut sent, "codex-state", &mut other);
        assert!(other.get("models").is_some(), "各成员各记各的");
    }
}
