//! Command Code（GOAT 套餐）的额度：DSH、Hermes 走 Command Code 的线路时，用该线路自己配置的 Key
//! 只读查询官方的计费接口（`/alpha/billing/credits`、`/alpha/usage/summary`、`/alpha/billing/subscriptions`），
//! 给调度和进度页看 5 小时 / 每周 / 本月窗口、剩余额度和本期用量。
//! 这些是未公开文档的 alpha 接口，字段缺了就不给，不编；Key 只用来发请求，不落盘、不进日志，
//! 对外只露一个短指纹，用来看出两条线路是不是同一个账号。
use serde_json::{json, Value};
use sha2::Digest;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const API: &str = "https://api.commandcode.ai/alpha/";
const FRESH: Duration = Duration::from_secs(300);

/// 一条走 Command Code 的线路。
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub agent: &'static str,
    pub name: String,
    pub key: String,
}

/// 模型 id 里带的线路名：Hermes 是 `custom:<线路>:<模型>`，DSH 是 `["<线路>","<模型>"]`。
pub fn line_of(model: &str) -> Option<String> {
    if let Some(rest) = model.strip_prefix("custom:") {
        return rest.split_once(':').map(|(line, _)| line.to_owned());
    }
    serde_json::from_str::<Vec<String>>(model)
        .ok()
        .and_then(|parts| (parts.len() == 2).then(|| parts[0].clone()))
}

/// `.env` 风格文件里取一个变量。
fn env_file(path: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()?.lines().find_map(|line| {
        let line = line.trim().strip_prefix("export ").unwrap_or(line.trim());
        let (key, value) = line.split_once('=')?;
        (key.trim() == name).then(|| value.trim().trim_matches(['"', '\'']).to_owned())
    })
    .filter(|value| !value.is_empty())
}

fn env_value(name: &str, env_files: &[PathBuf]) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| env_files.iter().find_map(|path| env_file(path, name)))
}

fn yaml_value(line: &str, key: &str) -> Option<String> {
    let rest = line.trim().trim_start_matches("- ").strip_prefix(key)?.trim_start();
    let value = rest.strip_prefix(':')?.trim().trim_matches(['"', '\'']);
    (!value.is_empty()).then(|| value.to_owned())
}

/// DSH：`llm-pi-ai.providers.<线路>` 下 `baseURL` 指向 commandcode 的，Key 在 `apiKeyEnv` 指的变量里
/// （进程环境或 DSH 家目录的 .env）。新版的 profile 文件同样格式，一并读。
pub fn dsh_lines(home: &Path) -> Vec<Line> {
    let mut files = vec![home.join("settings.yaml")];
    if let Ok(entries) = std::fs::read_dir(home.join("profiles")) {
        files.extend(entries.flatten().map(|entry| entry.path()).filter(|path| {
            path.extension().is_some_and(|ext| ext == "yaml" || ext == "yml")
        }));
    }
    let env_files = [home.join(".env")];
    let mut lines = Vec::new();
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else { continue };
        let (mut name, mut base, mut env) = (None::<String>, false, None::<String>);
        let mut flush = |name: &mut Option<String>, base: &mut bool, env: &mut Option<String>| {
            if let (Some(line), true, Some(env)) = (name.take(), *base, env.take()) {
                if let Some(key) = env_value(&env, &env_files) {
                    lines.push(Line { agent: "dsh-win", name: line, key });
                }
            }
            *base = false;
        };
        for raw in text.lines() {
            let indent = raw.len() - raw.trim_start().len();
            let trimmed = raw.trim();
            // 线路名这一层：四个空格缩进、以冒号结尾、没有值。
            if indent == 4 && trimmed.ends_with(':') && !trimmed.contains(' ') {
                flush(&mut name, &mut base, &mut env);
                name = Some(trimmed.trim_end_matches(':').to_owned());
            } else if indent < 4 && !trimmed.is_empty() {
                flush(&mut name, &mut base, &mut env);
            } else if let Some(url) = yaml_value(raw, "baseURL") {
                base = url.contains("commandcode.ai");
            } else if let Some(name) = yaml_value(raw, "apiKeyEnv") {
                env = Some(name);
            }
        }
        flush(&mut name, &mut base, &mut env);
    }
    lines.dedup();
    lines
}

