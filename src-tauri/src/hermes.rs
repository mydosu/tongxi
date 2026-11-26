//! 所有走 ACP 的成员共用这一个运行时：Hermes、DSH、Claude、OpenCode、Antigravity。
//! 连接、回合、超时、取消、用量、停止原因只在这里写一遍，修一处各家都生效。
//! 各家差别尽量从 agent 自己报的能力和配置项里读（同 Ekko Studio 的 NativeAcpTurn），
//! 读不出来的才按 `Target` 分（见 acp_transport.rs）。
use crate::acp_transport::Target;
use crate::codex::{
    busy_ids, busy_in, headline, insert_run, live_in, run_mut, run_ref, RunSnapshot,
    RuntimeSnapshot,
};
use crate::models::ModelOption;
use crate::rpc::Client;
use crate::store::{RunRecord, Store};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};
use tauri::Emitter;
use uuid::Uuid;

type Result<T> = std::result::Result<T, String>;
/// initialize 时桥可能现读模型目录（Claude 要跑 CLI），给足时间。
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(90);
struct Inner {
    connection: String,
    generation: String,
    client: Option<Arc<Client>>,
    /// 会话单独选了别的 profile 时另起的 Hermes 进程：profile → (generation, client)。一个进程只能服务一个 HERMES_HOME。
    extra: HashMap<String, (String, Arc<Client>)>,
    version: Option<String>,
    error: Option<String>,
    runs: Vec<RunSnapshot>,
    /// Albion 的原始正文（按运行 ID），可见性过滤从它算出 `run.text`。
    wire_text: HashMap<String, String>,
    models: Vec<ModelOption>,
    default_model: Option<String>,
    default_effort: Option<String>,
    /// initialize 报的能力：续聊用 session/load 还是 session/resume 看它。
    capabilities: Value,
    /// 读目录时那个空会话的回包：有哪些配置项（模型、思考强度、模式）看它。
    catalog: Value,
    /// 这条连接里已经载入的会话（进程 generation, 会话 ID），续聊不再重复载入。
    loaded: HashSet<(String, String)>,
    last_checkpoint: Instant,
}
pub struct Runtime {
    app: tauri::AppHandle,
    store: Arc<Mutex<Store>>,
    directory: PathBuf,
    target: Target,
    inner: Mutex<Inner>,
    revision: AtomicU64,
    /// 同一 profile 只起一个进程。
    spawning: Mutex<()>,
}
fn busy(run: &RunSnapshot) -> bool {
    matches!(
        run.record.status.as_str(),
        "starting" | "running" | "cancelling"
    )
}
pub(crate) fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}
fn initialize_params() -> Value {
    json!({"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false},"clientInfo":{"name":"personal-agent-hub","version":env!("CARGO_PKG_VERSION")}})
}
/// 续聊用哪个方法看 agent 自己报的能力：报了 loadSession 用 session/load（Hermes、OpenCode、
/// Claude 和 Antigravity 的桥），只报 sessionCapabilities.resume 的用 session/resume（DSH）。
fn restore_method(capabilities: &Value) -> &'static str {
    if capabilities["loadSession"] != true && capabilities["sessionCapabilities"]["resume"].is_object() {
        "session/resume"
    } else {
        "session/load"
    }
}
fn config_option<'a>(response: &'a Value, id: &str) -> Option<&'a Value> {
    response["configOptions"].as_array()?.iter().find(|option| option["id"] == id)
}
/// 要发的思考强度：会话选的 > 模型默认；都没有时，配置项里有空值（Claude 的「默认」）就发空值
/// 把上次的设置清掉，没有就不发。没有这个配置项的（OpenCode、Antigravity）不发。
fn effort_value(catalog: &Value, chosen: Option<String>) -> Option<String> {
    let option = config_option(catalog, "reasoning_effort")?;
    chosen.or_else(|| {
        option["options"]
            .as_array()?
            .iter()
            .any(|entry| entry["value"] == "")
            .then(String::new)
    })
}
/// 各家同一套停止原因：只有正常收尾算完成；撞输出上限算失败（内容可能不完整）。
fn completion(name: &str, reason: Option<&str>) -> (&'static str, Option<String>) {
    match reason {
        Some("cancelled") => ("interrupted", None),
        Some("end_turn" | "max_turn_requests" | "refusal") => ("completed", None),
        Some("max_tokens") => (
            "failed",
            Some(format!("{name} 输出达到 token 上限，返回内容可能不完整")),
        ),
        _ => ("failed", Some(format!("{name} 未返回正常完成状态"))),
    }
}
/// Hermes 调模型失败时不报错，而是把失败说明当回复、照常 end_turn 结束；认出它自己的几种失败开头，
/// 记成失败（额度类带「额度」二字，主脑的模型战绩据此算撞额度）。
fn failure_reply(text: &str) -> Option<String> {
    let text = text.trim_start();
    let line: String = text.lines().next().unwrap_or("").chars().take(200).collect();
    if text.starts_with("Billing or credits exhausted") || text.starts_with("Provider reported usage/credit exhaustion") {
        Some(format!("Hermes 线路额度不足（额度限制）：{line}"))
    } else if text.starts_with("API call failed after") {
        Some(format!("Hermes 调用模型失败：{line}"))
    } else {
        None
    }
}
/// Hermes 自己会话库里这个会话的累计用量（输入不含缓存、缓存读、缓存写、输出）。
/// 有的线路不在 ACP 回包里报用量，回合前后各读一次相减补上。
fn session_tokens(profile: Option<&str>, session_id: &str) -> Option<[i64; 4]> {
    let home = crate::service_updates::hermes_home_for(profile)?;
    let db = rusqlite::Connection::open_with_flags(home.join("state.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    db.busy_timeout(Duration::from_secs(2)).ok()?;
    db.query_row(
        "SELECT COALESCE(input_tokens,0),COALESCE(cache_read_tokens,0),COALESCE(cache_write_tokens,0),COALESCE(output_tokens,0) FROM sessions WHERE id=?1",
        [session_id],
        |row| Ok([row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?]),
    )
    .ok()
}
/// 换成和 ACP 回包一样的口径：inputTokens 含缓存读写。
fn turn_usage(before: [i64; 4], after: [i64; 4]) -> Option<Value> {
    let [input, read, write, output]: [i64; 4] = std::array::from_fn(|index| (after[index] - before[index]).max(0));
    (input + read + write + output > 0).then(|| {
        json!({"inputTokens": input + read + write, "cachedReadTokens": read, "cachedWriteTokens": write, "outputTokens": output})
    })
}
fn provider_label(model: &Value) -> Option<String> {
    let description = model["description"].as_str()?;
    let label = description
        .split("Provider:")
        .nth(1)?
        .split('•')
        .next()?
        .trim();
    (!label.is_empty()).then(|| label.to_owned())
}
/// 标准 ACP 的模型选项（configOptions 里 id=model：DSH、Claude、OpenCode、Antigravity）；Hermes 用的是 `models`。
fn standard_models(response: &Value) -> bool {
    response["models"].is_null() && crate::dsh::current_option(response, "model").is_some()
}
fn models_from_response(response: &Value) -> Vec<ModelOption> {
    if standard_models(response) {
        return crate::dsh::models_from_response(response);
    }
    response["models"]["availableModels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|model| {
            Some(ModelOption {
                id: model["modelId"].as_str()?.into(),
                name: model["name"].as_str().unwrap_or("模型").into(),
                provider_id: model["providerId"]
                    .as_str()
                    .or_else(|| model["provider"].as_str())
                    .map(String::from)
                    .or_else(|| {
                        let id = model["modelId"].as_str()?;
                        if id.starts_with("custom:") {
                            provider_label(model)
                        } else {
                            id.split_once(':').map(|(provider, _)| provider.to_owned())
                        }
                    })
                    .or_else(|| {
                        model["modelId"]
                            .as_str()?
                            .split_once('/')
                            .map(|(provider, _)| provider.to_owned())
                    }),
                provider_name: model["providerName"]
                    .as_str()
                    .or_else(|| model["provider"].as_str())
                    .map(String::from)
                    .or_else(|| provider_label(model)),
                efforts: [
                    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
                ]
                .into_iter()
                .map(String::from)
                .collect(),
                default_effort: None,
            })
        })
        .collect()
}

impl Runtime {
    /// 只读列出该 agent 自己的原生会话（ACP session/list，ACP 标准方法）。
    pub fn list_sessions(&self) -> Result<Vec<crate::codex::NativeSession>> {
        let client = {
            let inner = self.inner.lock().unwrap();
            inner
                .client
                .clone()
                .ok_or_else(|| format!("{} 未连接", self.target.name()))?
        };
        let response = client.rpc_timeout("session/list", json!({}), Duration::from_secs(20))?;
        // 每次连接都会在 <key>-workspaces/catalog 开一个空会话读模型目录，那些不是用户会话，不列出；
        // 按结尾认，测试版、别的数据目录留下的也一并滤掉。
        let catalog = format!("{}-workspaces\\catalog", self.target.key());
        let mut sessions: Vec<_> = crate::codex::native_sessions_from_acp(&response)
            .into_iter()
            .filter(|session| {
                session.cwd.as_deref().is_none_or(|cwd| {
                    !cwd.replace('/', "\\").trim_end_matches('\\').to_lowercase().ends_with(&catalog)
                })
            })
            .collect();
        if matches!(self.target, Target::Dsh) {
            crate::dsh::fill_titles(&self.directory, &mut sessions);
        }
        Ok(sessions)
    }

    pub fn new(app: tauri::AppHandle, store: Arc<Mutex<Store>>, directory: PathBuf) -> Arc<Self> {
        Self::with_target(app, store, directory, Target::windows())
    }
    pub fn with_target(
        app: tauri::AppHandle,
        store: Arc<Mutex<Store>>,
        directory: PathBuf,
        target: Target,
    ) -> Arc<Self> {
        Arc::new(Self {
            app,
            store,
            directory,
            target,
            revision: AtomicU64::new(1),
            spawning: Mutex::new(()),
            inner: Mutex::new(Inner {
                connection: "disconnected".into(),
                generation: String::new(),
                client: None,
                extra: HashMap::new(),
                version: None,
                error: None,
                runs: Vec::new(),
                wire_text: HashMap::new(),
                models: vec![],
                default_model: None,
                default_effort: None,
                capabilities: Value::Null,
                catalog: Value::Null,
                loaded: HashSet::new(),
                last_checkpoint: Instant::now(),
            }),
        })
    }
    pub fn snapshot(&self) -> RuntimeSnapshot {
        let inner = self.inner.lock().unwrap();
        RuntimeSnapshot {
            revision: self.revision.fetch_add(1, Ordering::Relaxed),
            connection: inner.connection.clone(),
            executable: self.target.executable(),
            version: inner.version.clone(),
            error: inner.error.clone(),
            active: headline(&inner.runs),
            runs: inner.runs.clone(),
            models: inner.models.clone(),
            default_model: inner.default_model.clone(),
            default_effort: inner.default_effort.clone(),
        }
    }
    fn emit(&self) {
        let event = format!("{}-state", self.target.key());
        let _ = self.app.emit(&event, crate::state_events::slim(&event, &self.snapshot()));
    }
    /// 版本：DSH 以安装目录的 package.json 为准，其余用 initialize 报的。
    fn version(&self, initialized: &Value) -> Option<String> {
        self.target
            .version(&self.directory)
            .or_else(|| initialized["agentInfo"]["version"].as_str().map(String::from))
    }
    /// 开个空会话带回模型目录、默认模型和默认思考强度，读完关掉；连接和「刷新模型」共用。
    fn read_catalog(&self, client: &Client) -> Result<(Vec<ModelOption>, Option<String>, Option<String>, Value)> {
        let cwd = self
            .directory
            .join(format!("{}-workspaces", self.target.key()))
            .join("catalog");
        std::fs::create_dir_all(&cwd).map_err(|_| format!("{} 目录创建失败", self.target.name()))?;
        let cwd = self.target.cwd(&cwd)?;
        let catalog = client.rpc("session/new", json!({"cwd":cwd,"mcpServers":[]}))?;
        let mut models = models_from_response(&catalog);
        if let Some(session_id) = catalog["sessionId"].as_str() {
            // 思考强度跟着模型变（DSH、Claude）：回包只带当前模型的，逐个切过去问一遍。
            if standard_models(&catalog) && config_option(&catalog, "reasoning_effort").is_some() {
                for model in &mut models {
                    let response = client.rpc(
                        "session/set_config_option",
                        json!({"sessionId":session_id,"configId":"model","value":model.id}),
                    )?;
                    if let Some(capability) = models_from_response(&response)
                        .into_iter()
                        .find(|entry| entry.id == model.id)
                    {
                        model.efforts = capability.efforts;
                        model.default_effort = capability.default_effort;
                    }
                }
            }
            let _ = client.rpc("session/close", json!({"sessionId":session_id}));
        }
        // 同席自己定的默认模型优先（比如固定走某个提供商），目录里没有它才退回 harness 的默认。
        let default_model = self
            .store
            .lock()
            .unwrap()
            .agent_default_model(self.target.id())
            .filter(|model| models.iter().any(|entry| &entry.id == model))
            .or_else(|| catalog["models"]["currentModelId"].as_str().map(String::from))
            .or_else(|| crate::dsh::current_option(&catalog, "model"));
        let default_effort = non_empty(crate::dsh::current_option(&catalog, "reasoning_effort"));
        Ok((models, default_model, default_effort, catalog))
    }
    /// 不断开、不动正在跑的会话，只把模型目录重读一遍。
    pub fn refresh_models(&self) -> Result<RuntimeSnapshot> {
        let client = {
            let inner = self.inner.lock().unwrap();
            inner
                .client
                .clone()
                .filter(|_| inner.connection == "connected")
                .ok_or_else(|| format!("{} 未连接", self.target.name()))?
        };
        // 桥只在 initialize 时读模型目录的（Claude、Antigravity），再发一次才会重读；真 ACP agent 不重复握手。
        let initialized = if self.target.catalog_on_initialize() {
            Some(client.rpc_timeout("initialize", initialize_params(), INITIALIZE_TIMEOUT)?)
        } else {
            None
        };
        let (models, default_model, default_effort, catalog) = self.read_catalog(&client)?;
        {
            let mut inner = self.inner.lock().unwrap();
            if let Some(initialized) = initialized {
                inner.version = self.version(&initialized).or(inner.version.take());
                inner.capabilities = initialized["agentCapabilities"].clone();
            }
            crate::pricing::remember(self.target.id(), &models);
            inner.models = models;
            inner.default_model = default_model;
            inner.default_effort = default_effort;
            inner.catalog = catalog;
        }
        self.emit();
        Ok(self.snapshot())
    }
    fn spawn(self: &Arc<Self>, command: std::process::Command, generation: &str) -> Result<Arc<Client>> {
        let (weak, exit) = (Arc::downgrade(self), Arc::downgrade(self));
        let (message_generation, exit_generation) = (generation.to_owned(), generation.to_owned());
        Client::spawn_command(
            command,
            move |value| {
                if let Some(runtime) = weak.upgrade() {
                    runtime.notification(&message_generation, value);
                }
            },
            move || {
                if let Some(runtime) = exit.upgrade() {
                    runtime.lost(&exit_generation);
                }
            },
        )
    }
    pub fn connect(self: &Arc<Self>) -> Result<RuntimeSnapshot> {
        let generation = Uuid::new_v4().to_string();
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.connection == "connected" {
                drop(inner);
                return Ok(self.snapshot());
            }
            if inner.connection == "connecting" {
                return Err(format!("{} 正在连接", self.target.name()));
            }
            inner.connection = "connecting".into();
            inner.error = None;
            inner.generation = generation.clone();
        }
        self.emit();
        let result: Result<()> = (|| {
            let command = self.target.command(&self.directory)?;
            let client = self.spawn(command, &generation)?;
            self.inner.lock().unwrap().client = Some(client.clone());
            let initialized = client.rpc_timeout("initialize", initialize_params(), INITIALIZE_TIMEOUT)?;
            let (models, default_model, default_effort, catalog) = self.read_catalog(&client)?;
            let mut inner = self.inner.lock().unwrap();
            if inner.generation != generation || inner.connection != "connecting" {
                return Err(format!("{} 连接已取消", self.target.name()));
            }
            inner.version = self.version(&initialized);
            inner.capabilities = initialized["agentCapabilities"].clone();
            crate::pricing::remember(self.target.id(), &models);
            inner.models = models;
            inner.default_model = default_model;
            inner.default_effort = default_effort;
            inner.catalog = catalog;
            inner.loaded.clear();
            inner.connection = "connected".into();
            Ok(())
        })();
        if let Err(error) = result {
            self.stop();
            self.inner.lock().unwrap().error = Some(error.clone());
            self.emit();
            return Err(error);
        }
        self.emit();
        Ok(self.snapshot())
    }
    pub fn guard<T>(&self, id: &str, action: impl FnOnce() -> Result<T>) -> Result<T> {
        let inner = self.inner.lock().map_err(|_| format!("{} 状态不可用", self.target.name()))?;
        if busy_in(&inner.runs, id) {
            return Err(format!(
                "{} 正在回复，请完成或停止后再修改会话",
                self.target.name()
            ));
        }
        action()
    }
    pub fn send(
        self: &Arc<Self>,
        conversation_id: &str,
        message_id: &str,
        content: &str,
    ) -> Result<RunRecord> {
        let inner = self.inner.lock().unwrap();
        let existing = self.store.lock().unwrap().run_for_message(message_id)?;
        self.store
            .lock()
            .unwrap()
            .validate_private_target(conversation_id, self.target.id())?;
        if let Some(record) = existing {
            self.store
                .lock()
                .unwrap()
                .save_message(conversation_id, message_id, content)?;
            return Ok(record);
        }
        if inner.connection != "connected" {
            return Err(format!("请先连接 {}", self.target.name()));
        }
        if busy_in(&inner.runs, conversation_id) {
            return Err(format!(
                "{} 正在处理这个会话的上一条消息，请等待或停止",
                self.target.name()
            ));
        }
        let client = inner.client.clone().ok_or_else(|| format!("{} 接口不可用", self.target.name()))?;
        let record = self.store.lock().unwrap().begin_agent_run(
            conversation_id,
            message_id,
            content,
            self.target.id(),
        )?;
        drop(inner);
        self.start(record, client, content.trim().to_owned())
    }
    pub(crate) fn send_discussion(
        self: &Arc<Self>,
        discussion: &str,
        round: u32,
    ) -> Result<RunRecord> {
        let inner = self.inner.lock().unwrap();
        if inner.connection != "connected" {
            return Err(format!("{} 尚未连接", self.target.name()));
        }
        let client = inner.client.clone().ok_or("成员接口不可用")?;
        let (record, text) = self.store.lock().unwrap().begin_discussion_turn(
            discussion,
            self.target.id(),
            round,
        )?;
        drop(inner);
        self.start(record, client, text)
    }
    /// 登记这一轮并在后台跑；出错收尾成失败。
    fn start(self: &Arc<Self>, record: RunRecord, client: Arc<Client>, text: String) -> Result<RunRecord> {
        {
            let mut inner = self.inner.lock().unwrap();
            insert_run(
                &mut inner.runs,
                RunSnapshot {
                    record: record.clone(),
                    text: String::new(),
                    thought: String::new(),
                    tool_events: Vec::new(),
                },
            );
            let live: Vec<String> = inner.runs.iter().map(|run| run.record.id.clone()).collect();
            inner.wire_text.retain(|id, _| live.contains(id));
        }
        self.emit();
        let runtime = self.clone();
        let run_id = record.id.clone();
        std::thread::spawn(move || {
            if let Err(error) = runtime.prompt(&client, &run_id, &text) {
                runtime.finish(&run_id, "failed", Some(error));
            }
        });
        Ok(record)
    }

    /// 会话单独选的 profile 和同席默认不同时，换用那个 profile 的进程，没有就起一个。返回 (generation, client)。
    fn client_for(self: &Arc<Self>, primary: &Arc<Client>, profile: Option<&str>) -> Result<(String, Arc<Client>)> {
        let profile = profile.filter(|_| self.target.id() == "hermes-win");
        let Some(profile) = profile.filter(|name| Some(*name) != crate::service_updates::hermes_profile().as_deref()) else {
            return Ok((self.inner.lock().unwrap().generation.clone(), primary.clone()));
        };
        let _spawning = self.spawning.lock().unwrap();
        if let Some((generation, client)) = self.inner.lock().unwrap().extra.get(profile) {
            return Ok((generation.clone(), client.clone()));
        }
        let command = self.target.command_profile(&self.directory, Some(profile))?;
        let generation = Uuid::new_v4().to_string();
        let client = self.spawn(command, &generation)?;
        if let Err(error) = client.rpc_timeout("initialize", initialize_params(), INITIALIZE_TIMEOUT) {
            client.stop();
            return Err(format!("Hermes profile「{profile}」启动失败：{error}"));
        }
        self.inner.lock().unwrap().extra.insert(profile.to_owned(), (generation.clone(), client.clone()));
        Ok((generation, client))
    }

    /// 主进程加各 profile 的额外进程，取消、卸会话时都要通知到。
    fn all_clients(&self) -> Vec<Arc<Client>> {
        let inner = self.inner.lock().unwrap();
        inner.client.iter().cloned().chain(inner.extra.values().map(|(_, client)| client.clone())).collect()
    }

    fn prompt(self: &Arc<Self>, primary: &Arc<Client>, run_id: &str, content: &str) -> Result<()> {
        let conversation_id = run_ref(&self.inner.lock().unwrap().runs, run_id)
            .ok_or("运行已结束")?
            .record
            .conversation_id
            .clone();
        let mut settings = self
            .store
            .lock()
            .unwrap()
            .detail(&conversation_id)?
            .sessions
            .into_iter()
            .find(|session| session.agent_id == self.target.id())
            .ok_or_else(|| format!("{} 会话不存在", self.target.name()))?;
        let profile = settings.profile.clone().or_else(crate::service_updates::hermes_profile);
        let (generation, client) = self.client_for(primary, settings.profile.as_deref())?;
        let client = &client;
        {
            let inner = self.inner.lock().unwrap();
            let record = &run_ref(&inner.runs, run_id).ok_or("运行已结束")?.record;
            settings.model = record.model.clone();
            settings.reasoning_effort = record.reasoning_effort.clone();
            crate::models::validate_selection(
                &inner.models,
                settings.model.as_deref(),
                settings.reasoning_effort.as_deref(),
                inner.default_model.as_deref(),
            )?;
        }
        // 接入的原生会话回到它自己的目录续聊（Claude、DSH 按 cwd 归档会话，换目录续不上）；
        // 原生会话的目录本来就是 agent 侧路径，原样传回。
        let cwd = match settings.native_cwd.as_deref() {
            Some(native) => native.to_owned(),
            None => {
                // 陪伴的目录映射到 WSL 数据目录，不跟默认工作文件夹走。
                let resuming = settings.native_session_id.is_some() || self.target.key() == "albion";
                let local = self.store.lock().unwrap().fallback_cwd(&self.directory, &conversation_id, self.target.id(), self.target.key(), resuming);
                std::fs::create_dir_all(&local).map_err(|_| format!("{} 会话目录创建失败", self.target.name()))?;
                self.target.cwd(&local)?
            }
        };
        let project_db = self.store.lock().unwrap().path.clone();
        let mcp_servers =
            crate::project_tools::project_chat_mcp_server(&project_db, &conversation_id)?
                .map_or_else(|| json!([]), |server| json!([server]));
        let (method, catalog) = {
            let inner = self.inner.lock().unwrap();
            (restore_method(&inner.capabilities), inner.catalog.clone())
        };
        let new_session = || client.rpc("session/new", json!({"cwd":cwd,"mcpServers":mcp_servers.clone()}));
        // 这条连接里载入过的会话不再重复载入。绑着的会话找不到时（换了 profile、agent 那边删了），
        // 直接开一条新的，别让整个会话发不出消息；bind_thread 会把新 id 写回。
        // Hermes 找不到会话时不报错、回空结果，所以空结果也算没载入。
        let (response, restored) = match settings.native_session_id.as_ref() {
            Some(session_id) if self.inner.lock().unwrap().loaded.contains(&(generation.clone(), session_id.clone())) => {
                (Value::Null, true)
            }
            Some(session_id) => match client
                .rpc(method, json!({"sessionId":session_id,"cwd":cwd,"mcpServers":mcp_servers.clone()}))
                .ok()
                .filter(|loaded| !loaded.is_null())
            {
                Some(loaded) => (loaded, true),
                None => (new_session()?, false),
            },
            None => (new_session()?, false),
        };
        let session_id = if restored { settings.native_session_id.clone() } else { None }
            .or_else(|| response["sessionId"].as_str().map(String::from))
            .ok_or_else(|| format!("{} 未返回原生会话 ID", self.target.name()))?;
        let key = (generation, session_id.clone());
        self.inner.lock().unwrap().loaded.insert(key.clone());
        let result = self.turn(client, run_id, &conversation_id, &settings, &cwd, &session_id, &response, &catalog, profile.as_deref(), &project_db, content);
        if let Err(error) = result {
            // 只这一轮失败：同一个进程上别的会话还在跑，不因为一轮出错就断开整个成员。
            // 让 agent 停掉这一轮（判定卡住时它可能还在跑），下一轮重新载入会话。
            self.inner.lock().unwrap().loaded.remove(&key);
            let _ = client.write(&json!({"method":"session/cancel","params":{"sessionId":session_id}}));
            let error = match self.target.error_hint() {
                Some(hint) => format!("{error}。{hint}"),
                None => error,
            };
            self.finish(run_id, "failed", Some(error.clone()));
            return Err(error);
        }
        Ok(())
    }

    /// 会话已打开之后的一轮：绑定会话、选模型和思考强度、按权限档收紧、发 prompt、记用量和结果。
    #[allow(clippy::too_many_arguments)]
    fn turn(
        &self,
        client: &Arc<Client>,
        run_id: &str,
        conversation_id: &str,
        settings: &crate::store::Session,
        cwd: &str,
        session_id: &str,
        response: &Value,
        catalog: &Value,
        profile: Option<&str>,
        project_db: &std::path::Path,
        content: &str,
    ) -> Result<()> {
        // 思考强度是逐个模型问出来的（DSH、Claude）时，回包只带当前模型的，不拿它覆盖目录。
        let models = models_from_response(response);
        let probed = config_option(catalog, "reasoning_effort").is_some();
        {
            let mut inner = self.inner.lock().unwrap();
            if !models.is_empty() && !probed {
                crate::pricing::remember(self.target.id(), &models);
                inner.models = models;
            }
            let run = run_mut(&mut inner.runs, run_id)
                .filter(|run| busy(run))
                .ok_or("运行已结束")?;
            self.store
                .lock()
                .unwrap()
                .bind_thread(&mut run.record, session_id)?;
            if settings.native_cwd.is_none() {
                // 早先续上的会话没记目录：记下这次用的，以后都回这里续。
                self.store.lock().unwrap().connection.execute(
                    "UPDATE sessions SET native_cwd=?1 WHERE conversation_id=?2 AND agent_id=?3 AND native_cwd IS NULL",
                    rusqlite::params![cwd, conversation_id, self.target.id()],
                ).map_err(|_| format!("{} 会话目录保存失败", self.target.name()))?;
            }
            if run.record.status == "cancelling" {
                drop(inner);
                self.finish(run_id, "interrupted", None);
                return Ok(());
            }
        }
        let access = crate::project_brain::access(&self.store.lock().unwrap().connection, conversation_id);
        let (selected_model, model_effort) = {
            let inner = self.inner.lock().unwrap();
            let model = settings.model.clone().or_else(|| inner.default_model.clone());
            let effort = model.as_ref().and_then(|model| {
                inner.models.iter().find(|entry| &entry.id == model).and_then(|entry| entry.default_effort.clone())
            });
            (model, effort)
        };
        let (applied_model, applied_effort) = if standard_models(catalog) {
            // 标准 ACP：模型、思考强度都走 configOptions。
            let mut applied = Value::Null;
            if let Some(model_id) = selected_model.as_ref() {
                applied = client.rpc("session/set_config_option", json!({"sessionId":session_id,"configId":"model","value":model_id}))?;
            }
            if let Some(effort) = effort_value(catalog, settings.reasoning_effort.clone().or(model_effort)) {
                applied = client.rpc("session/set_config_option", json!({"sessionId":session_id,"configId":"reasoning_effort","value":effort}))?;
            }
            // 带 plan 模式的（OpenCode、Antigravity）：只读档切到 plan（不许改文件），别的档回到默认 build。
            if config_option(catalog, "mode").is_some() {
                let read = access.as_ref().is_some_and(|(tier, _)| *tier == "read");
                client.rpc("session/set_config_option", json!({"sessionId":session_id,"configId":"mode","value":if read {"plan"} else {"build"}}))?;
            }
            (selected_model.clone(), non_empty(crate::dsh::current_option(&applied, "reasoning_effort")))
        } else {
            if let Some(model_id) = selected_model.as_ref() {
                client.rpc("session/set_model", json!({"sessionId":session_id,"modelId":model_id}))?;
            }
            let applied = client.rpc("session/set_config_option", json!({"sessionId":session_id,"configId":"reasoning_effort","value":settings.reasoning_effort.as_deref().unwrap_or("default")}))?;
            let applied = &applied["_meta"]["agentHub"];
            let effort = if applied["reasoningConfig"]["enabled"] == false {
                Some("none".into())
            } else {
                applied["reasoningConfig"]["effort"].as_str().map(String::from)
            };
            let model = applied["modelId"]
                .as_str()
                .map(String::from)
                .or_else(|| selected_model.clone())
                .or_else(|| applied["model"].as_str().map(String::from));
            (model, effort)
        };
        {
            let mut inner = self.inner.lock().unwrap();
            let run = run_mut(&mut inner.runs, run_id)
                .filter(|run| busy(run))
                .ok_or("运行已结束")?;
            if run.record.status == "cancelling" {
                drop(inner);
                self.finish(run_id, "interrupted", None);
                return Ok(());
            }
            self.store
                .lock()
                .unwrap()
                .mark_turn(&mut run.record, run_id)?;
            self.store.lock().unwrap().record_model(&mut run.record, applied_model, applied_effort)?;
        }
        if self.target.id() == "albion-wsl" {
            let digest = {
                let store = self.store.lock().unwrap();
                if store.conversation(conversation_id)?.kind == "direct" {
                    crate::services::dev_digest(&store)
                } else {
                    String::new()
                }
            };
            let applied = client.rpc(
                "session/set_config_option",
                json!({"sessionId":session_id,"configId":"hub_development_digest","value":digest}),
            )?;
            let applied = &applied["_meta"]["agentHub"];
            if applied["digest_injected"].as_bool() != Some(!digest.trim().is_empty()) {
                return Err("Hermes 摘要注入检查失败".into());
            }
        }
        // 项目里的权限档：DSH 的桥读权限表，Claude 的桥读 prompt 里的 tongxiTier。
        if matches!(self.target, Target::Dsh) {
            crate::dsh::permit(&self.directory, session_id, access.clone())?;
        }
        self.emit();
        let input = crate::attachments::prompt(project_db, conversation_id, content, false)?;
        let mut params = json!({"sessionId":session_id,"prompt":input});
        if matches!(self.target, Target::Claude) {
            params["tongxiTier"] = json!(access.as_ref().map_or("full", |(tier, _)| *tier));
        }
        let before = (self.target.id() == "hermes-win").then(|| session_tokens(profile, session_id)).flatten();
        let started = crate::store::now();
        let mut value = client.rpc_while_alive("session/prompt", params, crate::rpc::QUIET_LIMIT)?;
        // 回包不带用量的从 agent 自己的会话记录补：Hermes 读会话库前后相减；DSH 读会话日志，它落盘晚，收尾后在后台补。
        if !value["usage"].is_object() {
            if let Some(usage) = before.zip(session_tokens(profile, session_id)).and_then(|(before, after)| turn_usage(before, after)) {
                value["usage"] = usage;
            }
        }
        self.store.lock().unwrap().record_usage(run_id, &value);
        if matches!(self.target, Target::Dsh) && !value["usage"].is_object() {
            let (store, run_id, session_id) = (self.store.clone(), run_id.to_owned(), session_id.to_owned());
            std::thread::spawn(move || {
                if let Some(usage) = crate::dsh::session_usage(&session_id, started) {
                    store.lock().unwrap().record_usage(&run_id, &usage);
                }
            });
        }
        let (mut status, mut error) = completion(self.target.name(), value["stopReason"].as_str());
        if status == "completed" {
            let text = run_ref(&self.inner.lock().unwrap().runs, run_id)
                .map(|run| run.text.clone())
                .unwrap_or_default();
            if let Some(reason) = failure_reply(&text) {
                status = "failed";
                error = Some(reason);
            }
        }
        self.finish(run_id, status, error);
        Ok(())
    }
    fn notification(&self, generation: &str, value: Value) {
        if crate::project_brain::background_notice(&self.store, self.target.id(), &value) {
            return;
        }
        if value["method"] != "session/update" {
            return;
        }
        let params = &value["params"];
        let mut inner = self.inner.lock().unwrap();
        if inner.generation != generation && !inner.extra.values().any(|(extra, _)| extra == generation) {
            return;
        }
        let checkpoint = inner.last_checkpoint.elapsed() >= Duration::from_millis(350);
        let Inner {
            runs, wire_text, ..
        } = &mut *inner;
        let Some(run) = runs.iter_mut().find(|run| {
            busy(run) && run.record.native_thread_id.as_deref() == params["sessionId"].as_str()
        }) else {
            return;
        };
        let update = &params["update"];
        if let Some(event) = crate::codex::acp_tool_event(update) {
            let event = crate::codex::upsert_tool_event(&mut run.tool_events, &event);
            crate::store::push_tool(&mut run.thought, &run.text, &event.event_id);
            let _ = self
                .store
                .lock()
                .unwrap()
                .append_tool_event(&run.record, &event);
            drop(inner);
            self.emit();
            return;
        }
        if update["content"]["type"] != "text" {
            return;
        }
        let text = update["content"]["text"].as_str().unwrap_or("");
        // ACP 把「思考」和「正文」分成两种 chunk：思考进可折叠的思考块，正文进气泡；Albion 的可见性过滤只作用于正文。
        match update["sessionUpdate"].as_str().unwrap_or("") {
            "agent_thought_chunk" => crate::store::push_thought(&mut run.thought, &run.text, text),
            "agent_message_chunk" => {
                if self.target.id() == "albion-wsl" {
                    let wire = wire_text.entry(run.record.id.clone()).or_default();
                    wire.push_str(text);
                    run.text = crate::chat_text::albion_visible(wire);
                } else {
                    run.text.push_str(text);
                }
            }
            _ => return,
        }
        if checkpoint {
            let _ = self
                .store
                .lock()
                .unwrap()
                .checkpoint(&run.record, &run.text, &run.thought);
            inner.last_checkpoint = Instant::now();
        }
        drop(inner);
        self.emit();
    }
    fn finish(&self, id: &str, status: &str, error: Option<String>) {
        let mut inner = self.inner.lock().unwrap();
        let visible = (self.target.id() == "albion-wsl").then(|| {
            crate::chat_text::albion_complete(inner.wire_text.get(id).map_or("", String::as_str))
        });
        if let Some(run) = run_mut(&mut inner.runs, id).filter(|run| busy(run)) {
            run.record.status = status.into();
            run.record.error = error;
            if let Some(text) = visible {
                run.text = text;
            }
            let _ = self
                .store
                .lock()
                .unwrap()
                .checkpoint(&run.record, &run.text, &run.thought);
        }
        drop(inner);
        self.emit();
    }
    /// 把会话交给 CLI 前从桥里卸下这条会话；下次续聊重新载入，接上 CLI 里的新对话。
    pub(crate) fn release_native(&self, session_id: &str) {
        self.inner.lock().unwrap().loaded.retain(|(_, loaded)| loaded != session_id);
        for client in self.all_clients() {
            let _ = client.rpc("session/close", json!({"sessionId":session_id}));
        }
    }
    /// 换同席给这个成员定的默认模型，已连接时立即生效（下一条消息起）。
    pub(crate) fn set_default_model(&self, model: Option<&str>) -> Result<()> {
        {
            let inner = self.inner.lock().unwrap();
            if let Some(model) = model {
                if inner.connection == "connected" && !inner.models.iter().any(|entry| entry.id == model) {
                    return Err("当前模型目录里没有这个模型".into());
                }
            }
        }
        self.store
            .lock()
            .unwrap()
            .set_agent_default_model(self.target.id(), model)?;
        if let Some(model) = model {
            self.inner.lock().unwrap().default_model = Some(model.to_owned());
        }
        self.emit();
        Ok(())
    }
    pub fn cancel(&self, conversation_id: &str) -> Result<RuntimeSnapshot> {
        self.cancel_expected(conversation_id, None)
    }
    /// 回复进行中插话（目前只有 Claude 的桥支持 session/steer）：引导写进正在跑的进程，下一步原生采纳，不打断。
    pub fn steer(&self, conversation_id: &str, content: &str, message_id: &str) -> Result<()> {
        let (client, session) = {
            let inner = self.inner.lock().unwrap();
            let client = inner.client.clone().ok_or_else(|| format!("{} 已断开", self.target.name()))?;
            let run = inner
                .runs
                .iter()
                .find(|run| {
                    run.record.conversation_id == conversation_id
                        && busy(run)
                        && run.record.status != "cancelling"
                })
                .ok_or("这个会话没有正在进行的回复")?;
            (client, run.record.native_thread_id.clone())
        };
        let session = session.ok_or("回复还没真正开始，请稍后再插话")?;
        let db = self.store.lock().unwrap().path.clone();
        let input = crate::attachments::prompt(&db, conversation_id, content, false)?;
        client.rpc("session/steer", json!({"sessionId":session,"prompt":input}))?;
        let mut inner = self.inner.lock().unwrap();
        if let Some(run) = inner
            .runs
            .iter_mut()
            .find(|run| run.record.conversation_id == conversation_id && busy(run))
        {
            crate::store::push_steer(&mut run.thought, &run.text, message_id);
        }
        drop(inner);
        self.emit();
        Ok(())
    }
    pub(crate) fn cancel_expected(
        &self,
        conversation_id: &str,
        expected: Option<&str>,
    ) -> Result<RuntimeSnapshot> {
        let session = {
            let mut inner = self.inner.lock().unwrap();
            inner.client.as_ref().ok_or_else(|| format!("{} 已断开", self.target.name()))?;
            let run = live_in(&mut inner.runs, conversation_id)
                .filter(|run| expected.is_none_or(|id| run.record.id == id))
                .ok_or("这个会话没有正在进行的回复")?;
            run.record.status = "cancelling".into();
            self.store
                .lock()
                .unwrap()
                .checkpoint(&run.record, &run.text, &run.thought)?;
            run.record.native_thread_id.clone()
        };
        self.emit();
        if let Some(session_id) = session {
            // 会话在哪个 profile 的进程上不单独记，各进程都发一遍；不认识的会话 agent 直接忽略。
            for client in self.all_clients() {
                client.write(&json!({"method":"session/cancel","params":{"sessionId":session_id}}))?;
            }
        }
        Ok(self.snapshot())
    }
    fn lost(&self, generation: &str) {
        let id = {
            let mut inner = self.inner.lock().unwrap();
            if inner.generation != generation {
                // 某个 profile 的额外进程退出：从表里拿掉，下次用到再起；它上面的回合由 prompt 的出错分支收尾。
                inner.extra.retain(|_, (extra, _)| extra != generation);
                return;
            }
            inner.connection = "disconnected".into();
            inner.client = None;
            busy_ids(&inner.runs)
        };
        for id in id {
            self.finish(&id, "interrupted", Some(format!("{} 原生连接已断开", self.target.name())));
        }
        self.emit();
    }
    pub fn stop(&self) {
        self.stop_expected(None);
    }
    /// 换 profile 用：没有回复在跑才断开，下次连接按新 profile 起进程。
    pub(crate) fn disconnect_idle(&self) -> Result<()> {
        if !busy_ids(&self.inner.lock().unwrap().runs).is_empty() {
            return Err(format!("{} 正在回复，请完成或停止后再切换 profile", self.target.name()));
        }
        self.stop();
        Ok(())
    }
    pub(crate) fn stop_expected(&self, expected: Option<&str>) {
        let (client, active) = {
            let mut inner = self.inner.lock().unwrap();
            if expected.is_some_and(|id| !run_ref(&inner.runs, id).is_some_and(busy)) {
                return;
            }
            inner.generation = Uuid::new_v4().to_string();
            inner.connection = "disconnected".into();
            let extra: Vec<Arc<Client>> = inner.extra.drain().map(|(_, (_, client))| client).collect();
            (inner.client.take().into_iter().chain(extra).collect::<Vec<_>>(), busy_ids(&inner.runs))
        };
        for client in client {
            client.stop();
        }
        for id in active {
            self.finish(&id, "interrupted", None);
        }
        self.emit();
    }
}

