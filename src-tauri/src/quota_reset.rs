//! 从额度/限流报错里读出重置时间。只提取时间，不保留报错原文（原文可能带配置或凭据）。
//! 识别：桥接附带的 `resets_at=<秒>`、原生 JSON 里的 `resetsAt`/`resets_at` 数字（秒或毫秒）、
//! “try again in 4h 12m / in 3 days 2 hours / in 37 minutes”这类相对时长，以及
//! “try again at 5:52 PM / resets 5:50pm / resets at 17:50”这类本地钟点。

/// 重置时刻（Unix 秒）。`now` 为当前 Unix 秒，`offset` 为本地时区相对 UTC 的秒数。
pub fn reset_at(text: &str, now: i64, offset: i64) -> Option<i64> {
    let lower = text.to_lowercase();
    epoch(&lower)
        .or_else(|| relative(&lower).map(|secs| now + secs))
        .or_else(|| clock(&lower, now, offset))
        .filter(|at| *at > now - 60 && *at < now + 8 * 86_400)
}


/// 附在错误摘要后的机器可读标记，协调器据此等到重置。
pub fn marker(at: i64) -> String {
    let (offset, _) = local_offset();
    format!("（额度预计 {} 重置，resets_at={at}）", local_label(at, offset))
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// 本地时区偏移（秒）与当前 Unix 秒。
pub fn local_offset() -> (i64, i64) {
    let now = now_secs();
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::Storage::FileSystem::FileTimeToLocalFileTime;
        let ticks = (now + 11_644_473_600) as u64 * 10_000_000;
        let utc = FILETIME {
            dwLowDateTime: ticks as u32,
            dwHighDateTime: (ticks >> 32) as u32,
        };
        let mut local = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        if unsafe { FileTimeToLocalFileTime(&utc, &mut local) } != 0 {
            let value = (u64::from(local.dwHighDateTime) << 32) | u64::from(local.dwLowDateTime);
            let local_secs = (value / 10_000_000) as i64 - 11_644_473_600;
            return (local_secs - now, now);
        }
    }
    (0, now)
}

/// “10月13日 17:50”形式的本地时间。
pub fn local_label(at: i64, offset: i64) -> String {
    let local = at + offset;
    let days = local.div_euclid(86_400);
    let secs = local.rem_euclid(86_400);
    let (_, month, day) = civil(days);
    format!("{month}月{day}日 {:02}:{:02}", secs / 3600, secs % 3600 / 60)
}

/// Unix 秒 → ISO 8601（UTC），给界面的 `new Date()` 用。
pub fn iso_utc(secs: i64) -> String {
    let (year, month, day) = civil(secs.div_euclid(86_400));
    let rest = secs.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

fn digits_after(text: &str, key: &str) -> Option<i64> {
    let start = text.find(key)? + key.len();
    let rest = text[start..].trim_start_matches(|c: char| matches!(c, '"' | ':' | '=' | ' '));
    let number: String = rest.chars().take_while(char::is_ascii_digit).collect();
    number.parse().ok()
}

fn epoch(text: &str) -> Option<i64> {
    ["resets_at", "resetsat", "reset_at", "resetat"]
        .iter()
        .find_map(|key| digits_after(text, key))
        .map(|value| if value > 100_000_000_000 { value / 1000 } else { value })
}

fn relative(text: &str) -> Option<i64> {
    let start = ["try again in", "retry in", "resets in", "reset in", "available in"]
        .iter()
        .find_map(|key| text.find(key).map(|at| at + key.len()))?;
    let tail: String = text[start..].chars().take(60).collect();
    // 依次读“数字 + 单位”（4h 12m、3 days 2 hours、37 minutes），遇到不是这种结构的内容就停。
    let mut chars = tail.chars().peekable();
    let mut total = 0;
    let mut found = false;
    loop {
        while chars.peek().is_some_and(|c| *c == ' ' || *c == ',') {
            chars.next();
        }
        if found && chars.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
            let word: String = chars.clone().take_while(char::is_ascii_alphabetic).collect();
            if word == "and" {
                chars.nth(word.len() - 1);
                continue;
            }
        }
        let number: String = std::iter::from_fn(|| chars.next_if(char::is_ascii_digit)).collect();
        if number.is_empty() {
            break;
        }
        while chars.peek() == Some(&' ') {
            chars.next();
        }
        let unit: String = std::iter::from_fn(|| chars.next_if(char::is_ascii_alphabetic)).collect();
        let scale = match unit.chars().next() {
            Some('d') => 86_400,
            Some('h') => 3600,
            Some('m') => 60,
            Some('s') => 1,
            _ => break,
        };
        total += number.parse::<i64>().ok()? * scale;
        found = true;
    }
    found.then_some(total)
}