/// Hermes：`config.yaml` 的 `providers`（`<线路名>: {base_url, key_env}`）或 `custom_providers` 列表里
/// `base_url` 指向 commandcode 的；Key 在 `key_env` / `api_key_env` 指的变量里，或直接写在 `api_key`（可写 `${变量}`），
/// 变量从进程环境或 Hermes 家目录的 .env 取。线路名用 providers 的键（模型 id `custom:<线路>:…` 里就是它）。
pub fn hermes_lines(home: &Path) -> Vec<Line> {
    let Ok(text) = std::fs::read_to_string(home.join("config.yaml")) else {
        return Vec::new();
    };
    let env_files = [home.join(".env")];
    let mut lines = Vec::new();
    let mut section = "";
    let (mut name, mut keyed, mut base, mut key) = (None::<String>, false, false, None::<String>);
    let mut flush = |name: &mut Option<String>, base: &mut bool, key: &mut Option<String>| {
        if let (Some(line), true, Some(key)) = (name.take(), *base, key.take()) {
            lines.push(Line { agent: "hermes-win", name: line, key });
        }
        *base = false;
        *key = None;
    };
    for raw in text.lines() {
        if !raw.starts_with(' ') && !raw.starts_with('-') && !raw.trim().is_empty() {
            flush(&mut name, &mut base, &mut key);
            section = match raw.trim_end() {
                "providers:" => "map",
                "custom_providers:" => "list",
                _ => "",
            };
            continue;
        }
        if section.is_empty() {
            continue;
        }
        let indent = raw.len() - raw.trim_start().len();
        let trimmed = raw.trim();
        if section == "map" && indent == 2 && trimmed.ends_with(':') && !trimmed.contains(' ') {
            flush(&mut name, &mut base, &mut key);
            name = Some(trimmed.trim_end_matches(':').to_owned());
            keyed = true;
            continue;
        }
        if section == "list" && trimmed.starts_with("- ") {
            flush(&mut name, &mut base, &mut key);
            keyed = false;
        }
        if let Some(value) = yaml_value(raw, "name") {
            if !keyed {
                name = Some(value);
            }
        } else if let Some(url) = yaml_value(raw, "base_url") {
            base = url.contains("commandcode.ai");
        } else if let Some(value) = yaml_value(raw, "api_key") {
            key = match value.strip_prefix("${").and_then(|rest| rest.strip_suffix('}')) {
                Some(variable) => env_value(variable, &env_files),
                None => Some(value),
            };
        } else if let Some(variable) = yaml_value(raw, "key_env").or_else(|| yaml_value(raw, "api_key_env")) {
            key = env_value(&variable, &env_files);
        }
    }
    flush(&mut name, &mut base, &mut key);
    lines
}

/// Key 的短指纹：只用来看出两条线路是不是同一个账号。
pub fn fingerprint(key: &str) -> String {
    let digest = sha2::Sha256::digest(key.as_bytes());
    format!("goat-{:02x}{:02x}{:02x}", digest[0], digest[1], digest[2])
}

fn window(item: &Value) -> Option<Value> {
    let (used, cap) = (item["used"].as_f64()?, item["cap"].as_f64()?);
    if cap <= 0.0 {
        return None;
    }
    let mut value = json!({
        "used_percent": (used / cap * 1000.0).round() / 10.0,
        "used_usd": (used * 100.0).round() / 100.0,
        "cap_usd": cap,
    });
    // resetAt 为 0 表示这个窗口还没开始计（没用过）。
    if let Some(at) = item["resetAt"].as_i64().filter(|at| *at > 0) {
        let hours = (at / 1000 - crate::quota_reset::now_secs()) as f64 / 3600.0;
        value["resets_in_hours"] = json!((hours.max(0.0) * 10.0).round() / 10.0);
    }
    if item["exceeded"] == true {
        value["exceeded"] = json!(true);
    }
    Some(value)
}

