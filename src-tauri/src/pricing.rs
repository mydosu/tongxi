//! 各家模型的 API 标价（分段计价、闲时价），用来把每回合的用量折成钱：订阅线路也按 API 价折算，
//! 好让用户看到分活省了多少。
//!
//! 来源分两层：
//! - 自动更新：每天从 LiteLLM 社区维护的价格表（GitHub 高星项目）拉一次，抽出需要的字段缓存到本地；
//! - 内置表 `pricing.json`：人工按官方页面核对，联网拉不到时兜底。
//! 两边都命中时取型号片段更长（更具体）的一条，一样长就用自动更新的。
use crate::models::ModelOption;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::Duration;

const SOURCES: [&str; 2] = [
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json",
    "https://cdn.jsdelivr.net/gh/BerriAI/litellm@main/model_prices_and_context_window.json",
];
const CACHE: &str = "pricing-remote.json";
const REFRESH_SECS: u64 = 24 * 3_600;
/// 直连各家官方 API 的条目优先；Azure 的同名条目只用来补官方条目还没收录的新型号。
const PROVIDERS: [&str; 9] = ["openai", "anthropic", "deepseek", "gemini", "xai", "mistral", "dashscope", "moonshot", "zai"];
const FALLBACK_PROVIDERS: [&str; 1] = ["azure"];

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Tier {
    above: u64,
    input: f64,
    cached: f64,
    output: f64,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Window {
    /// ISO 星期：1=周一 … 7=周日。
    weekdays: Vec<u32>,
    /// UTC 时段，如 "10:00-00:00"（结束 00:00 表示到当天结束；"00:00-00:00" 表示全天）。
    hours_utc: Vec<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct OffPeak {
    input: f64,
    cached: f64,
    output: f64,
    windows: Vec<Window>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Entry {
    #[serde(rename = "match")]
    patterns: Vec<String>,
    name: String,
    input: f64,
    cached: f64,
    output: f64,
    /// 写缓存的单价；没有就按输入价算。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_write: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    long_context: Option<Tier>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    off_peak: Option<OffPeak>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_output: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reasoning: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    vision: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<String>,
}

#[derive(Deserialize)]
struct Bundled {
    checked: String,
    models: Vec<Entry>,
}
#[derive(Clone, Deserialize, Serialize)]
struct Remote {
    fetched_at: u64,
    #[serde(default)]
    etag: Option<String>,
    source: String,
    models: Vec<Entry>,
}

fn bundled() -> &'static Bundled {
    static TABLE: OnceLock<Bundled> = OnceLock::new();
    TABLE.get_or_init(|| serde_json::from_str(include_str!("pricing.json")).expect("pricing.json"))
}
fn remote() -> &'static RwLock<Option<Remote>> {
    static REMOTE: RwLock<Option<Remote>> = RwLock::new(None);
    &REMOTE
}
fn directory() -> &'static OnceLock<PathBuf> {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    &DIR
}
fn last_error() -> &'static Mutex<Option<String>> {
    static ERROR: Mutex<Option<String>> = Mutex::new(None);
    &ERROR
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn normalized(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// 型号片段要完整命中：「gpt5」不能命中「gpt56sol」，「sonnet5」不能命中「sonnet55」。
fn names(text: &str, pattern: &str) -> bool {
    !pattern.is_empty()
        && text.match_indices(pattern).any(|(at, _)| {
            !text[at + pattern.len()..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit())
        })
}

/// 一张表里命中的最长型号片段。
fn best<'a>(models: &'a [Entry], text: &str) -> Option<(usize, &'a Entry)> {
    models
        .iter()
        .filter_map(|entry| {
            entry
                .patterns
                .iter()
                .filter(|pattern| names(text, pattern))
                .map(|pattern| pattern.len())
                .max()
                .map(|length| (length, entry))
        })
        .max_by_key(|(length, _)| *length)
}

