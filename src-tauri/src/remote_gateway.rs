//! 手机遥控网关（第 0 期，只读）：只绑 Tailscale 地址，Bearer token 鉴权，
//! `POST /rpc/<command>` 走只读白名单，其余 GET 回打包进 exe 的前端资源。
use crate::AppState;
use flate2::{write::GzEncoder, Compression};
use ring::rand::{SecureRandom, SystemRandom};
use serde::Serialize;
use serde_json::Value;
use std::io::Read;
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::process::Command;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use tauri::{AppHandle, Listener, Manager, State};
use tiny_http::{Header, Method, Request, Response, Server};

const PORT: u16 = 47680;
const ENABLED_KEY: &str = "remote_gateway_enabled";
const TOKEN_KEY: &str = "remote_gateway_token";
/// 端口默认固定；测试版要和正在跑的正式版并存时，用 AGENT_HUB_GATEWAY_PORT 换一个。
fn gateway_port() -> u16 {
    std::env::var("AGENT_HUB_GATEWAY_PORT").ok().and_then(|port| port.parse().ok()).unwrap_or(PORT)
}
const MAX_BODY: u64 = 1 << 20;
/// 附件走 stage_attachment：前端单个附件上限 32 MiB，base64 后约 43 MB，留一点余量。
const MAX_ATTACHMENT_BODY: u64 = 48 << 20;
const MAX_EVENT_CLIENTS: usize = 16;
const PING_EVERY: Duration = Duration::from_secs(15);

/// 前端 boot 里 listen 的全部事件（六个 harness 的 `<key>-state` 加五个公共事件）。
const EVENTS: &[&str] = &[
    "codex-state",
    "hermes-state",
    "dsh-state",
    "claude-state",
    "opencode-state",
    "albion-state",
    "cli-handoffs",
    "cli-returned",
    "discussion-state",
    "brain-wrap-up",
    "brain-state",
];

/// 运行状态事件：每个都是带版本号的完整快照（含模型表和全部在跑回复的全文），流式回复时一秒几十个，
/// 手机上每个都是上百 KB。网关只把最新的一份按这个间隔发出去，前端本来就只认版本号最大的。
const COALESCED: &[&str] = &["codex-state", "hermes-state", "dsh-state", "claude-state", "opencode-state", "albion-state", "discussion-state"];
const COALESCE_EVERY: Duration = Duration::from_millis(100);

