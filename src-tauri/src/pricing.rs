//! 各家模型的计费规则（API 标价，附来源与核对日期），供调度按任务权衡模型。
//! 这里只放价格事实，不规定哪类任务用哪个模型——由调度结合任务、上下文大小和用户倾向自己判断。
use crate::models::ModelOption;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::OnceLock;

#[derive(Deserialize)]
struct Table {
    checked: String,
    note: String,
    models: Vec<Entry>,
    #[serde(default)]
    peaks: std::collections::HashMap<String, Peak>,
}
#[derive(Deserialize)]
struct Entry {
    #[serde(rename = "match")]
    patterns: Vec<String>,
    name: String,
    input: f64,
    cached: f64,
    output: f64,
    #[serde(default)]
    long_context: Option<Value>,
    #[serde(default)]
    peak: Option<String>,
}
#[derive(Deserialize)]
struct Peak {
    note: String,
    weekdays: bool,
    hours_utc: Vec<[u32; 2]>,
}

fn table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(|| serde_json::from_str(include_str!("pricing.json")).expect("pricing.json"))
}

fn normalized(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// 型号片段要完整命中：「gpt5」不能命中「gpt56sol」，「sonnet5」不能命中「sonnet55」。
fn names(text: &str, pattern: &str) -> bool {
    text.match_indices(pattern).any(|(at, _)| {
        !text[at + pattern.len()..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
    })
}

fn entry(model: &ModelOption) -> Option<&'static Entry> {
    let text = normalized(&format!("{} {}", model.id, model.name));
    table()
        .models
        .iter()
        .find(|entry| entry.patterns.iter().any(|pattern| names(&text, pattern)))
}

/// 现在是否处于某条线路的高峰时段（按 UTC 算；节假日不计）。
fn peak_now(name: &str, unix: u64) -> bool {
    let Some(peak) = table().peaks.get(name) else {
        return false;
    };
    let days = unix / 86_400;
    // 1970-01-01 是星期四：0=周日 … 6=周六。
    let weekday = (days + 4) % 7;
    if peak.weekdays && (weekday == 0 || weekday == 6) {
        return false;
    }
    let hour = (unix % 86_400 / 3_600) as u32;
    peak.hours_utc.iter().any(|[from, to]| hour >= *from && hour < *to)
}

/// 调度看到的一条价格：命中价格表才有。
pub fn price(model: &ModelOption) -> Option<Value> {
    let entry = entry(model)?;
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let mut value = json!({"name":entry.name,"input":entry.input,"cached":entry.cached,"output":entry.output});
    if let Some(long) = &entry.long_context {
        value["long_context"] = long.clone();
    }
    if let Some(peak) = &entry.peak {
        value["peak"] = json!(peak_now(peak, unix));
        if let Some(rule) = table().peaks.get(peak) {
            value["peak_rule"] = json!(rule.note);
        }
    }
    Some(value)
}

/// 价格表的说明与核对日期。
pub fn about() -> Value {
    json!({"checked":table().checked,"note":table().note})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, name: &str) -> ModelOption {
        ModelOption {
            id: id.into(),
            name: name.into(),
            provider_id: None,
            provider_name: None,
            efforts: vec![],
            default_effort: None,
        }
    }

    #[test]
    fn catalog_names_find_their_price_without_prefix_collisions() {
        let haiku = price(&model("haiku", "Haiku 5.5")).unwrap();
        assert_eq!(haiku["name"], "Claude Haiku 5.5");
        assert_eq!(haiku["long_context"]["above"], 100000);
        assert_eq!(price(&model("claude-haiku-4-5-20251001", "Haiku 4.5")).unwrap()["name"], "Claude Haiku 4.5");
        assert_eq!(price(&model("sonnet", "Sonnet 5.5")).unwrap()["name"], "Claude Sonnet 5.5");
        assert_eq!(price(&model("claude-sonnet-5", "Sonnet 5")).unwrap()["name"], "Claude Sonnet 5");
        assert_eq!(price(&model("default", "Default (recommended) · Opus 5.5")).unwrap()["name"], "Claude Opus 5.5");
        assert_eq!(price(&model("gpt-5.6-sol", "GPT-5.6 Sol")).unwrap()["name"], "GPT-5.6 Sol");
        assert_eq!(price(&model("gpt-5.4-mini", "gpt-5.4-mini")).unwrap()["name"], "GPT-5.4 mini");
        assert_eq!(price(&model("gpt-5.3-codex", "gpt-5.3-codex")).unwrap()["name"], "GPT-5.2 / GPT-5.3-Codex");
        assert_eq!(
            price(&model("[\"deepseek-official\",\"deepseek-v4-flash\"]", "DeepSeek · deepseek-v4-flash")).unwrap()["name"],
            "DeepSeek V4.1 Flash"
        );
        assert!(price(&model("gpt-5.9", "GPT-5.9")).is_none(), "没收录的新型号不能套用旧价");
    }

    #[test]
    fn deepseek_peak_hours_follow_utc_weekdays() {
        // 2026-10-15 是星期四。
        let thursday = 1_792_022_400; // 2026-10-15 00:00 UTC
        assert!(!peak_now("deepseek", thursday));
        assert!(peak_now("deepseek", thursday + 2 * 3_600));
        assert!(peak_now("deepseek", thursday + 7 * 3_600));
        assert!(!peak_now("deepseek", thursday + 12 * 3_600));
        assert!(!peak_now("deepseek", thursday + 2 * 86_400 + 2 * 3_600), "周六不算高峰");
    }
}
