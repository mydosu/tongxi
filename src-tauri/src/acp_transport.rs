use std::path::{Path, PathBuf};
use std::process::Command;

type Result<T> = std::result::Result<T, String>;

pub enum Target {
    Windows(Option<PathBuf>),
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
    pub fn windows() -> Self {
        let python = std::env::var_os("AGENT_HUB_HERMES_PYTHON")
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
            });
        Self::Windows(python)
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
            Self::Albion { .. } => "albion-wsl",
        }
    }
    pub fn key(&self) -> &'static str {
        match self {
            Self::Windows(_) => "hermes",
            Self::Albion { .. } => "albion",
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            Self::Windows(_) => "Hermes",
            Self::Albion { .. } => "阿尔比恩",
        }
    }
    pub fn executable(&self) -> Option<String> {
        match self {
            Self::Windows(python) => python.as_ref().map(|path| path.to_string_lossy().into()),
            Self::Albion {
                launcher, python, ..
            } => Some(format!("{} → {python}", launcher.display())),
        }
    }
    pub fn cwd(&self, path: &Path) -> Result<String> {
        match self {
            Self::Windows(_) => Ok(path.to_string_lossy().into()),
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
            Self::Windows(python) => {
                let python = python.as_ref().ok_or(
                    "未找到 Windows Hermes Python 环境，请检查安装或 AGENT_HUB_HERMES_PYTHON",
                )?;
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

    /// Hermes project sessions use a project-only bridge: private chat stays tool-free,
    /// while this subprocess can receive only the per-attempt Agent Hub MCP server.
    pub fn project_command(&self, directory: &Path) -> Result<Command> {
        match self {
            Self::Windows(python) => {
                let python = python.as_ref().ok_or(
                    "未找到 Windows Hermes Python 环境，请检查安装或 AGENT_HUB_HERMES_PYTHON",
                )?;
                let repo = python
                    .parent()
                    .and_then(Path::parent)
                    .and_then(Path::parent)
                    .ok_or("Hermes 安装路径不完整")?;
                let home = repo.parent().ok_or("Hermes 主目录不存在")?;
                let mut command = Command::new(python);
                command
                    .args(["-u", "-c", include_str!("hermes_project_bridge.py")])
                    .arg(repo)
                    .arg(directory.join("hermes-project"));
                command
                    .env("HERMES_HOME", home)
                    .env("HERMES_ACP_SKIP_CONFIGURED_MCP", "1");
                Ok(command)
            }
            Self::Albion { .. } => Err("WSL Pi 不属于 Hermes 项目 ACP 通道".into()),
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