/// 远程只读白名单：都只读数据，不改库、不起子进程、不碰文件系统和配置。
/// （get_conversation 会按需补建项目成员私聊的空行，和桌面打开项目时一样，不改已有内容。）
const READ_COMMANDS: &[&str] = &[
    "list_agents",
    "list_conversations",
    "get_conversation",
    "pending_chat_questions",
    "project_inquiries",
    "brain_overview",
    "brain_live_jobs",
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

/// 手机上要能做的写操作：发消息、插话、停止、答问、传附件、新建会话。
/// 不含：删除/归档/改名、头像、主题、项目目录、成员接入与断开、服务更新安装、设置类、读桌面剪贴板。
const WRITE_COMMANDS: &[&str] = &[
    "send_codex_message",
    "send_hermes_message",
    "send_dsh_message",
    "send_claude_message",
    "send_opencode_message",
    "send_albion_message",
    "steer_agent_message",
    "cancel_codex_run",
    "cancel_hermes_run",
    "cancel_dsh_run",
    "cancel_claude_run",
    "cancel_opencode_run",
    "cancel_albion_run",
    "brain_wrap_up",
    "stop_brain_job",
    "answer_chat_question",
    "answer_project_inquiry",
    "stage_attachment",
    "create_conversation",
    "cancel_chat_question",
    "start_discussion",
    "cancel_discussion",
    "save_local_message",
];

fn allowed(command: &str) -> bool {
    READ_COMMANDS.contains(&command) || WRITE_COMMANDS.contains(&command)
}

enum RpcError {
    Forbidden,
    BadRequest(String),
    Failed(String),
}

#[derive(Default)]
struct Running {
    stop: Option<Arc<AtomicBool>>,
    thread: Option<JoinHandle<()>>,
    front: Option<JoinHandle<()>>,
    address: Option<SocketAddr>,
    error: Option<String>,
}

struct Client {
    id: u64,
    tx: SyncSender<Arc<str>>,
    alive: Arc<AtomicBool>,
}

/// 每种事件只留最新一帧，等下一个发送时机一起放出。
/// 状态事件的模型表没变就不带（见 state_events）：被替换掉的那帧要是带着模型表而新这帧没有，要把模型表并进新帧，不然改动丢了。
#[derive(Default)]
struct Latest {
    payloads: Mutex<Vec<(&'static str, String)>>,
}

fn keep_models(old: &str, new: String) -> String {
    if !old.contains("\"models\"") {
        return new;
    }
    let (Ok(Value::Object(old)), Ok(Value::Object(mut fresh))) = (serde_json::from_str::<Value>(old), serde_json::from_str::<Value>(&new)) else { return new };
    match (old.get("models"), fresh.contains_key("models")) {
        (Some(models), false) => {
            fresh.insert("models".into(), models.clone());
            Value::Object(fresh).to_string()
        }
        _ => new,
    }
}

impl Latest {
    fn put(&self, name: &'static str, payload: &str) {
        let Ok(mut payloads) = self.payloads.lock() else { return };
        match payloads.iter_mut().find(|(have, _)| *have == name) {
            Some(slot) => slot.1 = keep_models(&slot.1, payload.to_string()),
            None => payloads.push((name, payload.to_string())),
        }
    }

    fn take(&self) -> Vec<String> {
        self.payloads.lock().map(|mut payloads| payloads.drain(..).map(|(name, payload)| sse_frame(name, &payload)).collect()).unwrap_or_default()
    }
}

/// SSE 广播：每条连接一个有界 channel，写不进去（满了/断了）就摘掉。
#[derive(Default)]
struct Events {
    next: AtomicU64,
    clients: Mutex<Vec<Client>>,
    /// 对外一侧的 socket（见 `forward`）：close_all 直接 shutdown 它们，对端才会收到 FIN。
    conns: Mutex<Vec<(u64, TcpStream)>>,
}

impl Events {
    fn subscribe(&self) -> Option<(u64, std::sync::mpsc::Receiver<Arc<str>>, Arc<AtomicBool>)> {
        let mut clients = self.clients.lock().ok()?;
        if clients.len() >= MAX_EVENT_CLIENTS {
            return None;
        }
        let (tx, rx) = sync_channel(256);
        let (id, alive) = (self.next.fetch_add(1, Ordering::SeqCst), Arc::new(AtomicBool::new(true)));
        clients.push(Client { id, tx, alive: alive.clone() });
        Some((id, rx, alive))
    }

    fn broadcast(&self, frame: &str) {
        let Ok(mut clients) = self.clients.lock() else { return };
        if clients.is_empty() {
            return;
        }
        let frame: Arc<str> = frame.into();
        clients.retain(|client| client.tx.try_send(frame.clone()).is_ok());
    }

    fn remove(&self, id: u64) {
        if let Ok(mut clients) = self.clients.lock() {
            clients.retain(|client| client.id != id);
        }
    }

    /// 网关关闭或换令牌：已有连接立刻作废（连接线程见 alive=false 就停写并收线）。
    fn close_all(&self) {
        if let Ok(mut clients) = self.clients.lock() {
            for client in clients.drain(..) {
                client.alive.store(false, Ordering::SeqCst);
            }
        }
        if let Ok(mut conns) = self.conns.lock() {
            for (_, conn) in conns.drain(..) {
                let _ = conn.shutdown(Shutdown::Both);
            }
        }
    }
}

/// tiny_http 的连接线程在 upgrade 之后仍握着底层 socket 去等下一个请求，丢掉 upgrade 出来的流并不会关连接，
/// 对端收不到 FIN；它也不给 TcpStream。所以对外监听的是我们自己的 listener，字节转发给回环上的 tiny_http，
/// 要断就 shutdown 对外这一端。
/// shortcut: 连接断开清理靠 forward 收尾，没做空闲超时，真有慢连接堆积时再加。
fn serve_front(listener: TcpListener, backend: SocketAddr, stop: Arc<AtomicBool>, events: Arc<Events>) -> JoinHandle<()> {
    let _ = listener.set_nonblocking(true);
    std::thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((client, _)) => {
                    let events = events.clone();
                    std::thread::spawn(move || forward(client, backend, &events));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(50)),
                Err(_) => break,
            }
        }
    })
}