fn lookup(model: &ModelOption, fetched: Option<&[Entry]>) -> Option<(Entry, bool)> {
    let text = normalized(&format!("{} {}", model.id, model.name));
    let local = best(&bundled().models, &text);
    let online = fetched.and_then(|models| best(models, &text));
    match (online, local) {
        (Some((a, online)), Some((b, _))) if a >= b => Some((online.clone(), true)),
        (_, Some((_, local))) => Some((local.clone(), false)),
        (Some((_, online)), None) => Some((online.clone(), true)),
        (None, None) => None,
    }
}

/// 各成员当前的模型目录：Claude 的 `opus`、`default` 这类别名要靠目录里的显示名才认得出是哪个型号。
fn catalogs() -> &'static Mutex<std::collections::HashMap<String, Vec<ModelOption>>> {
    static CATALOGS: OnceLock<Mutex<std::collections::HashMap<String, Vec<ModelOption>>>> = OnceLock::new();
    CATALOGS.get_or_init(Default::default)
}

pub fn remember(agent: &str, models: &[ModelOption]) {
    catalogs().lock().unwrap().insert(agent.to_owned(), models.to_vec());
}

/// 一回合按 API 价折成多少美元；价格表里没有这个型号返回 None，不瞎估。
/// usage 是 ACP 口径：inputTokens 含缓存读写。
pub fn cost(agent: &str, model: &str, usage: &Value, context: Option<u64>, at: u64) -> Option<f64> {
    let option = catalogs().lock().unwrap().get(agent).and_then(|models| models.iter().find(|option| option.id == model).cloned());
    let option = option.unwrap_or_else(|| ModelOption { id: model.into(), name: String::new(), provider_id: None, provider_name: None, efforts: vec![], default_effort: None });
    let guard = remote().read().ok();
    let fetched = guard.as_ref().and_then(|remote| remote.as_ref()).map(|remote| remote.models.as_slice());
    let (entry, _) = lookup(&option, fetched)?;
    // shortcut: 分段价按这回合最后一次调用的上下文判，回合里前几次调用可能还没过线；要精确得逐次调用记用量。
    let (mut input, mut cached, mut output) = match &entry.long_context {
        Some(tier) if context.is_some_and(|context| context > tier.above) => (tier.input, tier.cached, tier.output),
        _ => (entry.input, entry.cached, entry.output),
    };
    if let Some(rule) = entry.off_peak.as_ref().filter(|rule| off_peak_at(rule, at)) {
        (input, cached, output) = (rule.input, rule.cached, rule.output);
    }
    let tokens = |key: &str| usage[key].as_f64().unwrap_or(0.0).max(0.0);
    let (read, write) = (tokens("cachedReadTokens"), tokens("cachedWriteTokens"));
    let fresh = (tokens("inputTokens") - read - write).max(0.0);
    let dollars = (fresh * input + read * cached + write * entry.cache_write.unwrap_or(input) + tokens("outputTokens") * output) / 1e6;
    Some((dollars * 1e6).round() / 1e6)
}

fn minutes(text: &str) -> Option<u32> {
    let (hour, minute) = text.trim().split_once(':')?;
    Some(hour.parse::<u32>().ok()? * 60 + minute.parse::<u32>().ok()?)
}

/// 现在是否在闲时窗口里（按 UTC 算；节假日不计）。
fn off_peak_at(rule: &OffPeak, unix: u64) -> bool {
    let days = unix / 86_400;
    // 1970-01-01 是星期四（ISO 4）。
    let weekday = ((days + 3) % 7 + 1) as u32;
    let minute = (unix % 86_400 / 60) as u32;
    rule.windows.iter().any(|window| {
        window.weekdays.contains(&weekday)
            && window.hours_utc.iter().any(|span| {
                let Some((from, to)) = span.split_once('-') else {
                    return false;
                };
                let (Some(from), Some(to)) = (minutes(from), minutes(to)) else {
                    return false;
                };
                let to = if to == 0 { 24 * 60 } else { to };
                minute >= from && minute < to
            })
    })
}

