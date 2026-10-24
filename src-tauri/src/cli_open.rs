//! “在 CLI 中打开此会话”：把这条原生会话移交给该成员自己的 CLI。
//! 移交期间同席暂停使用它（不再往这条会话发消息），先从桥/app-server 里卸下已载入的会话，
//! 让 CLI 独占并续上同一条会话；CLI 窗口关闭后自动收回，同席下次发消息时从会话库重新载入，
//! 接上 CLI 里新说的内容。同席与 CLI 共用会话库（Codex/Claude 的用户目录、Hermes 的 state.db、DSH 的 ~/.dsh）。
use crate::store::Store;
use std::path::{Path, PathBuf};
use std::process::Command;

type Result<T> = std::result::Result<T, String>;

/// 会话 ID 只允许这些字符，拼进命令行前先校验，杜绝注入。
fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 200
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
}

/// PowerShell 单引号字符串：内部的单引号写两次。
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// 各 agent 续接会话的 CLI 命令（PowerShell 语法）。
pub(crate) fn resume_command(
    agent: &str,
    session: &str,
    executable: Option<&Path>,
) -> Result<String> {
    if !safe_id(session) {
        return Err("原生会话 ID 格式异常，无法在 CLI 中打开".into());
    }
    let program = |name: &str| {
        executable.map_or_else(
            || name.to_owned(),
            |path| format!("& {}", quote(&path.to_string_lossy())),
        )
    };
    Ok(match agent {
        "codex-win" => format!("{} resume {session}", program("codex")),
        "claude-win" => format!("{} --resume {session}", program("claude")),
        "hermes-win" => format!("{} --resume {session}", program("hermes")),
        "opencode-win" => format!("{} --session {session}", program("opencode")),
        // DSH 在这台机器上没有终端界面，只有网页版：开网页版，再从会话历史里选这条会话。
        "dsh-win" => format!(
            "Write-Host '在打开的 DSH 网页里，从会话历史中选择会话 {session}；关闭本窗口即回到同席。'; {} --profile web",
            program("dsh")
        ),
        _ => return Err("陪伴通过它自己的服务对话，没有 CLI 可以打开".into()),
    })
}

fn workspace_key(agent: &str) -> &'static str {
    match agent {
        "codex-win" => "codex",
        "claude-win" => "claude",
        "dsh-win" => "dsh",
        _ => "hermes",
    }
}

fn shell() -> &'static str {
    let found = |name: &str| {
        std::env::var_os("PATH")
            .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
    };
    if found("pwsh.exe") {
        "pwsh.exe"
    } else {
        "powershell.exe"
    }
}

/// 要交给 CLI 的那条原生会话。
pub(crate) struct Target {
    pub native: String,
    pub cwd: PathBuf,
    pub title: String,
}

/// 查出原生会话并登记移交；登记后同席不再往这条会话发消息。
pub(crate) fn begin(
    store: &mut Store,
    directory: &Path,
    conversation_id: &str,
    agent: &str,
) -> Result<Target> {
    if store
        .cli_handoffs
        .contains(&(conversation_id.to_owned(), agent.to_owned()))
    {
        return Err("这条会话已经在 CLI 中打开".into());
    }
    let detail = store.detail(conversation_id)?;
    let session = detail
        .sessions
        .into_iter()
        .find(|session| session.agent_id == agent)
        .ok_or("这个会话里没有该成员")?;
    let native = session
        .native_session_id
        .ok_or("这个会话还没有原生会话，先在同席里发一条消息再打开")?;
    resume_command(agent, &native, None)?;
    let cwd = session.native_cwd.map(PathBuf::from).unwrap_or_else(|| {
        directory
            .join(format!("{}-workspaces", workspace_key(agent)))
            .join(conversation_id)
    });
    std::fs::create_dir_all(&cwd).map_err(|_| "会话工作目录不可用")?;
    // 记下移交时刻：收回时把这之后 CLI 里新增的对话补进同席。进程号等窗口打开后再填。
    store.save_cli_handoff(conversation_id, agent, &native, 0, crate::store::now())?;
    Ok(Target {
        native,
        cwd,
        title: format!("同席 · {}", detail.conversation.title),
    })
}

/// CLI 所在的终端窗口（其中 PowerShell 的进程）。
pub(crate) struct Window {
    handle: windows_sys::Win32::Foundation::HANDLE,
}
// 进程句柄只用来等待退出，可以跨线程交给等待线程。
unsafe impl Send for Window {}

impl Window {
    /// 同席重启后重新认领还开着的 CLI 窗口：进程还在且确实是 PowerShell 才算（防进程号被复用）。
    pub(crate) fn attach(pid: u32) -> Option<Window> {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
            PROCESS_SYNCHRONIZE,
        };
        if pid == 0 {
            return None;
        }
        let handle = unsafe {
            OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                pid,
            )
        };
        if handle.is_null() {
            return None;
        }
        let mut name = [0u16; 1024];
        let mut length = name.len() as u32;
        let ok = unsafe { QueryFullProcessImageNameW(handle, 0, name.as_mut_ptr(), &mut length) };
        let image = String::from_utf16_lossy(&name[..length as usize]).to_lowercase();
        if ok == 0 || !(image.ends_with("pwsh.exe") || image.ends_with("powershell.exe")) {
            unsafe { CloseHandle(handle) };
            return None;
        }
        Some(Window { handle })
    }
    /// 等到窗口里的 PowerShell 退出（CLI 结束或窗口被关）。
    pub(crate) fn wait(self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{WaitForSingleObject, INFINITE};
        unsafe {
            WaitForSingleObject(self.handle, INFINITE);
            CloseHandle(self.handle);
        }
    }
}