#[cfg(test)]
mod model_catalog_tests {
    use super::{completion, effort_value, failure_reply, models_from_response, non_empty, restore_method};
    use serde_json::json;

    #[test]
    fn failure_text_hermes_returns_as_a_reply_counts_as_failed() {
        let billing = failure_reply("Billing or credits exhausted: HTTP 400: You have insufficient credits.\n\nAdd credits").unwrap();
        assert!(billing.contains("额度") && billing.contains("HTTP 400") && !billing.contains("Add credits"));
        assert!(failure_reply("API call failed after 3 retries: HTTP 503").unwrap().contains("调用模型失败"));
        assert_eq!(
            super::turn_usage([100, 1000, 0, 5], [150, 3000, 10, 25]),
            Some(json!({"inputTokens": 2060, "cachedReadTokens": 2000, "cachedWriteTokens": 10, "outputTokens": 20}))
        );
        assert_eq!(super::turn_usage([1, 2, 3, 4], [1, 2, 3, 4]), None);
        assert_eq!(failure_reply("## 交回\n统计完了：Billing or credits exhausted 这句不在开头"), None);
    }

    #[test]
    fn model_catalog_keeps_provider_routes_for_grouped_selection() {
        let models = models_from_response(&json!({"models":{"availableModels":[
            {"modelId":"openai:gpt-6","name":"GPT 6","description":"Provider: OpenAI • current"},
            {"modelId":"custom:private:sonnet","name":"Sonnet","description":"Provider: My Private API"}
        ]}}));
        assert_eq!(models[0].provider_id.as_deref(), Some("openai"));
        assert_eq!(models[0].provider_name.as_deref(), Some("OpenAI"));
        assert_eq!(models[1].provider_id.as_deref(), Some("My Private API"));
        assert_eq!(models[1].provider_name.as_deref(), Some("My Private API"));
    }