fn forward(client: TcpStream, backend: SocketAddr, events: &Events) {
    let _ = client.set_nonblocking(false);
    let Ok(server) = TcpStream::connect(backend) else { return };
    let (Ok(mut client_in), Ok(mut client_out), Ok(mut server_in), Ok(mut server_out), Ok(tracked)) =
        (client.try_clone(), client.try_clone(), server.try_clone(), server.try_clone(), client.try_clone())
    else {
        return;
    };
    let id = events.next.fetch_add(1, Ordering::SeqCst);
    match events.conns.lock() {
        Ok(mut conns) => conns.push((id, tracked)),
        Err(_) => return,
    }
    let upstream = std::thread::spawn(move || {
        let _ = std::io::copy(&mut client_in, &mut server_out);
        // 只把 EOF 传给后端；客户端半关写端后响应还要回，读端由下行收尾。
        let _ = server_out.shutdown(Shutdown::Write);
    });
    let _ = std::io::copy(&mut server_in, &mut client_out);
    let _ = client.shutdown(Shutdown::Both);
    let _ = server.shutdown(Shutdown::Both);
    let _ = upstream.join();
    if let Ok(mut conns) = events.conns.lock() {
        conns.retain(|(conn_id, _)| *conn_id != id);
    }
}

/// 一帧 SSE：`event: <名>` 加若干行 `data:`；payload 里的换行拆成多行 data，保证不破帧。
fn sse_frame(name: &str, payload: &str) -> String {
    let mut frame = format!("event: {name}\n");
    for line in payload.split('\n') {
        frame.push_str("data: ");
        frame.push_str(line.trim_end_matches('\r'));
        frame.push('\n');
    }
    frame.push('\n');
    frame
}

fn query_token(url: &str) -> Option<&str> {
    url.split_once('?')?.1.split('&').find_map(|pair| pair.strip_prefix("token="))
}

pub struct Gateway {
    events: Arc<Events>,
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
    token_eq(given.trim(), token)
}