/// 打开独立的终端窗口运行 CLI。CLI 退出时窗口随之关闭（出错时先停住让人看清），
/// 同席据此自动收回会话。直接用 CreateProcessW 且不继承任何句柄：同席自己的标准输出
/// 可能被重定向到文件，继承下去 CLI 会以为没有终端而直接退出。
pub(crate) fn launch(
    target: &Target,
    agent: &str,
    executable: Option<&Path>,
    conversation_id: &str,
) -> Result<(Window, u32)> {
    use base64::Engine;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, CREATE_NEW_CONSOLE, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION,
        STARTUPINFOW,
    };
    let command = resume_command(agent, &target.native, executable)?;
    let script = format!(
        "$Host.UI.RawUI.WindowTitle = {}; Get-ChildItem Env: | Where-Object {{ $_.Name -like 'CLAUDE_CODE_*' -or $_.Name -eq 'CLAUDECODE' }} | Remove-Item; {command}; if ($LASTEXITCODE) {{ Read-Host 'CLI 异常退出，按回车关闭窗口并回到同席' }}",
        quote(&target.title)
    );
    let encoded = base64::engine::general_purpose::STANDARD.encode(
        script
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<u8>>(),
    );
    let wide = |text: &str| text.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let mut line = wide(&format!(
        "\"{}\" -NoLogo -EncodedCommand {encoded}",
        shell()
    ));
    let cwd = wide(&target.cwd.to_string_lossy());
    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let created = unsafe {
        CreateProcessW(
            std::ptr::null(),
            line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            CREATE_NEW_CONSOLE | CREATE_UNICODE_ENVIRONMENT,
            std::ptr::null(),
            cwd.as_ptr(),
            &startup,
            &mut info,
        )
    };
    if created == 0 {
        return Err("无法打开终端窗口".into());
    }
    unsafe { CloseHandle(info.hThread) };
    let _ = conversation_id;
    Ok((
        Window {
            handle: info.hProcess,
        },
        info.dwProcessId,
    ))
}

/// CLI 窗口关闭后收回会话：把移交期间 CLI 里新增的对话补进同席，再解除暂停。
/// 读原生会话库（DSH 还要解压）不占同席的存储锁。返回补进来的条数。
pub(crate) fn finish(store: &std::sync::Mutex<Store>, conversation_id: &str, agent: &str) -> usize {
    let saved = store
        .lock()
        .ok()
        .and_then(|store| store.saved_cli_handoffs().ok())
        .unwrap_or_default()
        .into_iter()
        .find(|row| row.0 == conversation_id && row.1 == agent);
    let turns = saved
        .and_then(|(_, _, native, _, since)| {
            crate::cli_transcript::since(agent, &native, since).ok()
        })
        .unwrap_or_default();
    let Ok(mut store) = store.lock() else {
        return 0;
    };
    let imported = if turns.is_empty() {
        0
    } else {
        store
            .import_cli_turns(conversation_id, agent, &turns)
            .unwrap_or(0)
    };
    let _ = store.clear_cli_handoff(conversation_id, agent);
    imported
}

/// 用户主动收回：结束 CLI 窗口（连同其中的 CLI 进程），等待线程随后完成收回。
pub(crate) fn reclaim(store: &Store, conversation_id: &str, agent: &str) -> Result<()> {
    let pid = store
        .saved_cli_handoffs()?
        .into_iter()
        .find(|row| row.0 == conversation_id && row.1 == agent)
        .map(|row| row.3)
        .filter(|pid| *pid != 0)
        .ok_or("这条会话没有在 CLI 中打开")?;
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|_| "无法结束 CLI 窗口")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::resume_command;
    use std::path::Path;

    #[test]
    fn builds_each_agents_resume_command() {
        assert_eq!(
            resume_command("codex-win", "019a-uuid", Some(Path::new(r"C:\x\codex.exe"))).unwrap(),
            r"& 'C:\x\codex.exe' resume 019a-uuid"
        );
        assert_eq!(
            resume_command("claude-win", "abc", None).unwrap(),
            "claude --resume abc"
        );
        assert_eq!(
            resume_command("hermes-win", "20261013_1", None).unwrap(),
            "hermes --resume 20261013_1"
        );
        assert_eq!(
            resume_command("dsh-win", "session-1", None).unwrap(),
            "Write-Host '在打开的 DSH 网页里，从会话历史中选择会话 session-1；关闭本窗口即回到同席。'; dsh --profile web"
        );
        assert!(resume_command("albion-wsl", "x", None).is_err());
    }

    #[test]
    fn rejects_session_ids_that_could_inject_commands() {
        assert!(resume_command("codex-win", "x; rm -rf /", None).is_err());
        assert!(resume_command("claude-win", "a'b", None).is_err());
        assert!(resume_command("dsh-win", "", None).is_err());
    }
}
