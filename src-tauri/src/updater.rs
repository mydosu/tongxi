//! 同席自己的更新：查 GitHub 最新 Release，下载那一版的 exe，按同一版 manifest 核对大小和 sha256，
//! 把正在运行的 exe 改名让位、新的放进来，等本进程退出后再拉起新版。
//! 发布物是单个免安装 exe（不是安装包），所以不用 Tauri 官方 updater。
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

type Result<T> = std::result::Result<T, String>;

const LATEST: &str = "https://api.github.com/repos/mydosu/tongxi/releases/latest";

#[derive(Clone, Serialize, Deserialize)]
pub struct Update {
    pub current: String,
    pub version: String,
    pub notes: String,
    pub url: String,
    exe: String,
    manifest: String,
}

fn version(text: &str) -> Option<(u64, u64, u64)> {
    let mut parts = text.trim().trim_start_matches('v').split('.').map(|part| part.parse().ok());
    Some((parts.next()??, parts.next()??, parts.next().flatten().unwrap_or(0)))
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(10)).timeout(Duration::from_secs(300)).build()
}

fn get(url: &str) -> Result<ureq::Response> {
    agent()
        .get(url)
        .set("User-Agent", concat!("tongxi/", env!("CARGO_PKG_VERSION")))
        .call()
        .map_err(|error| format!("连不上 GitHub：{error}"))
}

fn newer(release: &serde_json::Value, current: &str) -> Option<Update> {
    let tag = release["tag_name"].as_str()?;
    if version(tag)? <= version(current)? {
        return None;
    }
    let bare = tag.trim_start_matches('v');
    let asset = |name: String| {
        release["assets"].as_array()?.iter().find(|asset| asset["name"] == name.as_str())?["browser_download_url"].as_str().map(String::from)
    };
    Some(Update {
        current: current.into(),
        version: bare.into(),
        notes: release["body"].as_str().unwrap_or("").chars().take(2000).collect(),
        url: release["html_url"].as_str().unwrap_or("").into(),
        exe: asset(format!("Agent-Hub-v{bare}.exe"))?,
        manifest: asset(format!("manifest-v{bare}.json"))?,
    })
}

#[tauri::command]
pub async fn check_app_update() -> Result<Option<Update>> {
    tauri::async_runtime::spawn_blocking(|| {
        let release: serde_json::Value = get(LATEST)?.into_json().map_err(|_| "GitHub 返回的发布信息读不出来".to_string())?;
        Ok(newer(&release, env!("CARGO_PKG_VERSION")))
    })
    .await
    .map_err(|_| "检查更新异常".to_string())?
}

fn old_copy(exe: &Path) -> PathBuf {
    exe.with_extension("exe.old")
}

/// 启动时清掉上次更新留下的旧 exe（那时它还在运行，删不掉）。
pub fn clean_previous() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(old_copy(&exe));
    }
}

/// 正在运行的 exe 改名让位，新的放进来；放不进去就把旧的改回来。
fn swap(exe: &Path, fresh: &Path) -> Result<()> {
    let old = old_copy(exe);
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old).map_err(|error| format!("当前程序让不了位：{error}"))?;
    if let Err(error) = std::fs::rename(fresh, exe) {
        let _ = std::fs::rename(&old, exe);
        return Err(format!("新版放不进去，已恢复原程序：{error}"));
    }
    Ok(())
}

fn download(update: &Update, exe: &Path) -> Result<PathBuf> {
    let manifest: serde_json::Value = get(&update.manifest)?.into_json().map_err(|_| "manifest 读不出来".to_string())?;
    if manifest["version"].as_str() != Some(update.version.as_str()) {
        return Err("manifest 的版本号和发布不一致，不更新".into());
    }
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut get(&update.exe)?.into_reader(), &mut bytes).map_err(|error| format!("下载中断：{error}"))?;
    // shortcut: 只防下载损坏，校验值和 exe 来自同一个 Release；要防发布被篡改得另加签名。
    let hash = format!("{:x}", Sha256::digest(&bytes));
    if Some(bytes.len() as u64) != manifest["bytes"].as_u64() || Some(hash.as_str()) != manifest["sha256"].as_str() {
        return Err("下载的文件和 manifest 对不上（大小或 sha256），没有替换".into());
    }
    let fresh = exe.with_extension("exe.new");
    std::fs::write(&fresh, bytes).map_err(|error| format!("新版写不进程序目录：{error}"))?;
    Ok(fresh)
}

