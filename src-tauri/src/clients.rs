//! 在同席里一键打开各成员自己的客户端。
//!
//! 用户说明：Hermes 是那个桌面端 exe；codex 是命令行里敲 `codex`；DSH 是它的网页端
//! （`dsh web`，地址形如 http://127.0.0.1:3080/?token=…，token 每次启动才生成、不落盘）；
//! WSL 里的阿尔比恩不做。

use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const DSH_WEB_PORT: u16 = 3080;
const DSH_URL_FILE: &str = "dsh-web-url.txt";

fn setting(key: &str, fallback: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

fn local_app_data() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 数据目录：和主程序同一套约定（AGENT_HUB_DATA_DIR），只为存放 DSH 的网页地址。
fn data_dir() -> PathBuf {
    std::env::var_os("AGENT_HUB_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

#[cfg(windows)]
fn quiet(command: &mut Command) -> &mut Command {
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x08000000) // CREATE_NO_WINDOW：别闪黑框
}

#[cfg(not(windows))]
fn quiet(command: &mut Command) -> &mut Command {
    command
}

fn shell(program: &[&str]) -> Result<(), String> {
    let mut command = Command::new("cmd");
    command.args(program);
    quiet(&mut command);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("启动失败：{error}"))
}

fn listening(port: u16) -> bool {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&address, Duration::from_millis(400)).is_ok()
}

/// DSH 网页端：没在跑就帮它起来（`dsh web`），从它输出的地址里取到带 token 的那个再交给浏览器。
/// 起来过一次就把地址记住，以后直接开。
fn dsh_web() -> Result<String, String> {
    let url_file = data_dir().join(DSH_URL_FILE);
    if listening(DSH_WEB_PORT) {
        if let Ok(url) = std::fs::read_to_string(&url_file) {
            let url = url.trim().to_string();
            if url.starts_with("http://127.0.0.1") {
                shell(&["/c", "start", "", &url])?;
                return Ok("DSH 网页端已经在运行，已打开浏览器".into());
            }
        }
        return Err(
            "DSH 网页端已经在运行，但它带 token 的地址我没有记录；请从它自己的窗口打开一次。"
                .into(),
        );
    }

    let cli = match setting("AGENT_HUB_DSH_CLI", "") {
        value if !value.is_empty() => value,
        _ => crate::service_install::on_path(&["dsh.cmd", "dsh"])
            .map(|path| path.to_string_lossy().to_string())
            .ok_or(
                "没找到 DSH 启动脚本：PATH 上没有 dsh.cmd，也没设 AGENT_HUB_DSH_CLI".to_string(),
            )?,
    };
    if !Path::new(&cli).is_file() {
        return Err(format!("没找到 DSH 启动脚本：{cli}"));
    }
    let workdir = Path::new(&cli)
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut command = Command::new("cmd");
    command
        .args(["/c", &cli, "web", "--no-open"])
        .current_dir(&workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    quiet(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("启动 DSH 网页端失败：{error}"))?;
    let stdout = child.stdout.take().ok_or("读不到 DSH 网页端输出")?;
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        // 一直读下去：既拿到地址，也让服务进程的输出管道不会堵住。
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if sender.send(line).is_err() {
                return;
            }
        }
    });
    drop(child);

    let deadline = std::time::Instant::now() + Duration::from_secs(40);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err("DSH 网页端 40 秒内没给出地址，请稍后重试".into());
        }
        match receiver.recv_timeout(remaining) {
            Ok(line) => {
                if let Some(index) = line.find("http://127.0.0.1") {
                    let url = line[index..].trim().to_string();
                    let _ = std::fs::write(&url_file, &url);
                    shell(&["/c", "start", "", &url])?;
                    return Ok("已启动 DSH 网页端并打开浏览器".into());
                }
            }
            Err(_) => return Err("DSH 网页端 40 秒内没给出地址，请稍后重试".into()),
        }
    }
}

/// 打开成员自己的客户端。返回一句话给界面提示。
pub fn open(agent_id: &str) -> Result<String, String> {
    match agent_id {
        "hermes-win" => {
            let fallback = local_app_data()
                .join("hermes/hermes-agent/apps/desktop/release/win-unpacked/Hermes.exe");
            let exe = PathBuf::from(setting(
                "AGENT_HUB_HERMES_DESKTOP",
                &fallback.to_string_lossy(),
            ));
            if !exe.is_file() {
                return Err(format!("没找到 Hermes 桌面端：{}", exe.display()));
            }
            let mut command = Command::new(&exe);
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            command
                .spawn()
                .map_err(|error| format!("启动 Hermes 桌面端失败：{error}"))?;
            Ok("已打开 Hermes 桌面端".into())
        }
        "codex-win" => {
            let cli = setting("AGENT_HUB_CODEX_CLI", "codex");
            // 开着窗口进 CLI：和用户手动 `cmd` 里敲 codex 一样，退出 CLI 后窗口还在。
            shell(&[
                "/c",
                "start",
                "",
                "cmd",
                "/k",
                &format!("title 同席 · codex & {cli}"),
            ])?;
            Ok("已打开 codex（命令行）".into())
        }
        "dsh-win" => dsh_web(),
        "albion-wsl" => Err("阿尔比恩没有独立的桌面客户端，这里不做".into()),
        _ => Err("成员不存在".into()),
    }
}
