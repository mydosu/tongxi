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
const RECIPES: [Recipe; 6] = [
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
    Recipe {
        id: "opencode-win",
        name: "OpenCode",
        install: "npm install -g opencode-ai",
        setup: "opencode auth login",
        docs: "https://opencode.ai/docs",
        needs_node: Some((18, 0)),
    },
    Recipe {
        id: "antigravity-win",
        name: "Antigravity",
        install: "irm https://antigravity.google/cli/install.ps1 | iex",
        setup: "agy",
        docs: "https://antigravity.google/",
        // 同席接它的桥是 Node 脚本。
        needs_node: Some((18, 0)),
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
    // PATH 里还是没有 node：fnm 这类工具只在终端会话里改 PATH，从资源管理器启动的同席看不到。
    // 补上常见 Node 管理器的「当前版本」目录（npm -g 装的成员命令也在这里）。
    if !paths.iter().any(|dir| dir.join("node.exe").is_file()) {
        if let Some(dir) = node_fallbacks().into_iter().find(|dir| dir.join("node.exe").is_file()) {
            paths.push(dir);
        }
    }
    if let Ok(joined) = std::env::join_paths(paths) {
        std::env::set_var("PATH", joined);
    }
}

fn node_fallbacks() -> Vec<PathBuf> {
    let env = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty()).map(PathBuf::from);
    let mut dirs = Vec::new();
    dirs.extend(env("NVM_SYMLINK")); // nvm-windows
    dirs.extend(env("FNM_DIR").map(|dir| dir.join("aliases/default")));
    dirs.extend(env("APPDATA").map(|dir| dir.join("fnm/aliases/default")));
    dirs.extend(env("LOCALAPPDATA").map(|dir| dir.join("fnm/aliases/default")));
    dirs.extend(env("LOCALAPPDATA").map(|dir| dir.join("Volta/bin")));
    dirs.extend(env("ProgramFiles").map(|dir| dir.join("nodejs")));
    dirs
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

/// `<程序> --version` 输出里第一个像版本号的词（`v1.2.3`、`Hermes Agent 0.9.1` 都认）。读不出就不显示。
fn cli_version(executable: &std::path::Path) -> Option<String> {
    if !executable.is_file() {
        return None;
    }
    let mut command = Command::new(executable);
    command.arg("--version");
    let output = quiet(&mut command).output().ok()?;
    version_in(&String::from_utf8_lossy(&output.stdout))
}

fn version_in(text: &str) -> Option<String> {
    text.split_whitespace()
        .map(|word| word.trim_start_matches(['v', 'V']).trim_end_matches([',', ')']).split('+').next().unwrap_or_default())
        .find(|word| {
            let mut parts = word.split('.');
            let number = |part: Option<&str>| part.is_some_and(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
            number(parts.next()) && number(parts.next().map(|part| part.split('-').next().unwrap_or_default()))
        })
        .map(String::from)
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
        "hermes-win" => {
            let path = crate::acp_transport::Target::windows_python();
            let version = path.as_ref().and_then(|python| cli_version(&python.parent()?.join("hermes.exe")));
            (path, version)
        }
        "opencode-win" => {
            let path = crate::acp_transport::Target::opencode_exe();
            let version = path.as_deref().and_then(cli_version);
            (path, version)
        }
        "antigravity-win" => {
            let path = crate::acp_transport::Target::antigravity_exe();
            let version = path.as_deref().and_then(cli_version);
            (path, version)
        }
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
                None => Some(format!("需要 Node.js {major}.{minor}+（未安装，安装时会先装 Node.js LTS）")),
                Some((have_major, have_minor, text)) if (*have_major, *have_minor) < (major, minor) => {
                    Some(format!("需要 Node.js {major}.{minor}+，当前 {text}（安装时会先升级到 LTS）"))
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
/// 脚本先写成临时 .ps1 再 `-File` 运行：`-EncodedCommand` 常被杀毒软件当可疑行为拦下。
fn terminal(title: &str, script: &str) -> Result<()> {
    let script = format!(
        "Remove-Item -LiteralPath $PSCommandPath -ErrorAction SilentlyContinue; $Host.UI.RawUI.WindowTitle = {}; Get-ChildItem Env: | Where-Object {{ $_.Name -like 'CLAUDE_CODE_*' -or $_.Name -eq 'CLAUDECODE' }} | Remove-Item; {script}",
        quote(title)
    );
    let file = std::env::temp_dir().join(format!("tongxi-{}.ps1", uuid::Uuid::new_v4()));
    // Windows PowerShell 5.1 不带 BOM 会按本地代码页读，中文提示会乱码。
    std::fs::write(&file, ["\u{feff}".as_bytes(), script.as_bytes()].concat())
        .map_err(|error| format!("写安装脚本失败：{error}"))?;
    let mut command = Command::new("powershell.exe");
    command.args(["-NoLogo", "-NoExit", "-ExecutionPolicy", "Bypass", "-File"]).arg(&file);
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

/// 确保 Node.js 不低于 major.minor：没有或太旧就装/升级 LTS——先用 winget，没有 winget 或失败就下官方 MSI。
/// 装完仍不够（多半是 nvm / fnm / Volta 把旧版排在 PATH 前面）就停下说明，不硬装成员。
fn node_script(major: u32, minor: u32) -> String {
    format!(
        r#"function Get-NodeVersion {{ try {{ [version]((& node --version 2>$null) -replace '^v','') }} catch {{ $null }} }}
$need = [version]'{major}.{minor}.0'; $have = Get-NodeVersion
if (-not $have -or $have -lt $need) {{
  if ($have) {{ Write-Host "当前 Node.js $have 低于 $need，先安装 Node.js LTS…" -ForegroundColor Cyan }} else {{ Write-Host '没有找到 Node.js，先安装 Node.js LTS…' -ForegroundColor Cyan }}
  $done = $false
  if (Get-Command winget -ErrorAction SilentlyContinue) {{
    winget install --id OpenJS.NodeJS.LTS -e --accept-source-agreements --accept-package-agreements
    if ($LASTEXITCODE -ne 0) {{ winget upgrade --id OpenJS.NodeJS.LTS -e --accept-source-agreements --accept-package-agreements }}
    $done = $LASTEXITCODE -eq 0
  }}
  if (-not $done) {{
    Write-Host '改用 Node.js 官方安装包…' -ForegroundColor Cyan
    $list = Invoke-RestMethod https://nodejs.org/dist/index.json  # 先存变量再过滤：直接接管道时整张表会被当成一个对象
    $lts = ($list | Where-Object {{ $_.lts }} | Select-Object -First 1).version
    $arch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') {{ 'arm64' }} else {{ 'x64' }}
    $msi = Join-Path $env:TEMP "node-$lts-$arch.msi"
    Invoke-WebRequest "https://nodejs.org/dist/$lts/node-$lts-$arch.msi" -OutFile $msi -UseBasicParsing
    Start-Process msiexec.exe -ArgumentList '/i', "`"$msi`"", '/passive' -Wait
  }}
  {FRESH_PATH}
  $have = Get-NodeVersion
  if (-not $have -or $have -lt $need) {{
    Write-Host "Node.js 仍不满足（当前：$have，需要 $need 以上）。如果用 nvm / fnm / Volta 管理 Node，请先切到新版本再回同席重试。" -ForegroundColor Yellow
    return
  }}
}}
"#
    )
}

/// 每段脚本开头都把 PATH 刷成最新（上一步刚装的程序立刻可用）。
const FRESH_PATH: &str = "$env:Path = [Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [Environment]::GetEnvironmentVariable('Path','User')";

/// 安装：缺 Node 先装 Node LTS，再跑官方安装命令，装好直接进入初次配置。
pub fn install(id: &str) -> Result<String> {
    let recipe = recipe(id)?;
    let node = recipe.needs_node.map(|(major, minor)| node_script(major, minor)).unwrap_or_default();
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
        "$ErrorActionPreference = 'Continue'; {FRESH_PATH}; Write-Host '同席 · 安装 {name}' -ForegroundColor Cyan; Write-Host '将执行：{install}'
{node}{install}; {FRESH_PATH}; Write-Host ''; {then}",
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

    #[test]
    fn versions_are_read_from_common_version_lines() {
        assert_eq!(super::version_in("v1.4.2
").as_deref(), Some("1.4.2"));
        assert_eq!(super::version_in("Hermes Agent 0.9.1 (abc)").as_deref(), Some("0.9.1"));
        assert_eq!(super::version_in("opencode version: 0.15.3-beta").as_deref(), Some("0.15.3-beta"));
        assert_eq!(super::version_in("no version here.").as_deref(), None);
        assert_eq!(super::version_in("Hermes Agent v0.21.7+18.g590af66.dirty (2026.9.24) · stable
Python: 3.11.16").as_deref(), Some("0.21.7"));
    }

    #[test]
    fn node_script_checks_version_and_falls_back_to_the_official_installer() {
        let script = super::node_script(22, 19);
        assert!(script.contains("[version]'22.19.0'") && script.contains("winget upgrade") && script.contains("nodejs.org/dist/index.json"));
    }
}