fn token_eq(given: &str, token: &str) -> bool {
    let (given, token) = (given.as_bytes(), token.as_bytes());
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

fn arg_strings(args: &Value, key: &str) -> Result<Vec<String>, RpcError> {
    args.get(key)
        .and_then(Value::as_array)
        .and_then(|items| items.iter().map(|item| item.as_str().map(str::to_owned)).collect())
        .ok_or_else(|| RpcError::BadRequest(format!("缺少参数 {key}")))
}

fn out<T: Serialize>(result: Result<T, String>) -> Result<Value, RpcError> {
    let value = result.map_err(RpcError::Failed)?;
    serde_json::to_value(value).map_err(|e| RpcError::Failed(e.to_string()))
}

/// 命令名和参数（camelCase，与前端 invoke 一致）→ 直接调对应的 #[tauri::command] 函数。
fn dispatch(app: &AppHandle, command: &str, args: &Value) -> Result<Value, RpcError> {
    if !allowed(command) {
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
        "brain_live_jobs" => out(tauri::async_runtime::block_on(crate::brain_live_jobs(state(), arg_str(args, "conversationId")?))),
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
        _ => dispatch_write(app, command, args),
    }
}

/// 写命令：参数名和前端 invoke 一致（camelCase）。
fn dispatch_write(app: &AppHandle, command: &str, args: &Value) -> Result<Value, RpcError> {
    use crate::{albion, claude, codex, dsh, hermes, opencode};
    let state = || app.state::<AppState>();
    let room = || arg_str(args, "conversationId");
    let (message, content) = (|| arg_str(args, "messageId"), || arg_str(args, "content"));
    match command {
        "send_codex_message" => out(crate::send_codex_message(app.state::<Arc<codex::Runtime>>(), room()?, message()?, content()?)),
        "send_hermes_message" => out(crate::send_hermes_message(app.state::<Arc<hermes::Runtime>>(), room()?, message()?, content()?)),
        "send_dsh_message" => out(crate::send_dsh_message(app.state::<Arc<dsh::Runtime>>(), room()?, message()?, content()?)),
        "send_claude_message" => out(crate::send_claude_message(app.state::<Arc<claude::Runtime>>(), room()?, message()?, content()?)),
        "send_opencode_message" => out(opencode::send_opencode_message(app.state::<opencode::Runtime>(), room()?, message()?, content()?)),
        "send_albion_message" => out(albion::send_albion_message(app.state::<albion::Runtime>(), room()?, message()?, content()?)),
        "steer_agent_message" => out(tauri::async_runtime::block_on(crate::steer_agent_message(
            state(),
            app.state::<Arc<codex::Runtime>>(),
            app.state::<Arc<claude::Runtime>>(),
            arg_str(args, "agentId")?,
            room()?,
            message()?,
            content()?,
        ))),
        "cancel_codex_run" => out(tauri::async_runtime::block_on(crate::cancel_codex_run(app.state::<Arc<codex::Runtime>>(), room()?))),
        "cancel_hermes_run" => out(crate::cancel_hermes_run(app.state::<Arc<hermes::Runtime>>(), room()?)),
        "cancel_dsh_run" => out(crate::cancel_dsh_run(app.state::<Arc<dsh::Runtime>>(), room()?)),
        "cancel_claude_run" => out(crate::cancel_claude_run(app.state::<Arc<claude::Runtime>>(), room()?)),
        "cancel_opencode_run" => out(opencode::cancel_opencode_run(app.state::<opencode::Runtime>(), room()?)),
        "cancel_albion_run" => out(albion::cancel_albion_run(app.state::<albion::Runtime>(), room()?)),
        "brain_wrap_up" => out(Ok(crate::brain_wrap_up(app.state::<Arc<crate::project_brain::Courier>>(), room()?))),
        "stop_brain_job" => out(crate::stop_brain_job(app.clone(), state(), room()?, arg_str(args, "jobId")?)),
        "answer_chat_question" => out(crate::answer_chat_question(app.clone(), state(), room()?, arg_str(args, "questionId")?, content()?)),
        "answer_project_inquiry" => out(crate::answer_project_inquiry(state(), room()?, arg_str(args, "inquiryId")?, arg_str(args, "answer")?)),
        "stage_attachment" => out(crate::stage_attachment(state(), room()?, arg_str(args, "name")?, arg_str(args, "mime")?, arg_str(args, "data")?)),
        "cancel_chat_question" => out(crate::cancel_chat_question(state(), room()?, arg_str(args, "questionId")?)),
        "start_discussion" => out(crate::discussion::start_discussion(
            app.state::<Arc<crate::discussion::Runtime>>(),
            room()?,
            message()?,
            content()?,
            arg_strings(args, "participants")?,
            args.get("rounds").and_then(Value::as_u64).and_then(|rounds| u32::try_from(rounds).ok()).ok_or_else(|| RpcError::BadRequest("缺少参数 rounds".into()))?,
        )),
        "cancel_discussion" => out(tauri::async_runtime::block_on(crate::discussion::cancel_discussion(app.state::<Arc<crate::discussion::Runtime>>(), arg_str(args, "id")?))),
        "save_local_message" => out(crate::save_local_message(state(), room()?, message()?, content()?)),
        "create_conversation" => out(crate::create_conversation(state(), arg_str(args, "title")?, arg_str(args, "kind")?, arg_strings(args, "members")?)),
        _ => Err(RpcError::Forbidden),
    }
}

fn reply(request: Request, status: u16, content_type: &str, body: Vec<u8>) {
    reply_with(request, status, content_type, body, "no-store", Compression::fast());
}

/// vite 打出来的 `assets/名字-哈希.扩展名` 内容跟文件名绑定、永不变，可以长缓存；index.html 等其余每次都问一遍。
fn cache_policy(path: &str) -> &'static str {
    if path.starts_with("/assets/") { "public, max-age=31536000, immutable" } else { "no-cache" }
}

