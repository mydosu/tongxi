//! 「导出诊断」：成员连接与版本、最近失败的回复和主脑任务、本机环境，脱敏后写成一个文本文件给用户转交。
//! 同席自己不写日志文件，失败原因都记在库里，所以从库里取。
use std::path::PathBuf;

/// 已在同席里实际跑过的 CLI 版本（按「主版本.次版本」比）；不在表里的提示可能不稳。
/// 升级验证过一个新版本就往这里加。
const VERIFIED: [(&str, &[&str]); 5] = [
    ("codex-win", &["0.164"]),
    ("claude-win", &["2.1"]),
    ("hermes-win", &["0.21"]),
    ("dsh-win", &["0.2"]),
    ("opencode-win", &["1.18"]),
];

/// None：没测到版本或这位成员不在表里（如陪伴）。
pub fn verified(agent: &str, version: Option<&str>) -> Option<bool> {
    let (_, known) = VERIFIED.iter().find(|(id, _)| *id == agent)?;
    let version = version?.trim().trim_start_matches('v');
    let minor: String = version.splitn(3, '.').take(2).collect::<Vec<_>>().join(".");
    Some(known.contains(&minor.as_str()))
}

/// 去掉用户名、家目录、邮箱和像密钥的串。
fn redact(text: &str) -> String {
    let mut text = text.to_owned();
    if let Some(home) = std::env::var_os("USERPROFILE").map(|home| home.to_string_lossy().into_owned()) {
        for form in [home.clone(), home.replace('\\', "/")] {
            text = text.replace(&form, "%USERPROFILE%");
        }
    }
    if let Some(user) = std::env::var("USERNAME").ok().filter(|user| user.len() >= 3) {
        text = text.replace(&user, "<user>");
    }
    let words: Vec<String> = text
        .split_inclusive(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '(' | ')' | '<' | '>' | ',' | '，' | '：'))
        .map(|word| {
            let core = word.trim_end_matches(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '(' | ')' | '<' | '>' | ',' | '，' | '：'));
            let tail = &word[core.len()..];
            // 路径里的 `@scope/包名` 不是邮箱。
            let email = !core.contains(['/', '\\']) && core.split_once('@').is_some_and(|(name, domain)| !name.is_empty() && domain.contains('.'));
            let key = ["sk-", "ghp_", "gho_", "github_pat_", "AKIA", "xox", "eyJ"].iter().any(|prefix| core.contains(prefix) && core.len() >= 20)
                || (core.len() >= 32 && core.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '=' | '/' | '+')) && core.chars().any(|c| c.is_ascii_digit()) && !core.contains('/'));
            if email {
                format!("<email>{tail}")
            } else if key {
                format!("<secret>{tail}")
            } else {
                word.to_owned()
            }
        })
        .collect();
    words.concat()
}

fn when(ms: i64) -> String {
    let seconds = ms / 1000;
    let (days, rest) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    // 公历换算（Howard Hinnant 的 civil_from_days），时间按 UTC。
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year}-{month:02}-{day:02} {:02}:{:02} UTC", rest / 3600, rest % 3600 / 60)
}