/// 三个接口的返回拼成一份额度概况；某个接口没拿到就少那几项。
pub fn summarize(credits: Option<&Value>, usage: Option<&Value>, plan: Option<&Value>) -> Option<Value> {
    let mut value = json!({"source":"commandcode"});
    if let Some(credits) = credits {
        let limits = &credits["windowLimits"];
        if let Some(five) = window(&limits["fiveHour"]) {
            value["five_hour"] = five;
        }
        if let Some(weekly) = window(&limits["weekly"]) {
            value["weekly"] = weekly;
        }
        let left: f64 = ["monthlyCredits", "purchasedCredits", "freeCredits"]
            .iter()
            .filter_map(|key| credits["credits"][key].as_f64())
            .sum();
        value["credits_left_usd"] = json!((left * 100.0).round() / 100.0);
        // 本月套餐额度：已用（本期计入套餐的消耗）÷（已用 + 套餐剩余）。
        if let (Some(spent), Some(remaining)) = (
            usage.and_then(|usage| usage["totalMonthlyCredits"].as_f64()),
            credits["credits"]["monthlyCredits"].as_f64(),
        ) {
            if spent + remaining > 0.0 {
                value["monthly"] = json!({"used_percent": (spent / (spent + remaining) * 1000.0).round() / 10.0});
            }
        }
    }
    if let Some(usage) = usage {
        value["period"] = json!({
            "spent_usd": usage["totalCost"].as_f64().map(|cost| (cost * 100.0).round() / 100.0),
            "purchased_usd": usage["totalPurchasedCredits"].as_f64().map(|cost| (cost * 100.0).round() / 100.0),
            "tokens_in": usage["totalTokensIn"], "tokens_out": usage["totalTokensOut"], "requests": usage["totalCount"],
        });
    }
    if let Some(plan) = plan.map(|plan| &plan["data"]) {
        value["plan"] = plan["planId"].clone();
        value["period_end"] = plan["currentPeriodEnd"].clone();
        if !plan["cancelAt"].is_null() {
            value["cancel_at"] = plan["cancelAt"].clone();
        }
    }
    (value.as_object().is_some_and(|map| map.len() > 1)).then_some(value)
}

fn fetch(key: &str) -> Option<Value> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
        .try_proxy_from_env(true)
        .build();
    let get = |path: &str| -> Option<Value> {
        agent
            .get(&format!("{API}{path}"))
            .set("Authorization", &format!("Bearer {key}"))
            .call()
            .ok()?
            .into_json()
            .ok()
    };
    let credits = get("billing/credits");
    let usage = get("usage/summary");
    let plan = get("billing/subscriptions");
    summarize(credits.as_ref(), usage.as_ref(), plan.as_ref())
}

fn cache() -> &'static Mutex<HashMap<String, (Instant, Option<Value>)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (Instant, Option<Value>)>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// 同一个账号 5 分钟内只查一次（两条线路共用一个 Key 时也只查一次）。
fn cached(key: &str) -> Option<Value> {
    let id = fingerprint(key);
    if let Some((at, value)) = cache().lock().ok()?.get(&id) {
        if at.elapsed() < FRESH {
            return value.clone();
        }
    }
    let value = fetch(key);
    if let Ok(mut cache) = cache().lock() {
        cache.insert(id, (Instant::now(), value.clone()));
    }
    value
}