#[tauri::command]
pub async fn apply_app_update(app: tauri::AppHandle, state: tauri::State<'_, crate::AppState>, update: Update) -> Result<()> {
    if cfg!(debug_assertions) {
        return Err("测试版不自我替换，请用正式版更新".into());
    }
    let busy: (i64, i64) = state.store.lock().unwrap().connection.query_row(
        "SELECT (SELECT COUNT(*) FROM runs WHERE status IN ('starting','running','cancelling')),(SELECT COUNT(*) FROM brain_jobs WHERE status IN ('pending','running'))",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).map_err(|error| error.to_string())?;
    if busy != (0, 0) {
        return Err(format!("还有 {} 个回复、{} 件主脑的活没结束，等它们结束再更新", busy.0, busy.1));
    }
    let prefix = "https://github.com/mydosu/tongxi/releases/download/";
    if !update.exe.starts_with(prefix) || !update.manifest.starts_with(prefix) {
        return Err("更新地址不是同席的 GitHub Release".into());
    }
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let target = exe.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let fresh = download(&update, &target)?;
        swap(&target, &fresh)
    })
    .await
    .map_err(|_| "更新任务异常".to_string())??;
    // 单实例：新版要等本进程退出后再起，不然会被当成第二个实例直接退掉。
    let script = format!(
        "Wait-Process -Id {} -ErrorAction SilentlyContinue; Start-Process -FilePath '{}'",
        std::process::id(),
        exe.to_string_lossy().replace('\'', "''")
    );
    let mut command = std::process::Command::new("powershell.exe");
    command.args(["-NoLogo", "-NoProfile", "-WindowStyle", "Hidden", "-Command", &script]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    command.spawn().map_err(|error| format!("新版已就位，但没能安排重启，请手动重新打开同席：{error}"))?;
    app.exit(0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_a_newer_release_with_both_assets_counts() {
        let release = |tag: &str| json!({"tag_name":tag,"html_url":"u","body":"b","assets":[
            {"name":format!("Agent-Hub-{tag}.exe"),"browser_download_url":"e"},
            {"name":format!("manifest-{tag}.json"),"browser_download_url":"m"}]});
        assert_eq!(newer(&release("v3.4.0"), "3.3.4").map(|update| update.version), Some("3.4.0".into()));
        assert!(newer(&release("v3.10.0"), "3.9.9").is_some(), "按数字比，不按字符串");
        assert!(newer(&release("v3.3.4"), "3.3.4").is_none());
        assert!(newer(&release("v3.3.3"), "3.3.4").is_none());
        assert!(newer(&json!({"tag_name":"v9.0.0","assets":[]}), "3.3.4").is_none(), "缺附件不提示");
    }

    /// 真去 GitHub 下最新 Release 并按 manifest 核对：`cargo test -- --ignored release_download`。
    #[test]
    #[ignore]
    fn release_download_matches_its_manifest() {
        let release: serde_json::Value = get(LATEST).unwrap().into_json().unwrap();
        let update = newer(&release, "0.0.1").expect("最新 Release 缺 exe 或 manifest");
        let root = std::env::temp_dir().join(format!("tongxi-download-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let fresh = download(&update, &root.join("Agent Hub.exe")).unwrap();
        assert!(std::fs::metadata(&fresh).unwrap().len() > 1_000_000);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn swap_puts_the_new_exe_in_place_and_keeps_the_old_one_aside() {
        let root = std::env::temp_dir().join(format!("tongxi-update-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let exe = root.join("Agent Hub.exe");
        std::fs::write(&exe, "old").unwrap();
        let fresh = root.join("Agent Hub.exe.new");
        std::fs::write(&fresh, "new").unwrap();
        swap(&exe, &fresh).unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(old_copy(&exe)).unwrap(), "old");
        assert!(swap(&exe, &root.join("missing")).is_err());
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "new", "放不进去时恢复原程序");
        std::fs::remove_dir_all(root).unwrap();
    }
}
