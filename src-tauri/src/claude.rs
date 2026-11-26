//! Claude Code 成员：复用本机安装的 `claude` CLI，经 `claude_bridge.mjs` 以 ACP 接入共用的运行时（hermes.rs）。
//! 原生会话保存在用户自己的 ~/.claude 中，同席只记录会话 ID 与工作目录。
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

pub const AGENT_ID: &str = "claude-win";
const NATIVE_EXE: &str = "node_modules/@anthropic-ai/claude-code/bin/claude.exe";

/// 状态身份：和别的 ACP 成员共用运行时类型，按这层包装区分；方法经 Deref 直接用。
pub struct Runtime(Arc<crate::hermes::Runtime>);
impl Runtime {
    pub fn new(app: tauri::AppHandle, store: Arc<Mutex<crate::store::Store>>, directory: PathBuf) -> Arc<Self> {
        Arc::new(Self(crate::hermes::Runtime::with_target(app, store, directory, crate::acp_transport::Target::Claude)))
    }
}
impl std::ops::Deref for Runtime {
    type Target = Arc<crate::hermes::Runtime>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

fn absolute_file(path: PathBuf) -> Option<PathBuf> {
    (path.is_absolute() && path.is_file()).then_some(path)
}

/// 原生 claude.exe：`AGENT_HUB_CLAUDE_EXE` 优先；否则 PATH 上的 claude.exe，
/// 或 npm 壳 claude.cmd 同级的包内原生程序；最后是官方安装器的 ~/.local/bin。
pub(crate) fn discover_executable() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("AGENT_HUB_CLAUDE_EXE").filter(|value| !value.is_empty())
    {
        return absolute_file(PathBuf::from(value));
    }
    crate::service_install::on_path(&["claude.exe"])
        .or_else(|| {
            crate::service_install::on_path_find(&["claude.cmd"], |shell| {
                absolute_file(shell.parent()?.join(NATIVE_EXE))
            })
        })
        .or_else(|| {
            let home = std::env::var_os("USERPROFILE")?;
            absolute_file(PathBuf::from(home).join(".local/bin/claude.exe"))
        })
}

/// `claude --version` 的首段，例如 `2.1.296 (Claude Code)` → `2.1.296`。
pub(crate) fn native_version(executable: &Path) -> Option<String> {
    let mut command = Command::new(executable);
    command.arg("--version");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let output = command.output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let version = text.split_whitespace().next()?;
    (output.status.success() && version.chars().next()?.is_ascii_digit())
        .then(|| version.to_owned())
}

/// Node 运行时：`AGENT_HUB_CLAUDE_NODE` 优先，否则与 DSH 同样从 PATH 查找。
pub(crate) fn discover_node() -> Option<PathBuf> {
    std::env::var_os("AGENT_HUB_CLAUDE_NODE")
        .filter(|value| !value.is_empty())
        .and_then(|value| absolute_file(PathBuf::from(value)))
        .or_else(crate::dsh::discover)
}

/// 同席若是从某个 Claude Code 里启动的，会带着它的 CLAUDE_CODE_* 标记；Claude 继承后会把自己
/// 当成嵌套子会话而不保存对话记录，原生会话就恢复不了，CLI 移交也导不回来。
pub(crate) fn nested_session_markers() -> Vec<std::ffi::OsString> {
    std::env::vars_os()
        .map(|(name, _)| name)
        .filter(|name| {
            let name = name.to_string_lossy().to_ascii_uppercase();
            name.starts_with("CLAUDE_CODE_") || name == "CLAUDECODE"
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn bridge_gives_members_the_full_cli() {
        let bridge = include_str!("claude_bridge.mjs");
        assert!(bridge.contains("else args.push('--permission-mode', 'bypassPermissions')"));
        assert!(bridge.contains("const denied = ['AskUserQuestion'"));
        // 宿主 Claude Code 的会话标记不能泄进成员子进程。
        assert!(bridge.contains("key === 'CLAUDECODE'"));
    }
}