/// 文本类（脚本、样式、JSON、SVG）压了才有收益；图片、字体本身已压缩。
fn compressible(content_type: &str) -> bool {
    content_type.starts_with("text/") || ["json", "javascript", "svg", "xml"].iter().any(|kind| content_type.contains(kind))
}

fn gzip(body: &[u8], level: Compression) -> Option<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::with_capacity(body.len() / 3), level);
    encoder.write_all(body).ok()?;
    encoder.finish().ok()
}

/// 手机经 Tailscale（可能走中继）下载，整包 JSON 动辄几百 KB 到一两 MB：对方接受 gzip 就压。
fn reply_with(request: Request, status: u16, content_type: &str, body: Vec<u8>, cache: &str, level: Compression) {
    let accepts_gzip = request.headers().iter().any(|h| h.field.equiv("Accept-Encoding") && h.value.as_str().contains("gzip"));
    let packed = if accepts_gzip && body.len() >= 1024 && compressible(content_type) { gzip(&body, level) } else { None };
    let mut headers = vec![("Content-Type", content_type), ("Cache-Control", cache), ("X-Content-Type-Options", "nosniff")];
    let body = match packed {
        Some(packed) => {
            headers.extend([("Content-Encoding", "gzip"), ("Vary", "Accept-Encoding")]);
            packed
        }
        None => body,
    };
    let mut response = Response::from_data(body).with_status_code(status);
    for (name, value) in headers {
        if let Ok(header) = Header::from_bytes(name, value) {
            response.add_header(header);
        }
    }
    let _ = request.respond(response);
}

fn reply_text(request: Request, status: u16, text: &str) {
    reply(request, status, "text/plain; charset=utf-8", text.as_bytes().to_vec());
}

/// SSE：用 upgrade 拿到原始连接自己写帧（tiny_http 的分块响应会缓冲，实时事件出不去）。
fn serve_events(events: &Events, request: Request) {
    let Some((id, rx, alive)) = events.subscribe() else {
        return reply_text(request, 503, "实时连接过多");
    };
    let mut head = Response::empty(200);
    for (name, value) in [("Content-Type", "text/event-stream"), ("Cache-Control", "no-store"), ("X-Accel-Buffering", "no")] {
        if let Ok(header) = Header::from_bytes(name, value) {
            head.add_header(header);
        }
    }
    let mut stream = request.upgrade("sse", head);
    let mut send = |frame: &str| stream.write_all(frame.as_bytes()).and_then(|_| stream.flush()).is_ok();
    let mut open = send(": connected\n\n");
    while open && alive.load(Ordering::SeqCst) {
        open = match rx.recv_timeout(PING_EVERY) {
            Ok(frame) => alive.load(Ordering::SeqCst) && send(&frame),
            Err(RecvTimeoutError::Timeout) => send(": ping\n\n"),
            Err(RecvTimeoutError::Disconnected) => false,
        };
    }
    events.remove(id);
}

