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