    #[test]
    fn stop_reasons_are_the_same_for_every_member() {
        assert_eq!(completion("DSH", Some("end_turn")), ("completed", None));
        assert_eq!(completion("DSH", Some("max_turn_requests")), ("completed", None));
        assert_eq!(completion("Claude", Some("cancelled")), ("interrupted", None));
        assert_eq!(
            completion("DSH", Some("max_tokens")),
            ("failed", Some("DSH 输出达到 token 上限，返回内容可能不完整".into()))
        );
        assert_eq!(completion("Hermes", None).0, "failed");
    }

    #[test]
    fn restore_method_follows_advertised_capabilities() {
        // 实测：DSH 只报 resume；OpenCode 两个都报；Hermes 和我们的桥只报 loadSession。
        assert_eq!(restore_method(&json!({"sessionCapabilities":{"close":{},"list":{},"resume":{}}})), "session/resume");
        assert_eq!(restore_method(&json!({"loadSession":true,"sessionCapabilities":{"resume":{}}})), "session/load");
        assert_eq!(restore_method(&json!({"loadSession":true})), "session/load");
        assert_eq!(restore_method(&json!(null)), "session/load");
    }

    #[test]
    fn effort_is_sent_only_when_the_agent_has_the_option() {
        let claude = json!({"configOptions":[{"id":"model","currentValue":"m"},{"id":"reasoning_effort","currentValue":"","options":[{"value":""},{"value":"high"}]}]});
        let dsh = json!({"configOptions":[{"id":"model","currentValue":"m"},{"id":"reasoning_effort","currentValue":"high","options":[{"value":"off"},{"value":"high"}]}]});
        let opencode = json!({"configOptions":[{"id":"model","currentValue":"m"},{"id":"mode","currentValue":"build"}]});
        assert_eq!(effort_value(&claude, None).as_deref(), Some(""), "Claude 没选时发空值回到 CLI 默认");
        assert_eq!(effort_value(&claude, Some("high".into())).as_deref(), Some("high"));
        assert_eq!(effort_value(&dsh, None), None, "DSH 没有空值选项，不发");
        assert_eq!(effort_value(&opencode, Some("high".into())), None);
        assert_eq!(non_empty(Some(String::new())), None);
    }
}