/// 给调度和进度页：{"<成员>:<线路>": 额度概况}，键和资源池的 pool 一致。
pub fn overview() -> serde_json::Map<String, Value> {
    let mut lines = crate::dsh::resolved_dsh_home().map(|home| dsh_lines(&home)).unwrap_or_default();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        lines.extend(hermes_lines(&PathBuf::from(local).join("hermes")));
    }
    let mut result = serde_json::Map::new();
    for line in lines {
        if let Some(mut value) = cached(&line.key) {
            value["account"] = json!(fingerprint(&line.key));
            result.insert(format!("{}:{}", line.agent, line.name), value);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_names_come_from_model_ids() {
        assert_eq!(line_of("custom:command-code-daily-2:deepseek/deepseek-v4.1-flash").as_deref(), Some("command-code-daily-2"));
        assert_eq!(line_of("[\"command-code-daily\",\"deepseek-v4-flash\"]").as_deref(), Some("command-code-daily"));
        assert_eq!(line_of("gpt-6-luna"), None);
    }

    #[test]
    fn reads_dsh_and_hermes_lines_without_touching_other_providers() {
        let home = std::env::temp_dir().join(format!("goat-test-{}", uuid::Uuid::new_v4()));
        let dsh = home.join("dsh");
        std::fs::create_dir_all(&dsh).unwrap();
        std::fs::write(dsh.join("settings.yaml"), "llm-pi-ai:\n  providers:\n    command-code-daily:\n      baseURL: https://api.commandcode.ai/provider/v1\n      apiKeyEnv: GOAT_TEST_ONE\n      models:\n      - id: flash\n    other:\n      baseURL: https://api.deepseek.com\n      apiKeyEnv: GOAT_TEST_TWO\n").unwrap();
        std::fs::write(dsh.join(".env"), "GOAT_TEST_ONE=\"k-one\"\nGOAT_TEST_TWO=k-two\n").unwrap();
        assert_eq!(dsh_lines(&dsh), vec![Line { agent: "dsh-win", name: "command-code-daily".into(), key: "k-one".into() }]);
        let hermes = home.join("hermes");
        std::fs::create_dir_all(&hermes).unwrap();
        std::fs::write(hermes.join("config.yaml"), "model:\n  default: x\ncustom_providers:\n- name: command-code-daily-2\n  base_url: https://api.commandcode.ai/provider/v1\n  api_key: ${GOAT_TEST_THREE}\n- name: local\n  base_url: http://127.0.0.1:8080/v1\n  api_key: abc\n").unwrap();
        std::fs::write(hermes.join(".env"), "GOAT_TEST_THREE=k-three\n").unwrap();
        assert_eq!(hermes_lines(&hermes), vec![Line { agent: "hermes-win", name: "command-code-daily-2".into(), key: "k-three".into() }]);
        // Hermes 常用的 providers 映射写法：线路名取键名，不取显示名。
        std::fs::write(hermes.join("config.yaml"), "providers:\n  packycode:\n    name: Packy\n    base_url: https://example.com/v1\n    key_env: GOAT_TEST_THREE\n  command-code-daily-2:\n    name: Command Code 2\n    base_url: https://api.commandcode.ai/provider/v1\n    models:\n      gpt-6-luna: {}\n    key_env: GOAT_TEST_THREE\nagent:\n  x: 1\n").unwrap();
        assert_eq!(hermes_lines(&hermes), vec![Line { agent: "hermes-win", name: "command-code-daily-2".into(), key: "k-three".into() }]);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn summarizes_real_response_shapes() {
        let credits = json!({"credits":{"monthlyCredits":0.76,"purchasedCredits":4.05,"freeCredits":0},"windowLimits":{"limited":true,"fiveHour":{"used":0,"cap":14,"exceeded":false,"resetAt":0},"weekly":{"used":5.42,"cap":35,"exceeded":false,"resetAt":(crate::quota_reset::now_secs() + 7200) * 1000}}});
        let usage = json!({"totalCount":15407,"totalCost":69.25,"totalTokensIn":2723276072u64,"totalTokensOut":27199664,"totalMonthlyCredits":69.24,"totalPurchasedCredits":0});
        let plan = json!({"data":{"planId":"individual-goat","currentPeriodEnd":"2026-11-02T08:44:37.000Z","cancelAt":"2026-11-02T08:44:37.000Z"}});
        let value = summarize(Some(&credits), Some(&usage), Some(&plan)).unwrap();
        assert_eq!(value["five_hour"]["used_percent"], 0.0);
        assert!(value["five_hour"].get("resets_in_hours").is_none());
        assert_eq!((value["weekly"]["used_percent"].as_f64(), value["weekly"]["resets_in_hours"].as_f64()), (Some(15.5), Some(2.0)));
        assert_eq!(value["credits_left_usd"], 4.81);
        assert_eq!(value["monthly"]["used_percent"], 98.9);
        assert_eq!((value["period"]["spent_usd"].as_f64(), value["period"]["tokens_out"].as_u64()), (Some(69.25), Some(27199664)));
        assert_eq!((value["plan"].as_str(), value["cancel_at"].as_str()), (Some("individual-goat"), Some("2026-11-02T08:44:37.000Z")));
        assert!(summarize(None, None, None).is_none());
    }
}

/// 手动核对：`cargo test -- --ignored goat_live` 用本机真实配置查一次，只打印额度概况（不含 Key）。
#[cfg(test)]
#[test]
#[ignore]
fn goat_live() {
    for (pool, value) in overview() {
        println!("{pool} → {value}");
    }
}

