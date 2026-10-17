//! 成员接入：扫描本机已装的 agent（不连接、不改任何配置），没装的打开终端跑官方安装命令，
//! 装好后在终端里做初次登录/配置。所有会改动本机的操作都在用户看得见的终端窗口里进行，
//! 命令原样显示在界面上，点了才执行。
use serde::Serialize;
use std::path::PathBuf;
use std::process::Command;

type Result<T> = std::result::Result<T, String>;

/// 一位成员在本机的安装情况。
#[derive(Clone, Debug, Serialize)]
pub struct AgentEnv {
    pub id: String,
    pub name: String,
    pub installed: bool,
    pub executable: Option<String>,
    pub version: Option<String>,
    /// 缺的前置条件（例如 Node.js 版本不够）；装的时候脚本会先补上。
    pub missing: Option<String>,
    /// 一键安装会执行的命令（原样展示给用户）。
    pub install_command: String,
    /// 初次登录/配置会执行的命令。
    pub setup_command: String,
    /// 官方安装说明。
    pub docs: String,
}

struct Recipe {
    id: &'static str,
    name: &'static str,
    install: &'static str,
    setup: &'static str,
    docs: &'static str,
    needs_node: Option<(u32, u32)>,
}

/// 官方安装与初次配置命令。Node 包（Codex、DSH）缺 Node 时先用 winget 装 LTS。
const RECIPES: [Recipe; 4] = [
    Recipe {
        id: "codex-win",
        name: "Codex",
        install: "npm install -g @openai/codex",
        setup: "codex login",
        docs: "https://github.com/openai/codex",
        needs_node: Some((18, 0)),
    },
    Recipe {
        id: "claude-win",
        name: "Claude",
        install: "irm https://claude.ai/install.ps1 | iex",
        setup: "claude",
        docs: "https://code.claude.com/docs/en/setup",
        needs_node: None,
    },
    Recipe {
        id: "hermes-win",
        name: "Hermes",
        install: "iex (irm https://hermes-agent.nousresearch.com/install.ps1)",
        setup: "hermes setup",
        docs: "https://hermes-agent.nousresearch.com/docs/user-guide/windows-native",
        needs_node: None,
    },
    Recipe {
        id: "dsh-win",
        name: "DSH",
        install: "npm install -g @deepseek-ai/dsh",
        setup: "dsh web",
        docs: "https://github.com/deepseek-ai/deepseek-harness",
        needs_node: Some((22, 19)),
    },
];

#[cfg(windows)]
fn quiet(command: &mut Command) -> &mut Command {
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0800_0000)
}
#[cfg(not(windows))]
fn quiet(command: &mut Command) -> &mut Command {
    command
}

/// 刚装好的程序写进了注册表里的 PATH，但同席这个进程还拿着启动时的旧 PATH：
/// 从系统与用户两级重新读一遍，把新增的目录补进本进程，扫描和连接才找得到。
pub(crate) fn refresh_path() {
    let mut command = Command::new("powershell.exe");
    command.args([
        "-NoLogo",
        "-NoProfile",
        "-Command",
        "[Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [Environment]::GetEnvironmentVariable('Path','User')",
    ]);
    let Ok(output) = quiet(&mut command).output() else {
        return;
    };
    let fresh = String::from_utf8_lossy(&output.stdout);
    let current = std::env::var_os("PATH").unwrap_or_default();
    let mut paths: Vec<PathBuf> = std::env::split_paths(&current).collect();
    for entry in fresh.trim().split(';').map(str::trim).filter(|entry| !entry.is_empty()) {
        let entry = PathBuf::from(entry);
        if !paths.iter().any(|known| known == &entry) {
            paths.push(entry);
        }
    }
    if let Ok(joined) = std::env::join_paths(paths) {
        std::env::set_var("PATH", joined);
    }
}

fn first_line(output: &[u8]) -> Option<String> {
    String::from_utf8_lossy(output)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(String::from)
}

/// `node --version` → (主版本, 次版本)。
fn node_version() -> Option<(u32, u32, String)> {
    let node = crate::dsh::discover()?;
    let mut command = Command::new(node);
    command.arg("--version");
    let text = first_line(&quiet(&mut command).output().ok()?.stdout)?;
    let mut parts = text.trim_start_matches('v').split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor, text))
}

fn detect(recipe: &Recipe, data: Option<&std::path::Path>) -> (Option<PathBuf>, Option<String>) {
    match recipe.id {
        "codex-win" => {
            let path = crate::codex::discover_executable();
            let version = path.as_ref().and_then(crate::codex::native_version);
            (path, version)
        }
        "claude-win" => {
            let path = crate::claude::discover_executable();
            let version = path.as_deref().and_then(crate::claude::native_version);
            (path, version)
        }
        "hermes-win" => (crate::acp_transport::Target::windows_python(), None),
        _ => {
            let package = data.and_then(|data| crate::service_install::dsh_path(data).ok());
            let package = package.filter(|path| path.join("package.json").is_file());
            let version = package.as_ref().and_then(|path| {
                let text = std::fs::read_to_string(path.join("package.json")).ok()?;
                serde_json::from_str::<serde_json::Value>(&text).ok()?["version"]
                    .as_str()
                    .map(String::from)
            });
            (package, version)
        }
    }
}

