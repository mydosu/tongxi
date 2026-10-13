use serde_json::{json, Value};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

pub fn call(project: &str, agent: &str, tool: &str, args: Value) -> Result<Value, String> {
    let python = std::env::var_os("AGENT_HUB_OPENVIKING_PYTHON")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("D:/AI/openviking/venv/Scripts/python.exe"));
    if !python.is_file() {
        return Err("未找到 OpenViking Python 客户端，可设置 AGENT_HUB_OPENVIKING_PYTHON".into());
    }
    let mut command = Command::new(python);
    command.env("PYTHONIOENCODING", "utf-8");
    // 不继承启动同席那个环境里的 OpenViking 身份（例如某个 agent 终端注入的 OPENVIKING_API_KEY），
    // 否则记忆会悄悄写成那个 agent；身份只来自下面指定的配置文件。
    for (name, _) in std::env::vars_os() {
        if name
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("OPENVIKING_")
        {
            command.env_remove(name);
        }
    }
    // 同席的记忆工具没有 harness 身份：用中立 tooling 身份的专属配置，不依赖共享 ovcli.conf 的顶层 key
    // （那个顶层 key 会抢在各 harness 自己的 key 前面，留空才能保证各 agent 不被解析成别人）。
    if let Some(config) = std::env::var_os("AGENT_HUB_OPENVIKING_CONFIG")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .map(|home| {
                    PathBuf::from(home)
                        .join(".openviking")
                        .join("ovcli.tooling.conf")
                })
                .filter(|path| path.is_file())
        })
    {
        command.env("OPENVIKING_CLI_CONFIG_FILE", config);
    }
    command
        .args(["-u", "-c", include_str!("project_memory.py")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command.spawn().map_err(|_| "项目记忆客户端启动失败")?;
    let _scope = crate::process_scope::ProcessScope::attach(&child)?;
    let payload = json!({"project":project,"agent":agent,"tool":tool,"args":args});
    child
        .stdin
        .take()
        .ok_or("项目记忆输入不可用")?
        .write_all(payload.to_string().as_bytes())
        .map_err(|_| "项目记忆请求失败")?;
    let output = child
        .wait_with_output()
        .map_err(|_| "项目记忆客户端未完成")?;
    let reply: Value = serde_json::from_slice(&output.stdout).map_err(|_| "项目记忆响应无效")?;
    if let Some(error) = reply["error"].as_str() {
        return Err(error.into());
    }
    Ok(reply["result"].clone())
}
