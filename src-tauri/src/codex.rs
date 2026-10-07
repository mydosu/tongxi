use crate::models::{codex_models, validate_selection, ModelOption};
use crate::rpc::Client;
use crate::store::{RunRecord, Store};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};
use tauri::Emitter;
use uuid::Uuid;

type Result<T> = std::result::Result<T, String>;
#[derive(Clone, Serialize)]
pub struct RunSnapshot {
    #[serde(flatten)]
    pub record: RunRecord,
    pub text: String,
    /// 后端推理块；没有推理的 runtime 留空。
    #[serde(default)]
    pub thought: String,
}

/// 只读的原生会话条目（各 agent 自己的历史），供「原生会话」面板展示。
#[derive(Clone, serde::Serialize)]
pub struct NativeSession {
    pub id: String,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub updated_at: Option<String>,
    /// 占用它的同席会话标题（None = 空闲，可接入）；由命令层填。
    pub occupied_by: Option<String>,
    /// 是否已经是这个同席会话当前绑定的原生会话；由命令层填。
    pub current: bool,
}

/// ACP `session/list` 的响应 → 只读列表；字段缺失就留空，不猜。
pub(crate) fn native_sessions_from_acp(value: &serde_json::Value) -> Vec<NativeSession> {
    value["sessions"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let id = item["sessionId"].as_str()?.to_string();
                    Some(NativeSession {
                        id,
                        title: item["title"].as_str().map(str::to_string),
                        cwd: item["cwd"].as_str().map(str::to_string),
                        updated_at: item["updatedAt"].as_str().map(str::to_string),
                        occupied_by: None,
                        current: false,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Clone, Serialize)]
pub struct RuntimeSnapshot {
    pub revision: u64,
    pub connection: String,
    pub executable: Option<String>,
    pub version: Option<String>,
    pub error: Option<String>,
    pub active: Option<RunSnapshot>,
    pub models: Vec<ModelOption>,
    pub default_model: Option<String>,
    pub default_effort: Option<String>,
}

struct ActiveRun {
    snapshot: RunSnapshot,
    items: BTreeMap<String, String>,
    order: Vec<String>,
    last_checkpoint: Instant,
}

fn busy(status: &str) -> bool {
    matches!(status, "starting" | "running" | "cancelling")
}

impl ActiveRun {
    /// 推理增量直接累加：思考块不分段、不参与正文拼接。
    fn thought(&mut self, delta: &str) {
        self.snapshot.thought.push_str(delta);
    }
    fn text(&mut self, item: &str, text: &str, replace: bool) {
        if !self.items.contains_key(item) {
            self.order.push(item.into());
        }
        let content = self.items.entry(item.into()).or_default();
        if replace {
            *content = text.into();
        } else {
            content.push_str(text);
        }
        self.snapshot.text = self
            .order
            .iter()
            .filter_map(|id| self.items.get(id))
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n");
    }

    fn accepts(&self, params: &Value) -> bool {
        busy(&self.snapshot.record.status)
            && params["threadId"].as_str() == self.snapshot.record.native_thread_id.as_deref()
            && self.snapshot.record.native_thread_id.is_some()
            && (self.snapshot.record.native_turn_id.is_none()
                || params
                    .get("turnId")
                    .and_then(Value::as_str)
                    .or_else(|| params["turn"]["id"].as_str())
                    == self.snapshot.record.native_turn_id.as_deref())
    }
}

struct Inner {
    client: Option<Arc<Client>>,
    connection_id: String,
    connection: String,
    version: Option<String>,
    error: Option<String>,
    active: Option<ActiveRun>,
    models: Vec<ModelOption>,
    default_model: Option<String>,
    default_effort: Option<String>,
}

pub struct Runtime {
    revision: AtomicU64,
    inner: Mutex<Inner>,
    store: Arc<Mutex<Store>>,
    app: tauri::AppHandle,
    executable: Mutex<Option<PathBuf>>,
    directory: PathBuf,
}

/// 同席自己管的 Codex 槽位（`<data>/managed-services/codex-win` 的激活槽）里的 codex.exe。
///
/// `discover_executable()` 是自由函数、拿不到数据目录，所以由 `codex::Runtime::new` 在启动时填，
/// 并在「启用候选 / 回退 / 重启该成员」时重新填一次——可刷新，否则切换槽位要重启整个 app 才生效。
static MANAGED_EXECUTABLE: Mutex<Option<PathBuf>> = Mutex::new(None);

/// 记录受管槽位里的 exe：没有激活槽就清空（回退到外部安装），槽位坏了就把错误交回调用方。
pub(crate) fn adopt_managed_executable(data: &std::path::Path) -> Result<()> {
    let resolved =
        crate::service_install::active_executable(data, &crate::service_install::Layout::CODEX)?;
    *MANAGED_EXECUTABLE.lock().unwrap() = resolved;
    Ok(())
}

pub(crate) fn discover_executable() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("AGENT_HUB_CODEX_EXE") {
        let path = PathBuf::from(path);
        return (path.is_absolute()
            && path.is_file()
            && path
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case("codex.exe")))
        .then_some(path);
    }
    // 同席自己管的槽位优先于 PATH / 常见安装位置。
    if let Some(path) = MANAGED_EXECUTABLE.lock().unwrap().clone() {
        if path.is_file() {
            return Some(path);
        }
    }
    // 配置的外部安装也优先于 PATH：回退到外部安装之后，跑的必须是**那一份**，
    // 而不是 PATH 上碰巧更新的另一份（否则版本对不上，回退会被判失败）。
    if let Ok(external) = crate::service_install::external(&crate::service_install::Layout::CODEX) {
        if let Some(path) = crate::service_updates::local_or_nested_executable(
            &external,
            &crate::service_install::Layout::CODEX,
        ) {
            return Some(path);
        }
    }
    let mut roots: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|value| std::env::split_paths(&value).collect())
        .unwrap_or_default();
    if let Some(app_data) = std::env::var_os("APPDATA") {
        roots.push(PathBuf::from(app_data).join("npm"));
    }
    for root in roots {
        for path in [
            root.join("codex.exe"),
            // 本地 prefix / 受管槽：平台包与主包平级。
            root.join(
                "node_modules/@openai/codex-win32-x64/vendor/x86_64-pc-windows-msvc/bin/codex.exe",
            ),
            // npm -g 的全局前缀：平台包嵌套在主包里面（且只有 codex / codex.cmd 壳，没有 codex.exe）。
            root.join(
                "node_modules/@openai/codex/node_modules/@openai/codex-win32-x64/vendor/x86_64-pc-windows-msvc/bin/codex.exe",
            ),
        ] {
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

pub(crate) fn native_version(executable: &PathBuf) -> Option<String> {
    let mut command = Command::new(executable);
    command.arg("--version").stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|value| value.trim().strip_prefix("codex-cli ").map(str::to_owned))
}

impl Runtime {
    /// 只读列出 codex 自己的历史会话（app-server 的 thread/list）。
    pub fn list_sessions(&self) -> Result<Vec<NativeSession>> {
        let client = {
            let inner = self.inner.lock().unwrap();
            inner
                .client
                .clone()
                .ok_or_else(|| "Codex 未连接".to_string())?
        };
        let response = client.rpc_timeout(
            "thread/list",
            serde_json::json!({"limit": 50}),
            std::time::Duration::from_secs(20),
        )?;
        let items = ["data", "threads", "sessions"]
            .iter()
            .find_map(|key| response[*key].as_array().cloned())
            .or_else(|| response.as_array().cloned())
            .ok_or_else(|| {
                format!(
                    "Codex 会话列表格式未知：{}",
                    response
                        .as_object()
                        .map(|object| object.keys().cloned().collect::<Vec<_>>().join(","))
                        .unwrap_or_default()
                )
            })?;
        Ok(items
            .iter()
            .filter_map(|item| {
                let id = item["id"]
                    .as_str()
                    .or_else(|| item["threadId"].as_str())?
                    .to_string();
                Some(NativeSession {
                    id,
                    title: item["title"]
                        .as_str()
                        .or_else(|| item["preview"].as_str())
                        .map(str::to_string),
                    cwd: item["cwd"].as_str().map(str::to_string),
                    updated_at: item["updatedAt"]
                        .as_str()
                        .or_else(|| item["createdAt"].as_str())
                        .map(str::to_string),
                    occupied_by: None,
                    current: false,
                })
            })
            .collect())
    }

    pub fn new(app: tauri::AppHandle, store: Arc<Mutex<Store>>, directory: PathBuf) -> Arc<Self> {
        // 受管槽位优先：同席自己装的那份 Codex 在 `<data>/managed-services/codex-win` 里。
        // 槽位存在但坏了**不静默回退**——把原因留在连接错误里，让人看得见。
        let slot_error = if directory.is_dir() {
            adopt_managed_executable(&directory).err()
        } else {
            None
        };
        Arc::new(Self {
            revision: AtomicU64::new(1),
            inner: Mutex::new(Inner {
                client: None,
                connection_id: String::new(),
                connection: "disconnected".into(),
                version: None,
                error: slot_error,
                active: None,
                models: Vec::new(),
                default_model: None,
                default_effort: None,
            }),
            store,
            app,
            executable: Mutex::new(discover_executable()),
            directory,
        })
    }

    pub fn snapshot(&self) -> RuntimeSnapshot {
        let inner = self.inner.lock().unwrap();
        RuntimeSnapshot {
            revision: self.revision.fetch_add(1, Ordering::Relaxed),
            connection: inner.connection.clone(),
            executable: self
                .executable
                .lock()
                .unwrap()
                .as_ref()
                .map(|path| path.to_string_lossy().into()),
            version: inner.version.clone(),
            error: inner.error.clone(),
            active: inner.active.as_ref().map(|run| run.snapshot.clone()),
            models: inner.models.clone(),
            default_model: inner.default_model.clone(),
            default_effort: inner.default_effort.clone(),
        }
    }

    fn emit(&self) {
        let _ = self.app.emit("codex-state", self.snapshot());
    }

    pub fn connect(self: &Arc<Self>) -> Result<RuntimeSnapshot> {
        // 每次连接都重新发现：受管槽位切换（启用候选 / 回退）之后重连就该用新槽，不留旧路径。
        let executable = discover_executable()
            .ok_or("未找到原生 codex.exe，请检查 PATH 或 AGENT_HUB_CODEX_EXE")?;
        *self.executable.lock().unwrap() = Some(executable.clone());
        let connection_id = Uuid::new_v4().to_string();
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.connection == "connected" {
                drop(inner);
                return Ok(self.snapshot());
            }
            if inner.connection == "connecting" {
                return Err("Codex 正在连接".into());
            }
            inner.connection = "connecting".into();
            inner.error = None;
            inner.connection_id = connection_id.clone();
        }
        self.emit();
        let weak = Arc::downgrade(self);
        let exit_weak = weak.clone();
        let message_id = connection_id.clone();
        let exit_id = connection_id.clone();
        let outcome: Result<()> = (|| {
            let client = Client::spawn(
                &executable,
                move |value| {
                    if let Some(runtime) = weak.upgrade() {
                        runtime.notification(&message_id, value);
                    }
                },
                move || {
                    if let Some(runtime) = exit_weak.upgrade() {
                        runtime.connection_lost(&exit_id);
                    }
                },
            )?;
            {
                self.inner.lock().unwrap().client = Some(client.clone());
            }
            client.rpc("initialize", json!({"clientInfo":{"name":"personal_agent_hub","title":"同席 Agent Hub","version":env!("CARGO_PKG_VERSION")}}))?;
            client.write(&json!({"method":"initialized","params":{}}))?;
            let mut models = Vec::new();
            let mut cursor = Value::Null;
            for _ in 0..16 {
                let page = client.rpc(
                    "model/list",
                    json!({"limit":100,"cursor":cursor,"includeHidden":false}),
                )?;
                models.extend(codex_models(&page));
                cursor = page["nextCursor"].clone();
                if cursor.is_null() {
                    break;
                }
            }
            // An ephemeral, unprompted thread reports the effective local defaults.
            let defaults = client.rpc(
                "thread/start",
                json!({"ephemeral":true,"approvalPolicy":"never","sandbox":"read-only"}),
            )?;
            let version = native_version(&executable);
            let mut inner = self.inner.lock().unwrap();
            if inner.connection_id != connection_id || inner.connection != "connecting" {
                return Err("Codex 连接已取消".into());
            }
            inner.version = version;
            inner.models = models;
            inner.default_model = defaults["model"].as_str().map(String::from);
            inner.default_effort = defaults["reasoningEffort"].as_str().map(String::from);
            inner.connection = "connected".into();
            Ok(())
        })();
        if let Err(error) = outcome {
            self.stop();
            self.inner.lock().unwrap().error = Some(error.clone());
            self.emit();
            return Err(error);
        }
        self.emit();
        Ok(self.snapshot())
    }

    pub fn mutate<T>(&self, id: &str, action: impl FnOnce(&mut Store) -> Result<T>) -> Result<T> {
        let inner = self.inner.lock().map_err(|_| "运行状态不可用")?;
        if inner.active.as_ref().is_some_and(|run| {
            busy(&run.snapshot.record.status) && run.snapshot.record.conversation_id == id
        }) {
            return Err("会话正在运行，请先停止并等待完成".into());
        }
        let mut store = self.store.lock().map_err(|_| "存储不可用")?;
        action(&mut store)
    }

    pub fn send(
        self: &Arc<Self>,
        conversation_id: &str,
        message_id: &str,
        content: &str,
    ) -> Result<RunRecord> {
        let mut inner = self.inner.lock().map_err(|_| "运行状态不可用")?;
        {
            let mut store = self.store.lock().map_err(|_| "存储不可用")?;
            store.validate_private_target(conversation_id, "codex-win")?;
            if let Some(existing) = store.run_for_message(message_id)? {
                store.save_message(conversation_id, message_id, content)?;
                return Ok(existing);
            }
        }
        if inner.connection != "connected" {
            return Err("请先连接 Codex".into());
        }
        if inner
            .active
            .as_ref()
            .is_some_and(|run| busy(&run.snapshot.record.status))
        {
            return Err("Codex 正在处理上一条消息，请等待或停止".into());
        }
        let client = inner.client.clone().ok_or("Codex 接口不可用")?;
        let settings = self
            .store
            .lock()
            .map_err(|_| "存储不可用")?
            .detail(conversation_id)?
            .sessions
            .into_iter()
            .find(|session| session.agent_id == "codex-win")
            .ok_or("Codex 会话设置不存在")?;
        validate_selection(
            &inner.models,
            settings.model.as_deref(),
            settings.reasoning_effort.as_deref(),
            inner.default_model.as_deref(),
        )?;
        let record = self.store.lock().map_err(|_| "存储不可用")?.begin_run(
            conversation_id,
            message_id,
            content,
        )?;
        inner.active = Some(ActiveRun {
            snapshot: RunSnapshot {
                record: record.clone(),
                text: String::new(),
                thought: String::new(),
            },
            items: BTreeMap::new(),
            order: Vec::new(),
            last_checkpoint: Instant::now(),
        });
        drop(inner);
        self.emit();
        let runtime = self.clone();
        let run_id = record.id.clone();
        let content = content.trim().to_owned();
        std::thread::spawn(move || {
            if let Err(error) = runtime.start_turn(&client, &run_id, &content) {
                runtime.abort(&run_id, error);
            }
        });
        Ok(record)
    }

    pub(crate) fn send_discussion(
        self: &Arc<Self>,
        discussion: &str,
        round: u32,
    ) -> Result<RunRecord> {
        let mut inner = self.inner.lock().map_err(|_| "运行状态不可用")?;
        if inner.connection != "connected"
            || inner
                .active
                .as_ref()
                .is_some_and(|r| busy(&r.snapshot.record.status))
        {
            return Err("Codex 尚未连接或正在其他会话回复".into());
        }
        let client = inner.client.clone().ok_or("Codex 接口不可用")?;
        let (record, text) =
            self.store
                .lock()
                .unwrap()
                .begin_discussion_turn(discussion, "codex-win", round)?;
        inner.active = Some(ActiveRun {
            snapshot: RunSnapshot {
                record: record.clone(),
                text: String::new(),
                thought: String::new(),
            },
            items: BTreeMap::new(),
            order: Vec::new(),
            last_checkpoint: Instant::now(),
        });
        drop(inner);
        self.emit();
        let runtime = self.clone();
        let run_id = record.id.clone();
        std::thread::spawn(move || {
            if let Err(error) = runtime.start_turn(&client, &run_id, &text) {
                runtime.abort(&run_id, error);
            }
        });
        Ok(record)
    }

    fn start_turn(&self, client: &Client, run_id: &str, content: &str) -> Result<()> {
        let (conversation_id, previous, native_cwd) = {
            let inner = self.inner.lock().unwrap();
            let run = inner
                .active
                .as_ref()
                .filter(|run| run.snapshot.record.id == run_id)
                .ok_or("运行已取消")?;
            if run.snapshot.record.status == "cancelling" {
                drop(inner);
                self.finish(run_id, "interrupted", None);
                return Ok(());
            }
            let detail = self
                .store
                .lock()
                .unwrap()
                .detail(&run.snapshot.record.conversation_id)?;
            let session = detail
                .sessions
                .into_iter()
                .find(|session| session.agent_id == "codex-win");
            (
                detail.conversation.id,
                session
                    .as_ref()
                    .and_then(|session| session.native_session_id.clone()),
                session.and_then(|session| session.native_cwd),
            )
        };
        // 接入的原生会话回到它自己的工作目录；resume 时目录不对会被原生实现拒绝。
        let cwd = match native_cwd.as_deref() {
            Some(native) => std::path::PathBuf::from(native),
            None => {
                let local = self
                    .directory
                    .join("codex-workspaces")
                    .join(&conversation_id);
                std::fs::create_dir_all(&local).map_err(|_| "Codex 会话工作目录创建失败")?;
                local
            }
        };
        let mut params = json!({"cwd":cwd,"approvalPolicy":"never","sandbox":"read-only",
            "developerInstructions":"你正在同席桌面软件的独立 Codex 会话中，可能是私聊或群内发言。本阶段仅交流和方案分析，没有绑定用户项目。按用户角色约定，Codex 只做方案与根因分析/复核，不输出完整实现或补丁，不自行写码；需要实际编码交 DSH 执行。不要读取凭据或其他会话，不发送外部消息。仅使用收到的公开讨论记录，不运行工具。"});
        let (model, effort) = {
            let inner = self.inner.lock().unwrap();
            let settings = &inner
                .active
                .as_ref()
                .filter(|run| run.snapshot.record.id == run_id)
                .ok_or("运行已结束")?
                .snapshot
                .record;
            let model = settings
                .model
                .clone()
                .or_else(|| inner.default_model.clone());
            let effort = settings.reasoning_effort.clone().or_else(|| {
                if settings.model.is_some() {
                    inner
                        .models
                        .iter()
                        .find(|entry| Some(&entry.id) == model.as_ref())
                        .and_then(|entry| entry.default_effort.clone())
                } else {
                    inner.default_effort.clone()
                }
            });
            validate_selection(
                &inner.models,
                model.as_deref(),
                effort.as_deref(),
                inner.default_model.as_deref(),
            )?;
            (model, effort)
        };
        params["model"] = json!(model);
        params["config"] = json!({"model_reasoning_effort":effort});
        let method = if let Some(thread_id) = previous {
            params["threadId"] = json!(thread_id);
            params["excludeTurns"] = json!(true);
            "thread/resume"
        } else {
            "thread/start"
        };
        let thread = client.rpc(method, params)?;
        let thread_id = thread["thread"]["id"]
            .as_str()
            .ok_or("Codex 未返回原生会话 ID")?
            .to_owned();
        {
            let mut inner = self.inner.lock().unwrap();
            let run = inner
                .active
                .as_mut()
                .filter(|run| run.snapshot.record.id == run_id && busy(&run.snapshot.record.status))
                .ok_or("运行已结束")?;
            self.store
                .lock()
                .unwrap()
                .bind_thread(&mut run.snapshot.record, &thread_id)?;
            if run.snapshot.record.status == "cancelling" {
                drop(inner);
                self.finish(run_id, "interrupted", None);
                return Ok(());
            }
        }
        let turn = client.rpc("turn/start", json!({"threadId":thread_id,"clientUserMessageId":self.snapshot().active.as_ref().map(|run|run.record.user_message_id.clone()),"input":[{"type":"text","text":content}],"model":model,"effort":effort,"approvalPolicy":"never","sandboxPolicy":{"type":"readOnly"}}))?;
        let turn_id = turn["turn"]["id"]
            .as_str()
            .ok_or("Codex 未返回运行 ID")?
            .to_owned();
        // A loaded thread's resume response can describe the previous turn.
        // Record the acknowledged turn/start overrides for this run instead.
        {
            let mut inner = self.inner.lock().unwrap();
            if let Some(run) = inner
                .active
                .as_mut()
                .filter(|run| run.snapshot.record.id == run_id)
            {
                self.store
                    .lock()
                    .unwrap()
                    .record_model(&mut run.snapshot.record, model, effort)?;
            }
        }
        let cancelling =
            {
                let mut inner = self.inner.lock().unwrap();
                if let Some(run) = inner.active.as_mut().filter(|run| {
                    run.snapshot.record.id == run_id && busy(&run.snapshot.record.status)
                }) {
                    self.store
                        .lock()
                        .unwrap()
                        .mark_turn(&mut run.snapshot.record, &turn_id)?;
                    run.snapshot.record.status == "cancelling"
                } else {
                    false
                }
            };
        self.emit();
        if cancelling {
            client.rpc(
                "turn/interrupt",
                json!({"threadId":thread_id,"turnId":turn_id}),
            )?;
        }
        Ok(())
    }

    pub fn cancel(&self, conversation_id: &str) -> Result<RuntimeSnapshot> {
        self.cancel_expected(conversation_id, None)
    }
    pub(crate) fn cancel_expected(
        &self,
        conversation_id: &str,
        expected: Option<&str>,
    ) -> Result<RuntimeSnapshot> {
        let (client, thread_id, turn_id, run_id) = {
            let mut inner = self.inner.lock().unwrap();
            let client = inner.client.clone().ok_or("Codex 未连接")?;
            let run = inner
                .active
                .as_mut()
                .filter(|run| {
                    run.snapshot.record.conversation_id == conversation_id
                        && busy(&run.snapshot.record.status)
                        && expected.is_none_or(|id| run.snapshot.record.id == id)
                })
                .ok_or("这个会话没有正在运行的任务")?;
            run.snapshot.record.status = "cancelling".into();
            self.store.lock().unwrap().checkpoint(
                &run.snapshot.record,
                &run.snapshot.text,
                &run.snapshot.thought,
            )?;
            (
                client,
                run.snapshot.record.native_thread_id.clone(),
                run.snapshot.record.native_turn_id.clone(),
                run.snapshot.record.id.clone(),
            )
        };
        self.emit();
        if let (Some(thread_id), Some(turn_id)) = (thread_id, turn_id) {
            if let Err(error) = client.rpc(
                "turn/interrupt",
                json!({"threadId":thread_id,"turnId":turn_id}),
            ) {
                // An unconfirmed interrupt must never permit a second concurrent turn.
                self.inner.lock().unwrap().error = Some(error);
                self.emit();
                return Err("停止未确认，请断开连接以终止原生进程".into());
            }
        }
        let _ = run_id;
        Ok(self.snapshot())
    }

    fn notification(&self, connection_id: &str, value: Value) {
        let mut inner = self.inner.lock().unwrap();
        if inner.connection_id != connection_id {
            return;
        }
        let Some(run) = inner.active.as_mut() else {
            return;
        };
        let params = &value["params"];
        if !run.accepts(params) {
            return;
        }
        let method = value["method"].as_str().unwrap_or("");
        if method != "turn/started" && run.snapshot.record.native_turn_id.is_none() {
            return;
        }
        let mut final_event = false;
        match method {
            "turn/started" => {
                if let Some(turn_id) = params["turn"]["id"].as_str() {
                    if self
                        .store
                        .lock()
                        .unwrap()
                        .mark_turn(&mut run.snapshot.record, turn_id)
                        .is_err()
                    {
                        inner.error = Some("原生运行状态保存失败".into());
                    }
                }
            }
            // codex 的推理流：正文增量和摘要增量都汇总到同一个思考块。
            "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta" => {
                if let Some(delta) = params["delta"].as_str() {
                    run.thought(delta);
                }
            }
            "item/agentMessage/delta" => {
                if let (Some(item), Some(delta)) =
                    (params["itemId"].as_str(), params["delta"].as_str())
                {
                    run.text(item, delta, false);
                }
            }
            "item/completed" if params["item"]["type"] == "agentMessage" => {
                if let (Some(item), Some(text)) = (
                    params["item"]["id"].as_str(),
                    params["item"]["text"].as_str(),
                ) {
                    run.text(item, text, true);
                }
            }
            "turn/completed" => {
                run.snapshot.record.status = match params["turn"]["status"].as_str() {
                    Some("completed") => "completed",
                    Some("interrupted") => "interrupted",
                    _ => "failed",
                }
                .into();
                if run.snapshot.record.status == "failed" {
                    run.snapshot.record.error =
                        Some("Codex 原生运行失败，请检查本机模型连接；可以发送新消息重试".into());
                }
                final_event = true;
            }
            _ => return,
        }
        let run = inner.active.as_mut().unwrap();
        if final_event || run.last_checkpoint.elapsed() > Duration::from_millis(350) {
            if self
                .store
                .lock()
                .unwrap()
                .checkpoint(
                    &run.snapshot.record,
                    &run.snapshot.text,
                    &run.snapshot.thought,
                )
                .is_err()
            {
                inner.error = Some("回复保存失败，请保留当前窗口中的内容".into());
            } else {
                run.last_checkpoint = Instant::now();
            }
        }
        drop(inner);
        self.emit();
    }

    fn finish(&self, run_id: &str, status: &str, error: Option<String>) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(run) = inner
            .active
            .as_mut()
            .filter(|run| run.snapshot.record.id == run_id && busy(&run.snapshot.record.status))
        {
            run.snapshot.record.status = status.into();
            run.snapshot.record.error = error;
            if self
                .store
                .lock()
                .unwrap()
                .checkpoint(
                    &run.snapshot.record,
                    &run.snapshot.text,
                    &run.snapshot.thought,
                )
                .is_err()
            {
                inner.error = Some("运行结果保存失败".into());
            }
        }
        drop(inner);
        self.emit();
    }

    fn abort(&self, run_id: &str, error: String) {
        let client = {
            let mut inner = self.inner.lock().unwrap();
            let Some(run) = inner.active.as_mut().filter(|run| {
                run.snapshot.record.id == run_id && busy(&run.snapshot.record.status)
            }) else {
                return;
            };
            run.snapshot.record.status = "failed".into();
            run.snapshot.record.error = Some(error.clone());
            let _ = self.store.lock().unwrap().checkpoint(
                &run.snapshot.record,
                &run.snapshot.text,
                &run.snapshot.thought,
            );
            inner.connection_id.clear();
            inner.connection = "disconnected".into();
            inner.error = Some(error);
            inner.client.take()
        };
        if let Some(client) = client {
            client.stop();
        }
        self.emit();
    }

    fn connection_lost(&self, connection_id: &str) {
        let run_id = {
            let mut inner = self.inner.lock().unwrap();
            if inner.connection_id != connection_id {
                return;
            }
            inner.connection = "disconnected".into();
            inner.error = Some("Codex 原生进程已退出，可以重新连接".into());
            inner
                .active
                .as_ref()
                .map(|run| run.snapshot.record.id.clone())
        };
        if let Some(id) = run_id {
            self.finish(&id, "failed", Some("Codex 连接中断".into()));
        }
        self.emit();
    }

    pub fn stop(&self) {
        self.stop_expected(None);
    }
    pub(crate) fn stop_expected(&self, expected: Option<&str>) {
        let (client, run_id) = {
            let mut inner = self.inner.lock().unwrap();
            if expected.is_some_and(|id| {
                !inner.active.as_ref().is_some_and(|run| {
                    run.snapshot.record.id == id && busy(&run.snapshot.record.status)
                })
            }) {
                return;
            }
            inner.connection_id.clear();
            inner.connection = "disconnected".into();
            inner.error = None;
            (
                inner.client.take(),
                inner
                    .active
                    .as_ref()
                    .map(|run| run.snapshot.record.id.clone()),
            )
        };
        if let Some(client) = client {
            client.stop();
        }
        if let Some(id) = run_id {
            self.finish(&id, "interrupted", Some("连接已主动断开".into()));
        }
        self.emit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn active() -> ActiveRun {
        ActiveRun {
            snapshot: RunSnapshot {
                record: RunRecord {
                    agent_id: "codex-win".into(),
                    model: None,
                    reasoning_effort: None,
                    id: "run".into(),
                    conversation_id: "room".into(),
                    user_message_id: "user".into(),
                    assistant_message_id: "assistant".into(),
                    status: "running".into(),
                    native_thread_id: Some("thread".into()),
                    discussion_id: None,
                    round: None,
                    native_turn_id: Some("turn".into()),
                    error: None,
                },
                text: String::new(),
                thought: String::new(),
            },
            items: BTreeMap::new(),
            order: Vec::new(),
            last_checkpoint: Instant::now(),
        }
    }
    #[test]
    fn rejects_events_from_other_thread_turn_and_terminal_run() {
        let mut run = active();
        assert!(run.accepts(&json!({"threadId":"thread","turnId":"turn"})));
        assert!(!run.accepts(&json!({"threadId":"other","turnId":"turn"})));
        assert!(!run.accepts(&json!({"threadId":"thread","turnId":"other"})));
        assert!(!run.accepts(&json!({"threadId":"thread"})));
        run.snapshot.record.status = "completed".into();
        assert!(!run.accepts(&json!({"threadId":"thread","turnId":"turn"})));
    }
    #[test]
    fn completed_item_replaces_deltas_and_preserves_native_item_order() {
        let mut run = active();
        run.text("z", "早", false);
        run.text("z", "期", false);
        run.text("a", "第二段", false);
        run.text("z", "最终第一段", true);
        assert_eq!(run.snapshot.text, "最终第一段\n\n第二段");
        run.text("z", "最终第一段", true);
        assert_eq!(run.snapshot.text, "最终第一段\n\n第二段");
    }
}