/// 扫一遍本机：先刷新 PATH，再按各成员自己的发现规则找安装位置。只读，不连接。
pub fn scan(data: Option<&std::path::Path>) -> Vec<AgentEnv> {
    refresh_path();
    let node = node_version();
    RECIPES
        .iter()
        .map(|recipe| {
            let (path, version) = detect(recipe, data);
            let missing = recipe.needs_node.and_then(|(major, minor)| match &node {
                None => Some(format!("需要 Node.js {major}.{minor}+（未安装，安装时会先用 winget 装 LTS）")),
                Some((have_major, have_minor, text)) if (*have_major, *have_minor) < (major, minor) => {
                    Some(format!("需要 Node.js {major}.{minor}+，当前 {text}"))
                }
                _ => None,
            });
            AgentEnv {
                id: recipe.id.into(),
                name: recipe.name.into(),
                installed: path.is_some(),
                executable: path.map(|path| path.to_string_lossy().into_owned()),
                version,
                missing,
                install_command: recipe.install.into(),
                setup_command: recipe.setup.into(),
                docs: recipe.docs.into(),
            }
        })
        .collect()
}

fn recipe(id: &str) -> Result<&'static Recipe> {
    RECIPES.iter().find(|recipe| recipe.id == id).ok_or_else(|| "这位成员不支持自动安装".into())
}

fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// 在新的 PowerShell 窗口里跑一段脚本；窗口留着，方便看输出、回答安装器的提问。
fn terminal(title: &str, script: &str) -> Result<()> {
    use base64::Engine;
    let script = format!(
        "$Host.UI.RawUI.WindowTitle = {}; Get-ChildItem Env: | Where-Object {{ $_.Name -like 'CLAUDE_CODE_*' -or $_.Name -eq 'CLAUDECODE' }} | Remove-Item; {script}",
        quote(title)
    );
    let encoded = base64::engine::general_purpose::STANDARD.encode(
        script.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<u8>>(),
    );
    let mut command = Command::new("powershell.exe");
    command.args(["-NoLogo", "-NoExit", "-ExecutionPolicy", "Bypass", "-EncodedCommand", &encoded]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0000_0010); // CREATE_NEW_CONSOLE：给用户一个看得见的窗口
    }
    if let Some(home) = std::env::var_os("USERPROFILE") {
        command.current_dir(home);
    }
    command.spawn().map(|_| ()).map_err(|error| format!("打开终端失败：{error}"))
}

/// 每段脚本开头都把 PATH 刷成最新（上一步刚装的程序立刻可用）。
const FRESH_PATH: &str = "$env:Path = [Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [Environment]::GetEnvironmentVariable('Path','User')";

/// 安装：缺 Node 先装 Node LTS，再跑官方安装命令，装好直接进入初次配置。
pub fn install(id: &str) -> Result<String> {
    let recipe = recipe(id)?;
    let node = if recipe.needs_node.is_some() {
        format!(
            "if (-not (Get-Command npm -ErrorAction SilentlyContinue)) {{ Write-Host '没有找到 Node.js，先用 winget 安装 Node.js LTS…' -ForegroundColor Cyan; winget install --id OpenJS.NodeJS.LTS -e --accept-source-agreements --accept-package-agreements; {FRESH_PATH} }}; "
        )
    } else {
        String::new()
    };
    // Hermes 的官方安装器自带配置向导，不再重复跑 `hermes setup`。
    let then = if recipe.id == "hermes-win" {
        "Write-Host '安装与配置向导结束，回到同席点「重新扫描」即可接入。' -ForegroundColor Green".to_string()
    } else {
        format!(
            "Write-Host '安装步骤结束。接下来做初次登录/配置（{}），完成后回到同席点「重新扫描」。' -ForegroundColor Green; {}",
            recipe.setup, recipe.setup
        )
    };
    let script = format!(
        "$ErrorActionPreference = 'Continue'; {FRESH_PATH}; Write-Host '同席 · 安装 {name}' -ForegroundColor Cyan; Write-Host '将执行：{install}'; {node}{install}; {FRESH_PATH}; Write-Host ''; {then}",
        name = recipe.name,
        install = recipe.install,
    );
    terminal(&format!("同席 · 安装 {}", recipe.name), &script)?;
    Ok(format!("已在新终端里开始安装 {}，装好后会接着进入初次配置", recipe.name))
}

/// 初次登录/配置：在终端里跑成员自己的配置命令。
pub fn setup(id: &str) -> Result<String> {
    let recipe = recipe(id)?;
    let script = format!(
        "{FRESH_PATH}; Write-Host '同席 · {name} 初次配置' -ForegroundColor Cyan; Write-Host '将执行：{setup}'; {setup}",
        name = recipe.name,
        setup = recipe.setup,
    );
    terminal(&format!("同席 · 配置 {}", recipe.name), &script)?;
    Ok(format!("已在新终端里打开 {} 的初次配置", recipe.name))
}

#[tauri::command]
pub fn scan_agents(app: tauri::AppHandle) -> Vec<AgentEnv> {
    let data = crate::service_updates::data_directory(&app).ok();
    scan(data.as_deref())
}

#[tauri::command]
pub fn install_agent(agent_id: String) -> Result<String> {
    install(&agent_id)
}

#[tauri::command]
pub fn setup_agent(agent_id: String) -> Result<String> {
    setup(&agent_id)
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_project_member_has_an_install_and_setup_recipe() {
        for id in crate::project_store::EXECUTOR_AGENTS {
            let recipe = super::recipe(id).unwrap();
            assert!(!recipe.install.is_empty() && !recipe.setup.is_empty() && recipe.docs.starts_with("https://"));
        }
        assert!(super::recipe("albion-wsl").is_err(), "陪伴没有安装包，靠接口配置接入");
    }
}