#[cfg(test)]
fn describe(entry: &Entry, unix: u64) -> Value {
    let mut value = json!({"name":entry.name,"input":entry.input,"cached":entry.cached,"output":entry.output});
    if let Some(long) = &entry.long_context {
        value["long_context"] = json!(long);
    }
    if let Some(rule) = &entry.off_peak {
        let off = off_peak_at(rule, unix);
        value["off_peak"] = json!({"input":rule.input,"cached":rule.cached,"output":rule.output});
        // peak=true：现在按标价（高峰价）计；false：现在是闲时价。
        value["peak"] = json!(!off);
    }
    for (key, field) in [("context_window", entry.context_window), ("max_output", entry.max_output)] {
        if let Some(tokens) = field {
            value[key] = json!(tokens);
        }
    }
    for (key, field) in [("reasoning", entry.reasoning), ("vision", entry.vision)] {
        if let Some(flag) = field {
            value[key] = json!(flag);
        }
    }
    value
}

#[cfg(test)]
fn date(unix: u64) -> String {
    // 公历换算（Howard Hinnant 的 civil_from_days）。
    let z = (unix / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// 设置页显示的状态。
pub fn status() -> Value {
    let table = bundled();
    let guard = remote().read().ok();
    let remote = guard.as_ref().and_then(|remote| remote.as_ref());
    json!({
        "source": if remote.is_some() { "litellm" } else { "bundled" },
        "fetched_at": remote.map(|remote| remote.fetched_at),
        "models": remote.map(|remote| remote.models.len()),
        "bundled_checked": table.checked,
        "bundled_models": table.models.len(),
        "error": last_error().lock().ok().and_then(|error| error.clone()),
    })
}

fn per_million(value: Option<&Value>) -> Option<f64> {
    let cost = value?.as_f64()?;
    (cost.is_finite() && cost >= 0.0).then(|| (cost * 1e6 * 1e6).round() / 1e6)
}

fn dated(name: &str) -> bool {
    let parts: Vec<&str> = name.split('-').collect();
    let numeric = |part: &&str, length: usize| part.len() == length && part.chars().all(|c| c.is_ascii_digit());
    parts.last().is_some_and(|last| numeric(last, 8) || numeric(last, 4) && parts.len() > 1)
        || parts.len() >= 3
            && numeric(&parts[parts.len() - 3], 4)
            && numeric(&parts[parts.len() - 2], 2)
            && numeric(&parts[parts.len() - 1], 2)
}

/// 最低的分段门槛：`input_cost_per_token_above_272k_tokens` 这类字段。
fn tier(fields: &Map<String, Value>, cached: f64, output: f64) -> Option<Tier> {
    let above = fields
        .keys()
        .filter_map(|key| key.strip_prefix("input_cost_per_token_above_")?.strip_suffix("k_tokens")?.parse::<u64>().ok())
        .min()?;
    let input = per_million(fields.get(&format!("input_cost_per_token_above_{above}k_tokens")))?;
    Some(Tier {
        above: above * 1_000,
        input,
        cached: per_million(fields.get(&format!("cache_read_input_token_cost_above_{above}k_tokens"))).unwrap_or(cached),
        output: per_million(fields.get(&format!("output_cost_per_token_above_{above}k_tokens"))).unwrap_or(output),
    })
}

fn off_peak(fields: &Map<String, Value>, cached: f64) -> Option<OffPeak> {
    let rule = fields.get("off_peak_pricing")?.as_object()?;
    let windows = rule
        .get("windows")?
        .as_array()?
        .iter()
        .filter_map(|window| {
            let weekdays = window
                .get("weekdays")?
                .as_array()?
                .iter()
                .filter_map(|day| day.as_u64().map(|day| day as u32))
                .collect::<Vec<_>>();
            let hours = match window.get("hours_utc")? {
                Value::String(span) => vec![span.clone()],
                Value::Array(spans) => spans.iter().filter_map(|span| span.as_str().map(str::to_owned)).collect(),
                _ => return None,
            };
            Some(Window { weekdays, hours_utc: hours })
        })
        .collect::<Vec<_>>();
    if windows.is_empty() {
        return None;
    }
    Some(OffPeak {
        input: per_million(rule.get("input_cost_per_token"))?,
        cached: per_million(rule.get("cache_read_input_token_cost")).unwrap_or(cached),
        output: per_million(rule.get("output_cost_per_token"))?,
        windows,
    })
}

/// 从 LiteLLM 的价格表里抽出调度要用的条目。
pub fn from_litellm(table: &Value) -> Vec<Entry> {
    let Some(table) = table.as_object() else {
        return Vec::new();
    };
    let mut direct = Vec::new();
    let mut fallback = Vec::new();
    for (key, fields) in table {
        let Some(fields) = fields.as_object() else {
            continue;
        };
        let provider = fields.get("litellm_provider").and_then(Value::as_str).unwrap_or("");
        let primary = PROVIDERS.contains(&provider);
        if !primary && !FALLBACK_PROVIDERS.contains(&provider) {
            continue;
        }
        if !matches!(fields.get("mode").and_then(Value::as_str), Some("chat" | "responses")) {
            continue;
        }
        let name = key.strip_prefix(&format!("{provider}/")).unwrap_or(key);
        if name.contains(['/', '@', ':']) || dated(name) {
            continue;
        }
        let (Some(input), Some(output)) = (
            per_million(fields.get("input_cost_per_token")),
            per_million(fields.get("output_cost_per_token")),
        ) else {
            continue;
        };
        let cached = per_million(fields.get("cache_read_input_token_cost"))
            .or_else(|| per_million(fields.get("input_cost_per_token_cache_hit")))
            .unwrap_or(input);
        // Claude 的目录名不带「claude」前缀（如 Haiku 5.5），型号片段去掉它。
        let pattern = normalized(name.strip_prefix("claude-").unwrap_or(name));
        if pattern.len() < 3 {
            continue;
        }
        let entry = Entry {
            patterns: vec![pattern],
            name: name.to_owned(),
            input,
            cached,
            output,
            cache_write: per_million(fields.get("cache_creation_input_token_cost")),
            long_context: tier(fields, cached, output),
            off_peak: off_peak(fields, cached),
            context_window: fields.get("max_input_tokens").and_then(Value::as_u64),
            max_output: fields.get("max_output_tokens").and_then(Value::as_u64),
            reasoning: fields.get("supports_reasoning").and_then(Value::as_bool),
            vision: fields.get("supports_vision").and_then(Value::as_bool),
            source: fields.get("source").and_then(Value::as_str).map(str::to_owned),
        };
        if primary { direct.push(entry) } else { fallback.push(entry) }
    }
    for entry in fallback {
        if !direct.iter().any(|known| known.patterns == entry.patterns) {
            direct.push(entry);
        }
    }
    direct.sort_by(|a, b| a.name.cmp(&b.name));
    direct.dedup_by(|a, b| a.patterns == b.patterns);
    direct
}

fn save(remote: &Remote) -> Result<(), String> {
    let Some(dir) = directory().get() else {
        return Ok(());
    };
    let path = dir.join(CACHE);
    let temporary = path.with_extension("json.tmp");
    let text = serde_json::to_string(remote).map_err(|_| "价格表缓存写入失败")?;
    std::fs::write(&temporary, text).map_err(|_| "价格表缓存写入失败")?;
    std::fs::rename(&temporary, &path).map_err(|_| "价格表缓存写入失败".into())
}

/// 联网更新一次价格表：先试 GitHub，连不上再试 jsDelivr 镜像；内容没变（ETag 相同）只记更新时间。
pub fn refresh() -> Result<Value, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(60))
        .try_proxy_from_env(true)
        .build();
    let etag = remote().read().ok().and_then(|remote| remote.as_ref().and_then(|remote| remote.etag.clone()));
    let mut failures = Vec::new();
    for source in SOURCES {
        let mut request = agent.get(source);
        if let Some(etag) = &etag {
            request = request.set("If-None-Match", etag);
        }
        let response = match request.call() {
            Ok(response) => response,
            Err(error) => {
                failures.push(format!("{source}: {error}"));
                continue;
            }
        };
        if response.status() == 304 {
            if let Some(remote) = remote().write().ok().as_mut().and_then(|remote| remote.as_mut()) {
                remote.fetched_at = now();
                save(remote)?;
            }
            *last_error().lock().unwrap() = None;
            return Ok(status());
        }
        let tag = response.header("ETag").map(str::to_owned);
        let mut body = String::new();
        // 价格表现在约 3MB，留足余量，防止拿到异常大的响应。
        let mut reader = std::io::Read::take(response.into_reader(), 64 * 1024 * 1024);
        if std::io::Read::read_to_string(&mut reader, &mut body).is_err() {
            failures.push(format!("{source}: 读取中断"));
            continue;
        }
        let models = serde_json::from_str::<Value>(&body).map(|table| from_litellm(&table)).unwrap_or_default();
        // 抽出的条目太少说明格式变了或拿到的不是价格表，宁可继续用旧的。
        if models.len() < 20 {
            failures.push(format!("{source}: 内容不像价格表（只解析出 {} 条）", models.len()));
            continue;
        }
        let fresh = Remote { fetched_at: now(), etag: tag, source: source.into(), models };
        save(&fresh)?;
        *remote().write().unwrap() = Some(fresh);
        *last_error().lock().unwrap() = None;
        return Ok(status());
    }
    let error = format!("价格表更新失败，继续用{}：{}", if etag.is_some() { "上次的" } else { "内置表" }, failures.join("；"));
    *last_error().lock().unwrap() = Some(error.clone());
    Err(error)
}

