use std::path::{Path, PathBuf};
use std::process::Command;

type Result<T> = std::result::Result<T, String>;

pub enum Target {
    Windows(Option<PathBuf>),
    /// OpenCode 自带标准 ACP（`opencode acp`），不用桥。
    OpenCode,
    /// 旧 ACP 通路（WSL Hermes）。Hermes 已从 WSL 删除，同席现在走 `albion_openai`（见 albion.rs）；
    /// 这个变体暂时没人构造，留着只为回退时按 README 装回 WSL Hermes 再用。
    #[allow(dead_code, reason = "ACP 回退分支，MVP 已换 OpenAI 通路")]
    Albion {
        launcher: PathBuf,
        distro: String,
        repo: String,
        python: String,
        profile: String,
    },
}

pub(crate) fn native_root(directory: &Path, distro: &str, profile: &str) -> Result<String> {
    let location = directory.join("albion-wsl.json");
    let namespace = if location.is_file() {
        let value: serde_json::Value = serde_json::from_slice(
            &std::fs::read(&location).map_err(|_| "阿尔比恩数据位置读取失败")?,
        )
        .map_err(|_| "阿尔比恩数据位置无效")?;
        if value["distro"] != distro || value["profile"] != profile {
            return Err("阿尔比恩数据属于其他 WSL 发行版或 profile，请使用独立数据目录".into());
        }
        uuid::Uuid::parse_str(value["namespace"].as_str().ok_or("阿尔比恩数据标识无效")?)
            .map_err(|_| "阿尔比恩数据标识无效")?
    } else {
        let namespace = uuid::Uuid::new_v4();
        let temporary = location.with_extension("json.tmp");
        std::fs::write(
            &temporary,
            serde_json::to_vec(&serde_json::json!({"namespace":namespace.to_string(),"distro":distro,"profile":profile})).unwrap(),
        )
        .map_err(|_| "阿尔比恩数据位置保存失败")?;
        std::fs::rename(temporary, &location).map_err(|_| "阿尔比恩数据位置保存失败")?;
        namespace
    };
    // Does not depend on Windows-drive automount, which is disabled on this PC.
    Ok(format!(
        "/root/.local/share/local-agent-hub/instances/{namespace}"
    ))
}

