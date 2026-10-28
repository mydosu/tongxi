//! 手机遥控网关（第 0 期，只读）：只绑 Tailscale 地址，Bearer token 鉴权，
//! `POST /rpc/<command>` 走只读白名单，其余 GET 回打包进 exe 的前端资源。
use crate::AppState;
use ring::rand::{SecureRandom, SystemRandom};
use serde::Serialize;
use serde_json::Value;
use std::io::Read;
use std::net::{Ipv4Addr, SocketAddr};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use tauri::{AppHandle, Manager, State};
use tiny_http::{Header, Method, Request, Response, Server};

const PORT: u16 = 47680;
const ENABLED_KEY: &str = "remote_gateway_enabled";
const TOKEN_KEY: &str = "remote_gateway_token";
const MAX_BODY: u64 = 1 << 20;

/// 远程只读白名单：都只读数据，不改库、不起子进程、不碰文件系统和配置。
/// （get_conversation 会按需补建项目成员私聊的空行，和桌面打开项目时一样，不改已有内容。）
const READ_COMMANDS: &[&str] = &[
    "list_agents",
    "list_conversations",
    "get_conversation",
    "pending_chat_questions",
    "project_inquiries",
    "brain_overview",
    "list_projects",
    "app_info",
    "own_avatars",
    "discussion_status",
    "cli_handoffs",
    "attachment_data",
    "list_backgrounds",
    "background_image",
    "codex_status",
    "hermes_status",
    "dsh_status",
    "claude_status",
    "opencode_status",
    "albion_status",
];

enum RpcError {
    Forbidden,
    BadRequest(String),
    Failed(String),
}

#[derive(Default)]
struct Running {
    stop: Option<Arc<AtomicBool>>,
    thread: Option<JoinHandle<()>>,
    address: Option<SocketAddr>,
    error: Option<String>,
}

pub struct Gateway {
    token: Arc<Mutex<String>>,
    running: Mutex<Running>,
}

#[derive(Serialize)]
pub struct GatewayStatus {
    enabled: bool,
    running: bool,
    /// 绑定地址 `100.x.y.z:47680`。
    address: Option<String>,
    /// 带 token 的完整访问地址；只在服务运行时给。
    url: Option<String>,
    /// 启用了却没跑起来的原因。
    error: Option<String>,
}

/// 100.64.0.0/10（Tailscale 的 CGNAT 段）。
fn in_tailscale_range(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    a == 100 && b & 0xC0 == 64
}

/// 取 `tailscale ip -4` 输出的第一行，必须落在 Tailscale 段内。
fn parse_tailscale_ip(output: &str) -> Result<Ipv4Addr, String> {
    let line = output.lines().map(str::trim).find(|line| !line.is_empty()).ok_or("tailscale 没有返回 IPv4 地址，请确认已登录")?;
    let ip: Ipv4Addr = line.parse().map_err(|_| "tailscale 返回的不是 IPv4 地址")?;
    if in_tailscale_range(ip) { Ok(ip) } else { Err("tailscale 返回的地址不在 100.64.0.0/10 内".into()) }
}

fn tailscale_ip() -> Result<Ipv4Addr, String> {
    let mut last = String::from("找不到 tailscale 命令，请确认已安装并登录");
    for program in ["tailscale", r"C:\Program Files\Tailscale\tailscale.exe"] {
        let mut command = Command::new(program);
        command.args(["ip", "-4"]);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        match command.output() {
            Ok(output) if output.status.success() => return parse_tailscale_ip(&String::from_utf8_lossy(&output.stdout)),
            Ok(_) => last = "tailscale 未运行或未登录".into(),
            Err(_) => {}
        }
    }
    Err(last)
}

fn new_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    SystemRandom::new().fill(&mut bytes).map_err(|_| "系统随机数不可用")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// `Authorization: Bearer <token>`，常量时间比较。
fn bearer_ok(header: Option<&str>, token: &str) -> bool {
    let Some(given) = header.and_then(|value| value.strip_prefix("Bearer ")) else {
        return false;
    };
    let (given, token) = (given.trim().as_bytes(), token.as_bytes());
    // token 定长，长度不同直接不等；等长时逐字节异或累加，耗时与内容无关。
    given.len() == token.len() && given.iter().zip(token).fold(0u8, |diff, (a, b)| diff | (a ^ b)) == 0
}