/// 启动时载入上次的缓存，后台按天更新。
pub fn init(dir: &Path) {
    let _ = directory().set(dir.to_path_buf());
    let cached = std::fs::read_to_string(dir.join(CACHE))
        .ok()
        .and_then(|text| serde_json::from_str::<Remote>(&text).ok())
        .filter(|remote| !remote.models.is_empty());
    let due = cached.as_ref().map_or(0, |remote| (remote.fetched_at + REFRESH_SECS).saturating_sub(now()));
    *remote().write().unwrap() = cached;
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(due.max(20)));
        loop {
            let _ = refresh();
            std::thread::sleep(Duration::from_secs(REFRESH_SECS));
        }
    });
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
    fn bundled_price(id: &str, name: &str) -> Value {
        describe(&lookup(&model(id, name), None).unwrap().0, 0)
    }

    #[test]
    fn catalog_names_find_their_price_without_prefix_collisions() {
        let haiku = bundled_price("haiku", "Haiku 5.5");
        assert_eq!(haiku["name"], "Claude Haiku 5.5");
        assert_eq!(haiku["long_context"]["above"], 100000);
        assert_eq!(bundled_price("claude-haiku-4-5-20251001", "Haiku 4.5")["name"], "Claude Haiku 4.5");
        assert_eq!(bundled_price("sonnet", "Sonnet 5.5")["name"], "Claude Sonnet 5.5");
        assert_eq!(bundled_price("claude-sonnet-5", "Sonnet 5")["name"], "Claude Sonnet 5");
        assert_eq!(bundled_price("default", "Default (recommended) · Opus 5.5")["name"], "Claude Opus 5.5");
        assert_eq!(bundled_price("claude/claude-sonnet-5-5", "Claude Sonnet 5.5")["name"], "Claude Sonnet 5.5");
        assert_eq!(bundled_price("default", "默认 · Claude Opus 5.5")["name"], "Claude Opus 5.5");
        assert_eq!(bundled_price("gpt-6.1-sol", "GPT-6.1 Sol")["name"], "GPT-6.1 Sol");
        assert_eq!(bundled_price("gpt-6-sol", "gpt-6-sol")["name"], "GPT-6 Sol");
        assert_eq!(bundled_price("gpt-6-luna", "GPT-6 Luna")["name"], "GPT-6 Luna");
        assert_eq!(bundled_price("gpt-5.6-sol", "GPT-5.6 Sol")["name"], "GPT-5.6 Sol");
        assert_eq!(bundled_price("gpt-5.4-mini", "gpt-5.4-mini")["name"], "GPT-5.4 mini");
        assert_eq!(bundled_price("gpt-5.3-codex", "gpt-5.3-codex")["name"], "GPT-5.2 / GPT-5.3-Codex");
        assert_eq!(
            bundled_price("[\"deepseek-official\",\"deepseek-v4-flash\"]", "DeepSeek · deepseek-v4-flash")["name"],
            "DeepSeek V4.1 Flash"
        );
        assert!(lookup(&model("gpt-6.9", "GPT-6.9"), None).is_none(), "没收录的新型号不能套用旧价");
    }

    #[test]
    fn deepseek_off_peak_windows_follow_utc_weekdays() {
        let flash = lookup(&model("deepseek-v4-flash", "deepseek-v4-flash"), None).unwrap().0;
        let rule = flash.off_peak.as_ref().unwrap();
        // 2026-10-15 是星期四。
        let thursday = 1_792_022_400; // 2026-10-15 00:00 UTC
        assert_eq!(date(thursday), "2026-10-15");
        assert!(off_peak_at(rule, thursday + 30 * 60), "00:30 闲时");
        assert!(!off_peak_at(rule, thursday + 2 * 3_600), "02:00 高峰");
        assert!(off_peak_at(rule, thursday + 5 * 3_600), "05:00 闲时");
        assert!(!off_peak_at(rule, thursday + 7 * 3_600), "07:00 高峰");
        assert!(off_peak_at(rule, thursday + 12 * 3_600), "12:00 闲时");
        assert!(off_peak_at(rule, thursday + 2 * 86_400 + 2 * 3_600), "周六全天闲时");
        let at_peak = describe(&flash, thursday + 2 * 3_600);
        assert_eq!(at_peak["peak"], true);
        assert!(at_peak["off_peak"]["input"].as_f64().unwrap() < at_peak["input"].as_f64().unwrap());
    }

    #[test]
    fn litellm_table_is_extracted_with_tiers_off_peak_and_capabilities() {
        let table = json!({
            "sample_spec": {"input_cost_per_token": 0},
            "gpt-6-sol": {"litellm_provider":"openai","mode":"chat","input_cost_per_token":2e-6,"cache_read_input_token_cost":2e-7,"output_cost_per_token":1e-5,
                "input_cost_per_token_above_272k_tokens":4e-6,"cache_read_input_token_cost_above_272k_tokens":4e-7,"output_cost_per_token_above_272k_tokens":1.5e-5,
                "input_cost_per_token_above_272k_tokens_priority":1.6e-5,"max_input_tokens":922000,"max_output_tokens":128000,"supports_reasoning":true,"supports_vision":true},
            "gpt-6-sol-2026-09-22": {"litellm_provider":"openai","mode":"chat","input_cost_per_token":1e-6,"output_cost_per_token":1e-6},
            "azure/gpt-6.1-sol": {"litellm_provider":"azure","mode":"chat","input_cost_per_token":2e-6,"cache_read_input_token_cost":1e-7,"output_cost_per_token":1e-5},
            "azure/gpt-6-sol": {"litellm_provider":"azure","mode":"chat","input_cost_per_token":9e-6,"output_cost_per_token":9e-6},
            "azure/eu/gpt-6.1-sol": {"litellm_provider":"azure","mode":"chat","input_cost_per_token":2.4e-6,"output_cost_per_token":1.2e-5},
            "chatgpt/gpt-6.1-sol": {"litellm_provider":"chatgpt","mode":"responses"},
            "claude-haiku-5-5": {"litellm_provider":"anthropic","mode":"chat","input_cost_per_token":1e-7,"cache_read_input_token_cost":1e-8,"output_cost_per_token":5e-7,
                "input_cost_per_token_above_100k_tokens":5e-7,"output_cost_per_token_above_100k_tokens":2.5e-6},
            "deepseek-v4-flash": {"litellm_provider":"deepseek","mode":"chat","input_cost_per_token":3e-7,"cache_read_input_token_cost":6e-9,"output_cost_per_token":1.2e-6,
                "off_peak_pricing":{"input_cost_per_token":1.5e-7,"cache_read_input_token_cost":3e-9,"output_cost_per_token":6e-7,
                    "windows":[{"hours_utc":["00:00-01:00","04:00-06:00","10:00-00:00"],"weekdays":[1,2,3,4,5]},{"hours_utc":"00:00-00:00","weekdays":[6,7]}]}},
            "text-embedding-9": {"litellm_provider":"openai","mode":"embedding","input_cost_per_token":1e-7},
            "bedrock/claude-opus-5-5": {"litellm_provider":"bedrock","mode":"chat","input_cost_per_token":1e-6,"output_cost_per_token":1e-6}
        });
        let entries = from_litellm(&table);
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["claude-haiku-5-5", "deepseek-v4-flash", "gpt-6-sol", "gpt-6.1-sol"], "带日期、地区线路、非对话、其他云的条目不收；Azure 只补官方没有的");
        let sol = &entries[2];
        assert_eq!((sol.input, sol.cached, sol.output), (2.0, 0.2, 10.0), "官方条目优先于 Azure 同名条目");
        assert_eq!(sol.long_context, Some(Tier { above: 272_000, input: 4.0, cached: 0.4, output: 15.0 }));
        assert_eq!((sol.context_window, sol.reasoning, sol.vision), (Some(922_000), Some(true), Some(true)));
        assert_eq!(entries[0].patterns, ["haiku55"]);
        assert_eq!(entries[0].long_context.as_ref().unwrap().cached, 0.01, "分段缺缓存价就沿用基础缓存价");
        let flash = entries[1].off_peak.as_ref().unwrap();
        assert_eq!((flash.input, flash.output, flash.windows.len()), (0.15, 0.6, 2));

        let fetched = entries.as_slice();
        let (codex, online) = lookup(&model("gpt-6.1-sol", "GPT-6.1 Sol"), Some(fetched)).unwrap();
        assert!(online && codex.name == "gpt-6.1-sol", "同样具体时用自动更新的");
        let (claude, online) = lookup(&model("haiku", "Haiku 5.5"), Some(fetched)).unwrap();
        assert!(online && claude.name == "claude-haiku-5-5");
        let (opus, online) = lookup(&model("default", "Default (recommended) · Opus 5.5"), Some(fetched)).unwrap();
        assert!(!online && opus.name == "Claude Opus 5.5", "自动更新没有的型号用内置表补");
        assert!(lookup(&model("gpt-6.9", "GPT-6.9"), Some(fetched)).is_none());
    }

    #[test]
    fn a_turn_is_priced_by_the_catalog_name_and_unknown_models_stay_unpriced() {
        remember("claude-test", &[model("opus", "Opus 5.5")]);
        let usage = json!({"inputTokens": 1_100_000, "cachedReadTokens": 1_000_000, "cachedWriteTokens": 0, "outputTokens": 10_000});
        let opus = lookup(&model("opus", "Opus 5.5"), None).unwrap().0;
        let expected = 100_000.0 * opus.input / 1e6 + 1_000_000.0 * opus.cached / 1e6 + 10_000.0 * opus.output / 1e6;
        let got = cost("claude-test", "opus", &usage, Some(1000), 0).unwrap();
        assert!((got - expected).abs() < 1e-6, "{got} vs {expected}");
        assert_eq!(cost("claude-test", "gpt-6.9", &usage, None, 0), None);
    }

    #[test]
    fn dated_snapshots_are_recognized() {
        assert!(dated("gpt-6-sol-2026-09-22"));
        assert!(dated("claude-haiku-4-5-20251001"));
        assert!(dated("gpt-4-0613"));
        assert!(!dated("gpt-6.1-sol"));
        assert!(!dated("claude-opus-5-5"));
    }
}