impl Target {
    /// OpenCode 的程序：npm 装的 `opencode.cmd` 只是壳，真身在旁边的 `node_modules/opencode-ai/bin/opencode.exe`。
    /// 每次连接都重新找，刚装好的不用重启同席。
    pub fn opencode_exe() -> Option<PathBuf> {
        std::env::var_os("AGENT_HUB_OPENCODE").map(PathBuf::from).filter(|path| path.is_file()).or_else(|| {
            crate::service_install::on_path_find(&["opencode.cmd", "opencode.exe"], |shell| {
                let beside = shell.parent()?.join("node_modules/opencode-ai/bin/opencode.exe");
                if beside.is_file() {
                    Some(beside)
                } else {
                    shell.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("exe")).then_some(shell)
                }
            })
        })
    }
    pub fn windows() -> Self {
        Self::Windows(Self::windows_python())
    }
    /// Windows Hermes 的 Python：官方安装器装在 `%LOCALAPPDATA%\hermes\hermes-agent\.venv`。
    /// 每次连接都重新找一遍——同席运行中刚装好的 Hermes 不用重启就能接上。
    pub fn windows_python() -> Option<PathBuf> {
        std::env::var_os("AGENT_HUB_HERMES_PYTHON")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("LOCALAPPDATA").map(|root| {
                    PathBuf::from(root).join("hermes/hermes-agent/.venv/Scripts/python.exe")
                })
            })
            .filter(|path| {
                path.is_absolute()
                    && path.is_file()
                    && path
                        .file_name()
                        .is_some_and(|name| name.eq_ignore_ascii_case("python.exe"))
            })
    }
    #[allow(dead_code, reason = "ACP 回退分支，MVP 已换 OpenAI 通路")]
    pub fn albion() -> Self {
        let setting =
            |name: &str, fallback: &str| std::env::var(name).unwrap_or_else(|_| fallback.into());
        let repo = setting("AGENT_HUB_ALBION_REPO", "/usr/local/lib/hermes-agent");
        Self::Albion {
            launcher: PathBuf::from(
                std::env::var_os("SystemRoot").unwrap_or_else(|| "C:/Windows".into()),
            )
            .join("System32/wsl.exe"),
            distro: setting("AGENT_HUB_ALBION_DISTRO", "Ubuntu"),
            python: setting(
                "AGENT_HUB_ALBION_PYTHON",
                &format!("{repo}/venv/bin/python"),
            ),
            profile: setting("AGENT_HUB_ALBION_PROFILE", "/root/.hermes/profiles/albion"),
            repo,
        }
    }
    pub fn id(&self) -> &'static str {
        match self {
            Self::Windows(_) => "hermes-win",
            Self::OpenCode => "opencode-win",
            Self::Albion { .. } => "albion-wsl",
        }
    }
    pub fn key(&self) -> &'static str {
        match self {
            Self::Windows(_) => "hermes",
            Self::OpenCode => "opencode",
            Self::Albion { .. } => "albion",
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            Self::Windows(_) => "Hermes",
            Self::OpenCode => "OpenCode",
            Self::Albion { .. } => "阿尔比恩",
        }
    }
    pub fn executable(&self) -> Option<String> {
        match self {
            Self::Windows(python) => python
                .clone()
                .or_else(Self::windows_python)
                .map(|path| path.to_string_lossy().into()),
            Self::OpenCode => Self::opencode_exe().map(|path| path.to_string_lossy().into()),
            Self::Albion {
                launcher, python, ..
            } => Some(format!("{} → {python}", launcher.display())),
        }
    }
    pub fn cwd(&self, path: &Path) -> Result<String> {
        match self {
            Self::Windows(_) | Self::OpenCode => Ok(path.to_string_lossy().into()),
            Self::Albion {
                distro, profile, ..
            } => {
                let directory = path
                    .parent()
                    .and_then(Path::parent)
                    .ok_or("阿尔比恩会话目录无效")?;
                let leaf = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or("阿尔比恩会话目录无效")?;
                Ok(format!(
                    "{}/workspaces/{leaf}",
                    native_root(directory, distro, profile)?
                ))
            }
        }
    }
    pub fn command(&self, directory: &Path) -> Result<Command> {
        let data = directory.join(format!("{}-native", self.key()));
        match self {
            Self::OpenCode => {
                let exe = Self::opencode_exe().ok_or("未找到 OpenCode，请先安装（npm i -g opencode-ai），或设置 AGENT_HUB_OPENCODE")?;
                let mut command = Command::new(exe);
                command.arg("acp");
                Ok(command)
            }
            Self::Windows(python) => {
                let python = python.clone().or_else(Self::windows_python).ok_or(
                    "未找到 Windows Hermes，请在「成员与服务 → 接入成员」里一键安装，或设置 AGENT_HUB_HERMES_PYTHON",
                )?;
                let python = &python;
                let repo = python
                    .parent()
                    .and_then(Path::parent)
                    .and_then(Path::parent)
                    .ok_or("Hermes 安装路径不完整")?;
                let mut command = Command::new(python);
                command
                    .args(["-u", "-c", include_str!("hermes_bridge.py")])
                    .arg(repo)
                    .arg(data);
                let home = repo.parent().ok_or("Hermes 主目录不存在")?;
                command.env("HERMES_HOME", home);
                // 指向真实会话库：面板能列出并续聊她自己的历史会话（同一个家目录下的 state.db）。
                // 不给这个变量时桥会退回同席自己的隔离库，删掉即可回退。
                if std::env::var_os("AGENT_HUB_HERMES_SESSION_DB").is_none() {
                    command.env("AGENT_HUB_HERMES_SESSION_DB", home.join("state.db"));
                }
                Ok(command)
            }
            Self::Albion {
                launcher,
                distro,
                repo,
                python,
                profile,
            } => {
                if !launcher.is_file() {
                    return Err("未找到 WSL，请检查 Ubuntu 安装".into());
                }
                if [repo, python, profile]
                    .iter()
                    .any(|value| !value.starts_with('/') || value.contains('\0'))
                    || distro.is_empty()
                    || distro.starts_with('-')
                {
                    return Err("阿尔比恩 WSL 配置需要有效发行版与 Linux 绝对路径".into());
                }
                let source = format!(
                    "{}\n{}",
                    include_str!("albion_prefix.py"),
                    include_str!("hermes_bridge.py")
                );
                let data = format!("{}/albion-native", native_root(directory, distro, profile)?);
                // Windows 的环境变量过不去 wsl.exe（实测），所以库路径走命令行参数。
                // 测试隔离模式（外层已设变量）就用同席自己的库，否则共用她 profile 的真实库。
                let session_db = if std::env::var_os("AGENT_HUB_HERMES_SESSION_DB").is_some() {
                    format!("{data}/sessions.db")
                } else {
                    format!("{profile}/state.db")
                };
                let mut command = Command::new(launcher);
                command.args([
                    "--distribution",
                    distro,
                    "--exec",
                    python,
                    "-u",
                    "-c",
                    &source,
                    repo,
                    &data,
                    profile,
                    &session_db,
                ]);
                Ok(command)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_namespaces_survive_restart_and_do_not_depend_on_windows_mounts() {
        let directory = std::env::temp_dir().join(format!("hub-native-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let root = native_root(&directory, "Ubuntu", "/root/.hermes/profiles/albion").unwrap();
        assert_eq!(
            root,
            native_root(&directory, "Ubuntu", "/root/.hermes/profiles/albion").unwrap()
        );
        assert!(native_root(&directory, "Other", "/root/.hermes/profiles/albion").is_err());
        assert!(root.starts_with("/root/.local/share/local-agent-hub/instances/"));
        std::fs::write(
            directory.join("albion-wsl.json"),
            br#"{"namespace":"../../private"}"#,
        )
        .unwrap();
        assert!(native_root(&directory, "Ubuntu", "/root/.hermes/profiles/albion").is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