fn windows_version() -> String {
    let mut command = std::process::Command::new("cmd");
    command.args(["/c", "ver"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    // `ver` 的字是本地代码页（中文系统是 GBK），只取里面的版本号。
    let output = command.output().map(|output| output.stdout).unwrap_or_default();
    String::from_utf8_lossy(&output)
        .split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .find(|part| part.matches('.').count() >= 2)
        .map_or_else(|| std::env::consts::OS.into(), |version| format!("Windows {version}"))
}

fn report(app: &tauri::AppHandle, state: &crate::AppState) -> Result<String, String> {
    let mut out = format!(
        "同席诊断\n版本 v{} · {}\n系统 {} · {}\n数据目录 {}\n项目记忆 {}\n\n## 成员\n",
        env!("CARGO_PKG_VERSION"),
        when(crate::store::now()),
        windows_version(),
        std::env::consts::ARCH,
        state.directory.display(),
        if crate::project_memory::uses_openviking() { "OpenViking" } else { "项目内 markdown（.tongxi/记忆/）" },
    );
    for service in crate::services::service_inventory(app.clone()) {
        let version = service.runtime_version.as_deref().or(service.installed_version.as_deref());
        let check = match service.verified {
            Some(true) => "已验证版本",
            Some(false) => "未验证的版本，可能不稳",
            None => "版本未知",
        };
        out += &format!(
            "- {}（{}）：{} · 运行 {} · 安装 {} · {} · {}\n  程序 {}\n",
            service.name,
            service.id,
            service.connection,
            service.runtime_version.as_deref().unwrap_or("-"),
            service.installed_version.as_deref().unwrap_or("-"),
            if version.is_some() { check } else { "版本未知" },
            if service.busy { "忙" } else { "空闲" },
            service.executable.as_deref().unwrap_or("-"),
        );
    }
    let store = state.store.lock().unwrap();
    let since = crate::store::now() - 7 * 86_400_000;
    let rows = |sql: &str| -> Result<Vec<(i64, String, String)>, String> {
        let mut statement = store.connection.prepare(sql).map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([since], |row| Ok((row.get(0)?, row.get(1)?, row.get::<_, Option<String>>(2)?.unwrap_or_default())))
            .map_err(|error| error.to_string())?
            .filter_map(Result::ok)
            .collect();
        Ok(rows)
    };
    let runs = rows("SELECT m.created_at,r.agent_id,r.error FROM runs r JOIN messages m ON m.id=r.assistant_message_id WHERE r.status='failed' AND m.created_at>=?1 ORDER BY m.created_at DESC LIMIT 40")?;
    out += &format!("\n## 最近 7 天失败的回复（{} 条，最多列 40 条）\n", runs.len());
    for (at, agent, error) in runs {
        out += &format!("- {} · {} · {}\n", when(at), agent, error.chars().take(400).collect::<String>().replace('\n', " "));
    }
    let jobs = rows("SELECT created_at,seat,error FROM brain_jobs WHERE status='failed' AND created_at>=?1 ORDER BY created_at DESC LIMIT 40")?;
    out += &format!("\n## 最近 7 天失败的主脑任务（{} 条）\n", jobs.len());
    for (at, seat, error) in jobs {
        let error = error.chars().take(400).collect::<String>().replace('\n', " ");
        out += &format!("- {} · {} · {}\n", when(at), seat, if error.trim().is_empty() { "（没有记录原因）" } else { &error });
    }
    let counts: (i64, i64, i64) = store
        .connection
        .query_row(
            "SELECT (SELECT COUNT(*) FROM conversations),(SELECT COUNT(*) FROM projects),(SELECT COUNT(*) FROM runs WHERE status IN ('starting','running','cancelling'))",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|error| error.to_string())?;
    out += &format!("\n## 概况\n会话 {} · 项目 {} · 进行中的回复 {}\n", counts.0, counts.1, counts.2);
    Ok(redact(&out))
}

/// 写到「下载」文件夹并在资源管理器里选中它；返回文件路径。不含会话内容，只有失败原因这类元信息。
#[tauri::command]
pub async fn export_diagnostics(app: tauri::AppHandle) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = tauri::Manager::state::<crate::AppState>(&app);
        let text = report(&app, &state)?;
        let folder = std::env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join("Downloads")).filter(|path| path.is_dir()).unwrap_or_else(std::env::temp_dir);
        let stamp = when(crate::store::now()).replace([' ', ':'], "-").replace("-UTC", "");
        let path = folder.join(format!("同席诊断-{stamp}.txt"));
        std::fs::write(&path, text).map_err(|error| format!("诊断文件写不进去：{error}"))?;
        let _ = std::process::Command::new("explorer.exe").arg(format!("/select,{}", path.display())).spawn();
        Ok(path.to_string_lossy().into_owned())
    })
    .await
    .map_err(|_| "导出诊断异常".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_major_minor() {
        assert_eq!(verified("claude-win", Some("2.1.310")), Some(true));
        assert_eq!(verified("codex-win", Some("0.170.0")), Some(false));
        assert_eq!(verified("dsh-win", Some("v0.2.1-rc.2")), Some(true));
        assert_eq!(verified("albion-wsl", Some("1.0")), None);
        assert_eq!(verified("codex-win", None), None);
    }

    #[test]
    fn redaction_hides_keys_and_mail_but_keeps_errors_readable() {
        let text = redact("HTTP 401：key sk-abcdefghijklmnopqrstuvwxyz0123 无效，联系 someone@example.com，提交 0123456789abcdef0123456789abcdef");
        assert!(!text.contains("sk-abc") && !text.contains("someone@") && !text.contains("0123456789abcdef0123"), "{text}");
        assert!(text.contains("HTTP 401") && text.contains("无效"));
        assert!(redact(r"D:\npm\node_modules\@openai/codex.exe").contains("@openai/codex.exe"), "路径不是邮箱");
        assert_eq!(redact("Billing or credits exhausted: HTTP 400"), "Billing or credits exhausted: HTTP 400");
        assert_eq!(when(0), "1970-01-01 00:00 UTC");
        assert_eq!(when(1_792_829_803_547), "2026-10-24 08:16 UTC");
    }
}
