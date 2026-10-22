//! 各家订阅额度的已用比例（5 小时窗口与每周额度），交给调度按剩余额度分配工作、决定何时收尾。
//! 只读：Claude 读 Claude Code 登录留下的 OAuth 令牌查官方用量接口（不刷新令牌），
//! Codex 走已连上的 app-server 的 account/rateLimits/read。取不到就不给，不影响调度。

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 每家最多 5 分钟查一次；调度一回合接一回合时不重复联网。
const FRESH: Duration = Duration::from_secs(300);

fn cache() -> &'static Mutex<HashMap<&'static str, (Instant, Option<Value>)>> {
    static CACHE: OnceLock<Mutex<HashMap<&'static str, (Instant, Option<Value>)>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn cached(name: &'static str, fetch: impl FnOnce() -> Option<Value>) -> Option<Value> {
    if let Some((at, value)) = cache().lock().ok()?.get(name) {
        if at.elapsed() < FRESH {
            return value.clone();
        }
    }
    let value = fetch();
    // 查不到（令牌刚过期、接口 429 限流）只记 2 分钟，到点再试；查到的记 5 分钟。
    // 失败时间往前拨，让它比成功的早过期。
    let at = match &value {
        Some(_) => Some(Instant::now()),
        None => Instant::now().checked_sub(FRESH - RETRY),
    };
    if let (Ok(mut cache), Some(at)) = (cache().lock(), at) {
        cache.insert(name, (at, value.clone()));
    }
    value
}

const RETRY: Duration = Duration::from_secs(120);

/// 和 Claude 额度挂件共用的结果文件：`{"fetched_at": Unix 秒, "data": 接口原始返回}`。
/// 这个接口查得太勤会 429，谁查到了都写进来，另一边新鲜的就直接用。
fn shared_usage_path() -> Option<std::path::PathBuf> {
    Some(std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("claude-usage").join("usage.json"))
}

fn shared_usage(max_age: f64) -> Option<Value> {
    let cached: Value = serde_json::from_str(&std::fs::read_to_string(shared_usage_path()?).ok()?).ok()?;
    let age = crate::quota_reset::now_secs() as f64 - cached["fetched_at"].as_f64()?;
    (age < max_age).then(|| cached["data"].clone())
}