fn handle(app: &AppHandle, token: &Mutex<String>, events: &Events, mut request: Request) {
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
        let limit = if command == "stage_attachment" { MAX_ATTACHMENT_BODY } else { MAX_BODY };
        let mut body = String::new();
        if request.as_reader().take(limit + 1).read_to_string(&mut body).is_err() {
            return reply_text(request, 400, "请求体读取失败");
        }
        if body.len() as u64 > limit {
            return reply_text(request, 413, "请求体过大");
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
    if path == "/events" {
        let valid = query_token(&url).zip(token.lock().ok()).is_some_and(|(given, token)| token_eq(given, &token));
        if !valid {
            return reply_text(request, 401, "未授权");
        }
        return serve_events(events, request);
    }
    // 页面本身不含数据，静态资源不鉴权；资源表是打包时固定的，这里只挡掉路径穿越。
    if path.contains("..") || path.contains('\\') {
        return reply_text(request, 400, "路径无效");
    }
    let cache = cache_policy(&path);
    match app.asset_resolver().get(path) {
        Some(asset) => reply_with(request, 200, &asset.mime_type, asset.bytes, cache, Compression::default()),
        None => reply_text(request, 404, "没有这个页面"),
    }
}

impl Gateway {
    fn stop(&self) {
        let Ok(mut running) = self.running.lock() else { return };
        self.events.close_all();
        if let Some(stop) = running.stop.take() {
            stop.store(true, Ordering::SeqCst);
        }
        for thread in [running.front.take(), running.thread.take()].into_iter().flatten() {
            let _ = thread.join();
        }
        running.address = None;
    }

    /// 起服务；失败只记原因，不算命令错误。
    fn start(&self, app: &AppHandle) {
        self.stop();
        let result = tailscale_ip().and_then(|ip| {
            let address = SocketAddr::from((ip, gateway_port()));
            let listener = TcpListener::bind(address).map_err(|e| format!("绑定 {address} 失败：{e}"))?;
            let server = Server::http("127.0.0.1:0").map_err(|e| format!("起内部服务失败：{e}"))?;
            let backend = server.server_addr().to_ip().ok_or("内部服务地址无效")?;
            Ok((address, listener, server, backend))
        });
        let Ok(mut running) = self.running.lock() else { return };
        match result {
            Err(reason) => running.error = Some(reason),
            Ok((address, listener, server, backend)) => {
                let stop = Arc::new(AtomicBool::new(false));
                let (flag, app, token, events) = (stop.clone(), app.clone(), self.token.clone(), self.events.clone());
                running.front = Some(serve_front(listener, backend, stop.clone(), events.clone()));
                running.thread = Some(std::thread::spawn(move || {
                    while !flag.load(Ordering::SeqCst) {
                        match server.recv_timeout(Duration::from_millis(300)) {
                            Ok(Some(request)) => {
                                let (app, token, events) = (app.clone(), token.clone(), events.clone());
                                std::thread::spawn(move || handle(&app, &token, &events, request));
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
    let events = Arc::new(Events::default());
    let latest = Arc::new(Latest::default());
    for name in EVENTS {
        let (events, latest) = (events.clone(), latest.clone());
        if COALESCED.contains(name) {
            app.listen_any(*name, move |event| latest.put(name, event.payload()));
        } else {
            app.listen_any(*name, move |event| events.broadcast(&sse_frame(name, event.payload())));
        }
    }
    let flush = events.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(COALESCE_EVERY);
        for frame in latest.take() {
            flush.broadcast(&frame);
        }
    });
    let gateway = Gateway { events, token: Arc::new(Mutex::new(token)), running: Mutex::new(Running::default()) };
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
    gateway.events.close_all();
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
        for command in WRITE_COMMANDS {
            for danger in ["delete", "archive", "rename", "connect", "disconnect", "update", "install", "avatar", "paste", "attach_native", "set_", "remove", "remote_gateway", "open_", "register", "bind", "add_"] {
                assert!(!command.contains(danger), "写表里不该有 {command}");
            }
            assert!(!READ_COMMANDS.contains(command), "{command} 同时在读表和写表");
        }
        assert!(allowed("send_claude_message") && allowed("stage_attachment") && !allowed("delete_conversation") && !allowed("connect_claude"));
        assert!(READ_COMMANDS.contains(&"list_conversations") && READ_COMMANDS.contains(&"dsh_status") && !READ_COMMANDS.contains(&"connect_dsh"));
        assert!(READ_COMMANDS.contains(&"background_image") && !READ_COMMANDS.contains(&"add_background") && !READ_COMMANDS.contains(&"remove_background"));
    }

    #[test]
    fn latest_keeps_only_newest_frame_per_event() {
        let latest = Latest::default();
        for revision in 0..50 {
            latest.put("claude-state", &format!("{{\"revision\":{revision}}}"));
        }
        latest.put("codex-state", "{\"revision\":1}");
        latest.put("claude-state", "{\"revision\":50}");
        assert_eq!(latest.take(), vec![sse_frame("claude-state", "{\"revision\":50}"), sse_frame("codex-state", "{\"revision\":1}")]);
        assert!(latest.take().is_empty(), "放出去的不会再发第二遍");
        assert!(COALESCED.iter().all(|name| EVENTS.contains(name)));
        assert!(!COALESCED.contains(&"brain-state"), "brain-state 载荷是各自不同的会话 id，不是快照，不能合并");
    }

    #[test]
    fn coalescing_never_loses_a_models_change() {
        let latest = Latest::default();
        latest.put("claude-state", r#"{"revision":1,"models":[{"id":"new"}],"runs":[]}"#);
        latest.put("claude-state", r#"{"revision":2,"runs":[{"text":"提到 \"models\" 的回复"}]}"#);
        let frame = latest.take().pop().unwrap();
        let data: Value = serde_json::from_str(frame.lines().find(|line| line.starts_with("data: ")).unwrap().trim_start_matches("data: ")).unwrap();
        assert_eq!(data["revision"], 2, "留最新一帧");
        assert_eq!(data["models"][0]["id"], "new", "被覆盖那帧带的模型表要并进来");
        latest.put("claude-state", r#"{"revision":3,"models":[{"id":"newer"}]}"#);
        latest.put("claude-state", r#"{"revision":4,"models":[{"id":"newest"}]}"#);
        assert!(latest.take().pop().unwrap().contains("newest"), "后一帧自己带模型表就用它的");
        assert_eq!(keep_models(r#"{"a":1}"#, r#"{"b":2}"#.into()), r#"{"b":2}"#, "旧帧没有模型表就原样");
    }

    #[test]
    fn hashed_assets_cache_forever_and_pages_revalidate() {
        assert!(cache_policy("/assets/index-BPXGmM66.js").contains("immutable"));
        assert!(cache_policy("/assets/KaTeX_Main-Regular-B22Nviop.woff2").contains("immutable"));
        for page in ["/", "/index.html", "/favicon.ico"] {
            assert_eq!(cache_policy(page), "no-cache", "{page} 不能长缓存，否则发新版后手机还在用旧页面");
        }
    }

    #[test]
    fn gzip_only_text_and_round_trips() {
        assert!(compressible("application/json") && compressible("text/javascript") && compressible("text/css") && compressible("image/svg+xml"));
        assert!(!compressible("font/woff2") && !compressible("image/webp") && !compressible("application/octet-stream"));
        let body = r#"{"messages":[{"content":"同席同席同席同席"}]}"#.repeat(200).into_bytes();
        let packed = gzip(&body, Compression::fast()).unwrap();
        assert!(packed.len() < body.len() / 4);
        let mut back = Vec::new();
        flate2::read::GzDecoder::new(&packed[..]).read_to_end(&mut back).unwrap();
        assert_eq!(back, body);
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

    #[test]
    fn sse_frames_split_lines_and_read_token_from_query() {
        assert_eq!(sse_frame("brain-state", "\"room\""), "event: brain-state\ndata: \"room\"\n\n");
        assert_eq!(sse_frame("x", "a\r\nb"), "event: x\ndata: a\ndata: b\n\n");
        assert_eq!(query_token("/events?token=abc"), Some("abc"));
        assert_eq!(query_token("/events?a=1&token=abc"), Some("abc"));
        assert_eq!(query_token("/events"), None);
        assert!(token_eq("abc", "abc") && !token_eq("abd", "abc") && !token_eq("ab", "abc"));
    }

    #[test]
    fn events_broadcast_and_close_all_cut_clients() {
        let events = Events::default();
        let (_, rx, alive) = events.subscribe().unwrap();
        events.broadcast("hello");
        assert_eq!(&*rx.recv().unwrap(), "hello");
        events.close_all();
        assert!(!alive.load(Ordering::SeqCst));
        assert!(events.clients.lock().unwrap().is_empty());
        let _held: Vec<_> = (0..MAX_EVENT_CLIENTS).map(|_| events.subscribe().unwrap()).collect();
        assert!(events.subscribe().is_none());
    }

    /// 真起一个 tiny_http，确认 SSE 帧不经缓冲、立刻到客户端，且 close_all 之后连接收线。
    #[test]
    fn sse_stream_flushes_immediately_and_closes() {
        use std::io::BufRead;
        let server = Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let events = Arc::new(Events::default());
        let served = events.clone();
        let worker = std::thread::spawn(move || serve_events(&served, server.recv().unwrap()));
        let mut client = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        client.write_all(b"GET /events HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut reader = std::io::BufReader::new(client);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(line.starts_with("HTTP/1.1 200"), "{line}");
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
        }
        line.clear();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line, ": connected\n");
        events.broadcast(&sse_frame("dsh-state", "{\"revision\":1}"));
        line.clear();
        reader.read_line(&mut line).unwrap();
        reader.read_line(&mut line).unwrap();
        assert!(line.contains("event: dsh-state\n"), "{line}");
        events.close_all();
        events.broadcast("ignored");
        worker.join().unwrap();
    }

    /// close_all 后对端必须真的收到 FIN（read 返回 0 或出错），不能只是服务端线程退出。
    #[test]
    fn close_all_makes_client_see_eof() {
        use std::io::Read;
        let server = Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let events = Arc::new(Events::default());
        let served = events.clone();
        let _worker = std::thread::spawn(move || serve_events(&served, server.recv().unwrap()));
        let front = TcpListener::bind("127.0.0.1:0").unwrap();
        let front_port = front.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let front_thread = serve_front(front, SocketAddr::from(([127, 0, 0, 1], port)), stop.clone(), events.clone());
        let mut client = std::net::TcpStream::connect(("127.0.0.1", front_port)).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        client.write_all(b"GET /events HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut seen = Vec::new();
        let mut buf = [0u8; 256];
        while !String::from_utf8_lossy(&seen).contains(": connected\n") {
            let n = client.read(&mut buf).unwrap();
            assert!(n > 0, "握手前连接就断了");
            seen.extend_from_slice(&buf[..n]);
        }
        events.close_all();
        client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let eof = loop {
            match client.read(&mut buf) {
                Ok(0) => break true,
                Ok(_) => continue,
                Err(e) => break !matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut),
            }
        };
        assert!(eof, "close_all 之后 2 秒内客户端没收到 EOF");
        stop.store(true, Ordering::SeqCst);
        front_thread.join().unwrap();
    }

    /// 客户端发完请求就半关写端，转发层不能把还没回完的响应一起切掉。
    #[test]
    fn half_closed_client_still_gets_response() {
        use std::io::Read;
        let backend = TcpListener::bind("127.0.0.1:0").unwrap();
        let backend_addr = backend.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut conn, _) = backend.accept().unwrap();
            let mut request = Vec::new();
            conn.read_to_end(&mut request).unwrap(); // 读到客户端的 EOF 才会返回
            std::thread::sleep(Duration::from_millis(200));
            conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok").unwrap();
        });
        let front = TcpListener::bind("127.0.0.1:0").unwrap();
        let front_addr = front.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let front_thread = serve_front(front, backend_addr, stop.clone(), Arc::new(Events::default()));
        let mut client = TcpStream::connect(front_addr).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        client.write_all(b"GET /nope HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK") && response.ends_with("ok"), "{response}");
        stop.store(true, Ordering::SeqCst);
        front_thread.join().unwrap();
    }
}