fn clock(text: &str, now: i64, offset: i64) -> Option<i64> {
    let start = ["try again at", "resets at", "resets", "reset at", "available at"]
        .iter()
        .find_map(|key| text.find(key).map(|at| at + key.len()))?;
    let tail: String = text[start..].chars().take(40).collect();
    let colon = tail.find(':')?;
    let hour: i64 = tail[..colon]
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>()
        .parse()
        .ok()?;
    let after = &tail[colon + 1..];
    let minute: i64 = after
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()?;
    let suffix = after.trim_start_matches(|c: char| c.is_ascii_digit()).trim_start();
    let hour = match (suffix.starts_with("pm"), suffix.starts_with("am")) {
        (true, _) if hour < 12 => hour + 12,
        (_, true) if hour == 12 => 0,
        _ => hour,
    };
    if hour > 23 || minute > 59 {
        return None;
    }
    let local_now = now + offset;
    let midnight = local_now - local_now.rem_euclid(86_400);
    let mut at = midnight + hour * 3600 + minute * 60 - offset;
    if at <= now {
        at += 86_400;
    }
    Some(at)
}

#[cfg(test)]
mod tests {
    use super::*;
    const NOW: i64 = 1_791_882_600; // 2026-10-13 09:10 UTC = 北京时间 17:10
    const CST: i64 = 8 * 3600;

    #[test]
    fn reads_bridge_marker_and_native_epochs() {
        assert_eq!(reset_at("…（额度预计 10月13日 22:50 重置，resets_at=1791903000）", NOW, CST), Some(1_791_903_000));
        assert_eq!(reset_at(r#"{"resetsAt": 1791903000000}"#, NOW, CST), Some(1_791_903_000));
    }


    #[test]
    fn reads_relative_durations() {
        assert_eq!(reset_at("You've hit your usage limit. Try again in 4h 12m.", NOW, CST), Some(NOW + 4 * 3600 + 12 * 60));
        assert_eq!(reset_at("try again in 1 day 2 hours 5 minutes", NOW, CST), Some(NOW + 86_400 + 2 * 3600 + 300));
        assert_eq!(reset_at("rate limited, retry in 37 minutes", NOW, CST), Some(NOW + 37 * 60));
    }

    #[test]
    fn reads_local_clock_times() {
        // NOW 是本地 17:10；5:50 PM 在今天，9:00 AM 已过则算明天。
        assert_eq!(reset_at("5-hour limit reached ∙ resets 5:50pm", NOW, CST), Some(NOW + 40 * 60));
        assert_eq!(reset_at("Try again at 9:00 AM", NOW, CST), Some(NOW + (24 * 60 - 8 * 60 - 10) * 60));
        assert_eq!(reset_at("resets at 17:50", NOW, CST), Some(NOW + 40 * 60));
    }

    #[test]
    fn ignores_text_without_times() {
        assert_eq!(reset_at("模型服务限流，请稍后继续当前任务", NOW, CST), None);
        assert_eq!(reset_at("HTTP 429 Too Many Requests", NOW, CST), None);
    }

    #[test]
    fn formats_iso_utc() {
        assert_eq!(iso_utc(1_791_903_000), "2026-10-13T14:50:00Z");
    }

    #[test]
    fn labels_local_time() {
        assert_eq!(local_label(1_791_903_000, CST), "10月13日 22:50");
    }
}