fn share_usage(data: &Value) {
    let Some(path) = shared_usage_path() else { return };
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let tmp = path.with_extension("json.tmp");
    let body = json!({"fetched_at": crate::quota_reset::now_secs(), "data": data});
    if std::fs::write(&tmp, body.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// 给调度的额度概况：{成员: {five_hour, weekly}}，每个窗口是已用百分比和距重置的小时数。
/// 走 Command Code（GOAT）的线路按「成员:线路」另列，多给剩余额度、本月用量和本期消耗。
pub fn overview(codex: &Arc<crate::codex::Runtime>) -> Value {
    let mut members = serde_json::Map::new();
    if let Some(value) = magpie_member("claude").or_else(|| cached("claude-win", claude)) {
        members.insert("claude-win".into(), value);
    }
    if let Some(value) = magpie_member("codex").or_else(|| cached("codex-win", || codex_limits(codex))) {
        members.insert("codex-win".into(), value);
    }
    members.extend(crate::goat::overview());
    Value::Object(members)
}

fn window(used: Option<f64>, resets_at: Option<i64>) -> Option<Value> {
    let used = used?;
    let mut value = json!({"used_percent": used.round()});
    if let Some(at) = resets_at {
        let hours = (at - crate::quota_reset::now_secs()) as f64 / 3600.0;
        value["resets_in_hours"] = json!((hours.max(0.0) * 10.0).round() / 10.0);
    }
    Some(value)
}

fn claude() -> Option<Value> {
    let data = shared_usage(600.0).or_else(claude_fetch)?;
    let pick = |key: &str| {
        window(
            data[key]["utilization"].as_f64(),
            data[key]["resets_at"].as_str().and_then(iso_secs),
        )
    };
    Some(json!({"five_hour": pick("five_hour"), "weekly": pick("seven_day")}))
}

fn claude_fetch() -> Option<Value> {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    let path = std::path::Path::new(&home).join(".claude").join(".credentials.json");
    let credentials: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let oauth = &credentials["claudeAiOauth"];
    let token = oauth["accessToken"].as_str()?;
    if oauth["expiresAt"].as_i64().is_some_and(|at| at / 1000 < crate::quota_reset::now_secs()) {
        return None;
    }
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
        .try_proxy_from_env(true)
        .build();
    let data: Value = agent
        .get("https://api.anthropic.com/api/oauth/usage")
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-beta", "oauth-2025-04-20")
        .call()
        .ok()?
        .into_json()
        .ok()?;
    share_usage(&data);
    Some(data)
}

/// 用户用 magpie 统一管各家订阅时，额度以它为准：一家可能挂着好几个账号，magpie 会在账号之间切换，
/// 官方接口只看得到当前登录的那一个。返回当前在用账号的窗口；有多个账号时另列 accounts（不含邮箱）。
fn magpie_member(agent: &str) -> Option<Value> {
    let accounts = cached("magpie", magpie_accounts)?;
    let mine: Vec<&Value> = accounts
        .as_array()?
        .iter()
        .filter(|account| account["agent"] == agent && account["on"] != false)
        .collect();
    let current = mine.iter().find(|account| account["active"] == true).or(mine.first())?;
    let windows = |account: &Value| {
        let mut value = json!({});
        for item in account["windows"].as_array().into_iter().flatten() {
            let name = match item["name"].as_str() {
                Some("5 hours") => "five_hour",
                Some("7 days") => "weekly",
                _ => continue,
            };
            if let Some(window) = window(item["used"].as_f64(), item["resetsAt"].as_str().and_then(iso_secs)) {
                value[name] = window;
            }
        }
        value
    };
    let mut value = windows(current);
    if value.as_object().is_some_and(|map| map.is_empty()) {
        return None;
    }
    if mine.len() > 1 {
        value["accounts"] = mine
            .iter()
            .map(|account| {
                let mut entry = windows(account);
                entry["plan"] = account["plan"].clone();
                entry["active"] = json!(account["active"] == true);
                entry
            })
            .collect();
        value["note"] = json!("magpie 管着这家的多个账号，当前账号用满时会切到别的账号；看总余量要把 accounts 合起来算");
    }
    Some(value)
}

fn magpie_accounts() -> Option<Value> {
    let mut command = std::process::Command::new(magpie_exe()?);
    command
        .args(["accounts", "--json"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = command.spawn().ok()?;
    let started = Instant::now();
    while child.try_wait().ok()?.is_none() {
        if started.elapsed() > Duration::from_secs(20) {
            let _ = child.kill();
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let mut text = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut text).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value.is_array().then_some(value)
}

/// magpie 可执行文件：MAGPIE_BIN 指定的，PATH 里的，或者正在运行的那个（它常不在 PATH 里）。
fn magpie_exe() -> Option<std::path::PathBuf> {
    static FOUND: OnceLock<Mutex<Option<std::path::PathBuf>>> = OnceLock::new();
    let found = FOUND.get_or_init(Default::default);
    if let Some(path) = found.lock().ok()?.clone().filter(|path| path.exists()) {
        return Some(path);
    }
    let path = std::env::var_os("MAGPIE_BIN")
        .map(std::path::PathBuf::from)
        .filter(|path| path.exists())
        .or_else(|| {
            let names = ["magpie.exe", "magpie-windows-amd64.exe", "magpie"];
            std::env::split_paths(&std::env::var_os("PATH")?)
                .flat_map(|dir| names.map(|name| dir.join(name)))
                .find(|path| path.is_file())
        })
        .or_else(running_magpie)?;
    *found.lock().ok()? = Some(path.clone());
    Some(path)
}

#[cfg(windows)]
fn running_magpie() -> Option<std::path::PathBuf> {
    use std::os::windows::process::CommandExt;
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", "(Get-Process magpie* -ErrorAction SilentlyContinue | Where-Object Path | Select-Object -First 1).Path"])
        .creation_flags(0x0800_0000)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let path = std::path::PathBuf::from(text);
    path.is_file().then_some(path)
}

#[cfg(not(windows))]
fn running_magpie() -> Option<std::path::PathBuf> {
    None
}

fn codex_limits(codex: &Arc<crate::codex::Runtime>) -> Option<Value> {
    let client = codex.shared_project_client().ok()?;
    let reply = client
        .rpc_timeout("account/rateLimits/read", Value::Null, Duration::from_secs(15))
        .ok()?;
    let limits = &reply["rateLimits"];
    let mut value = json!({});
    for key in ["primary", "secondary"] {
        let item = &limits[key];
        let name = match item["windowDurationMins"].as_i64() {
            Some(300) => "five_hour",
            Some(10080) => "weekly",
            _ => continue,
        };
        if let Some(window) = window(item["usedPercent"].as_f64(), item["resetsAt"].as_i64()) {
            value[name] = window;
        }
    }
    value.as_object().is_some_and(|map| !map.is_empty()).then_some(value)
}

/// "2026-10-16T21:59:59.6+00:00" → Unix 秒（接口给的是 UTC）。
fn iso_secs(text: &str) -> Option<i64> {
    let number = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    // days_from_civil（Howard Hinnant）
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    // 带时区的（magpie 给本地时间如 "-06:00"）换回 UTC；没写或写 Z 的按 UTC。
    let zone = text.get(19..).unwrap_or_default().trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    let offset = match zone.as_bytes().first() {
        Some(sign @ (b'+' | b'-')) => {
            let minutes = zone.get(1..3)?.parse::<i64>().ok()? * 60 + zone.get(4..6)?.parse::<i64>().ok()?;
            if *sign == b'+' { minutes * 60 } else { -minutes * 60 }
        }
        _ => 0,
    };
    Some(days * 86_400 + hour * 3600 + minute * 60 + second - offset)
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_utc_timestamps() {
        assert_eq!(super::iso_secs("1970-01-02T00:00:01+00:00"), Some(86_401));
        assert_eq!(
            super::iso_secs("2026-10-16T21:59:59.608858+00:00").map(crate::quota_reset::iso_utc).as_deref(),
            Some("2026-10-16T21:59:59Z")
        );
        assert_eq!(super::iso_secs("bad"), None);
        assert_eq!(super::iso_secs("1970-01-01T18:00:01-06:00"), Some(86_401));
        assert_eq!(super::iso_secs("1970-01-02T00:00:01.5Z"), Some(86_401));
    }
}

/// 手动核对：`cargo test -- --ignored quota_live --nocapture` 打印 Claude 额度查询结果（不含令牌）。
#[cfg(test)]
#[test]
#[ignore]
fn quota_live() {
    println!("claude → {:?}", claude());
    println!("magpie claude → {:?}", magpie_member("claude"));
    println!("magpie codex → {:?}", magpie_member("codex"));
}