fn meta_get(app: &AppHandle, key: &str) -> Option<String> {
    let state = app.state::<AppState>();
    let store = state.store.lock().ok()?;
    store.connection.query_row("SELECT value FROM metadata WHERE key=?1", [key], |row| row.get(0)).ok()
}

fn meta_set(app: &AppHandle, key: &str, value: &str) -> Result<(), String> {
    let state = app.state::<AppState>();
    let store = state.store.lock().map_err(|_| "本地数据暂不可用")?;
    store
        .connection
        .execute("INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=?2", [key, value])
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn arg_str(args: &Value, key: &str) -> Result<String, RpcError> {
    args.get(key).and_then(Value::as_str).map(str::to_owned).ok_or_else(|| RpcError::BadRequest(format!("缺少参数 {key}")))
}

fn arg_bool(args: &Value, key: &str) -> Result<bool, RpcError> {
    args.get(key).and_then(Value::as_bool).ok_or_else(|| RpcError::BadRequest(format!("缺少参数 {key}")))
}

fn out<T: Serialize>(result: Result<T, String>) -> Result<Value, RpcError> {
    let value = result.map_err(RpcError::Failed)?;
    serde_json::to_value(value).map_err(|e| RpcError::Failed(e.to_string()))
}

/// 命令名和参数（camelCase，与前端 invoke 一致）→ 直接调对应的 #[tauri::command] 函数。
fn dispatch(app: &AppHandle, command: &str, args: &Value) -> Result<Value, RpcError> {
    if !READ_COMMANDS.contains(&command) {
        return Err(RpcError::Forbidden);
    }
    let state = || app.state::<AppState>();
    match command {
        "list_agents" => out(Ok(crate::list_agents(state()))),
        "list_conversations" => out(crate::list_conversations(state(), arg_str(args, "search")?, arg_bool(args, "archived")?)),
        "get_conversation" => out(crate::get_conversation(state(), arg_str(args, "id")?)),
        "pending_chat_questions" => out(crate::pending_chat_questions(state())),
        "project_inquiries" => out(tauri::async_runtime::block_on(crate::project_inquiries(state(), arg_str(args, "conversationId")?))),
        "brain_overview" => out(tauri::async_runtime::block_on(crate::brain_overview(state(), arg_str(args, "conversationId")?))),
        "list_projects" => out(crate::projects::list_projects(state())),
        "app_info" => out(crate::app_info(app.clone(), state())),
        "own_avatars" => out(crate::avatars::own_avatars(state())),
        "discussion_status" => out(crate::discussion::discussion_status(app.state::<Arc<crate::discussion::Runtime>>())),
        "cli_handoffs" => out(Ok(crate::cli_handoffs(app.clone()))),
        "attachment_data" => out(crate::attachment_data(state(), arg_str(args, "conversationId")?, arg_str(args, "attachmentId")?)),
        "codex_status" => out(Ok(crate::codex_status(app.state::<Arc<crate::codex::Runtime>>()))),
        "hermes_status" => out(Ok(crate::hermes_status(app.state::<Arc<crate::hermes::Runtime>>()))),
        "dsh_status" => out(Ok(crate::dsh_status(app.state::<Arc<crate::dsh::Runtime>>()))),
        "claude_status" => out(Ok(crate::claude_status(app.state::<Arc<crate::claude::Runtime>>()))),
        "opencode_status" => out(Ok(crate::opencode::opencode_status(app.state::<crate::opencode::Runtime>()))),
        "albion_status" => out(Ok(crate::albion::albion_status(app.state::<crate::albion::Runtime>()))),
        "list_backgrounds" => out(crate::backgrounds::list_backgrounds(state())),
        "background_image" => out(crate::backgrounds::background_image(state(), arg_str(args, "id")?)),
        _ => Err(RpcError::Forbidden),
    }
}

fn reply(request: Request, status: u16, content_type: &str, body: Vec<u8>) {
    let mut response = Response::from_data(body).with_status_code(status);
    for (name, value) in [("Content-Type", content_type), ("Cache-Control", "no-store"), ("X-Content-Type-Options", "nosniff")] {
        if let Ok(header) = Header::from_bytes(name, value) {
            response.add_header(header);
        }
    }
    let _ = request.respond(response);
}

fn reply_text(request: Request, status: u16, text: &str) {
    reply(request, status, "text/plain; charset=utf-8", text.as_bytes().to_vec());
}

fn handle(app: &AppHandle, token: &Mutex<String>, mut request: Request) {
    let url = request.url().to_owned();
    let path = url.split('?').next().unwrap_or("/").to_owned();
    if let Some(command) = path.strip_prefix("/rpc/") {
        if *request.method() != Method::Post {
            return reply_text(request, 405, "只接受 POST");
        }
        let authorization = request.headers().iter().find(|h| h.field.equiv("Authorization")).map(|h| h.value.to_string());
        let valid = token.lock().map(|token| bearer_ok(authorization.as_deref(), &token)).unwrap_or(false);
        if !valid {
            return reply_text(request, 401, "未授权");
        }
        let mut body = String::new();
        if request.as_reader().take(MAX_BODY).read_to_string(&mut body).is_err() {
            return reply_text(request, 400, "请求体读取失败");
        }
        let args = if body.trim().is_empty() { Ok(Value::Object(Default::default())) } else { serde_json::from_str::<Value>(&body) };
        let result = args.map_err(|e| RpcError::BadRequest(format!("请求体不是 JSON：{e}"))).and_then(|args| dispatch(app, command, &args));
        return match result {
            Ok(value) => reply(request, 200, "application/json", value.to_string().into_bytes()),
            Err(RpcError::Forbidden) => reply_text(request, 403, "该命令不允许远程调用"),
            Err(RpcError::BadRequest(message)) => reply_text(request, 400, &message),
            Err(RpcError::Failed(message)) => reply_text(request, 500, &message),
        };
    }
    if *request.method() != Method::Get {
        return reply_text(request, 405, "只接受 GET");
    }
    // 页面本身不含数据，静态资源不鉴权；资源表是打包时固定的，这里只挡掉路径穿越。
    if path.contains("..") || path.contains('\\') {
        return reply_text(request, 400, "路径无效");
    }
    match app.asset_resolver().get(path) {
        Some(asset) => reply(request, 200, &asset.mime_type, asset.bytes),
        None => reply_text(request, 404, "没有这个页面"),
    }
}

impl Gateway {
    fn stop(&self) {
        let Ok(mut running) = self.running.lock() else { return };
        if let Some(stop) = running.stop.take() {
            stop.store(true, Ordering::SeqCst);
        }
        if let Some(thread) = running.thread.take() {
            let _ = thread.join();
        }
        running.address = None;
    }

    /// 起服务；失败只记原因，不算命令错误。
    fn start(&self, app: &AppHandle) {
        self.stop();
        let result = tailscale_ip().and_then(|ip| {
            let address = SocketAddr::from((ip, PORT));
            let server = Server::http(address).map_err(|e| format!("绑定 {address} 失败：{e}"))?;
            Ok((address, server))
        });
        let Ok(mut running) = self.running.lock() else { return };
        match result {
            Err(reason) => running.error = Some(reason),
            Ok((address, server)) => {
                let stop = Arc::new(AtomicBool::new(false));
                let (flag, app, token) = (stop.clone(), app.clone(), self.token.clone());
                running.thread = Some(std::thread::spawn(move || {
                    while !flag.load(Ordering::SeqCst) {
                        match server.recv_timeout(Duration::from_millis(300)) {
                            Ok(Some(request)) => {
                                let (app, token) = (app.clone(), token.clone());
                                std::thread::spawn(move || handle(&app, &token, request));
                            }
                            Ok(None) => {}
                            Err(_) => break,
                        }
                    }
                }));
                running.stop = Some(stop);
                running.address = Some(address);
                running.error = None;
            }
        }
    }

    fn status(&self, app: &AppHandle) -> GatewayStatus {
        let running = self.running.lock().ok();
        let address = running.as_ref().and_then(|r| r.address);
        let token = self.token.lock().map(|t| t.clone()).unwrap_or_default();
        GatewayStatus {
            enabled: meta_get(app, ENABLED_KEY).as_deref() == Some("1"),
            running: address.is_some(),
            address: address.map(|a| a.to_string()),
            url: address.map(|a| format!("http://{a}/?token={token}")),
            error: running.and_then(|r| r.error.clone()),
        }
    }
}

/// 应用启动时调用：准备 token，已启用就起服务。
pub fn init(app: &AppHandle) -> Result<(), String> {
    let token = match meta_get(app, TOKEN_KEY) {
        Some(token) if !token.is_empty() => token,
        _ => {
            let token = new_token()?;
            meta_set(app, TOKEN_KEY, &token)?;
            token
        }
    };
    let gateway = Gateway { token: Arc::new(Mutex::new(token)), running: Mutex::new(Running::default()) };
    if meta_get(app, ENABLED_KEY).as_deref() == Some("1") {
        gateway.start(app);
    }
    app.manage(gateway);
    Ok(())
}

#[tauri::command]
pub fn remote_gateway_status(app: AppHandle, gateway: State<'_, Gateway>) -> GatewayStatus {
    gateway.status(&app)
}

#[tauri::command]
pub fn set_remote_gateway(app: AppHandle, gateway: State<'_, Gateway>, enabled: bool) -> Result<GatewayStatus, String> {
    meta_set(&app, ENABLED_KEY, if enabled { "1" } else { "0" })?;
    if enabled {
        gateway.start(&app);
    } else {
        gateway.stop();
        if let Ok(mut running) = gateway.running.lock() {
            running.error = None;
        }
    }
    Ok(gateway.status(&app))
}

/// 换新 token：旧的立即失效（每个请求都读当前 token），等于吊销。
#[tauri::command]
pub fn reset_remote_gateway_token(app: AppHandle, gateway: State<'_, Gateway>) -> Result<GatewayStatus, String> {
    let token = new_token()?;
    meta_set(&app, TOKEN_KEY, &token)?;
    *gateway.token.lock().map_err(|_| "token 暂不可用")? = token;
    Ok(gateway.status(&app))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_compares_exact_token() {
        assert!(bearer_ok(Some("Bearer abc123"), "abc123"));
        assert!(!bearer_ok(Some("Bearer abc124"), "abc123"));
        assert!(!bearer_ok(Some("Bearer abc12"), "abc123"));
        assert!(!bearer_ok(Some("abc123"), "abc123"));
        assert!(!bearer_ok(None, "abc123"));
    }

    #[test]
    fn whitelist_rejects_writes_and_unknown_commands() {
        for command in ["send_claude_message", "apply_app_update", "install_agent", "set_default_workspace", "save_companion", "delete_conversation", "remote_gateway_status", "reset_remote_gateway_token", "nope"] {
            assert!(!READ_COMMANDS.contains(&command), "{command} 不该在白名单里");
        }
        assert!(READ_COMMANDS.contains(&"list_conversations") && READ_COMMANDS.contains(&"dsh_status") && !READ_COMMANDS.contains(&"connect_dsh"));
        assert!(READ_COMMANDS.contains(&"background_image") && !READ_COMMANDS.contains(&"add_background") && !READ_COMMANDS.contains(&"remove_background"));
    }

    #[test]
    fn tailscale_address_must_be_in_cgnat_range() {
        assert_eq!(parse_tailscale_ip("100.98.117.121\n").unwrap(), Ipv4Addr::new(100, 98, 117, 121));
        assert_eq!(parse_tailscale_ip("\n100.64.0.1\n100.64.0.2").unwrap(), Ipv4Addr::new(100, 64, 0, 1));
        assert_eq!(parse_tailscale_ip("100.127.255.254").unwrap(), Ipv4Addr::new(100, 127, 255, 254));
        for bad in ["", "0.0.0.0", "127.0.0.1", "192.168.1.5", "100.63.0.1", "100.128.0.1", "fd7a::1", "garbage"] {
            assert!(parse_tailscale_ip(bad).is_err(), "{bad} 应被拒绝");
        }
    }

    #[test]
    fn tokens_are_64_hex_and_unique() {
        let (a, b) = (new_token().unwrap(), new_token().unwrap());
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
