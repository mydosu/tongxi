use crate::codex::{
    busy_ids, busy_in, headline, insert_run, live_in, run_mut, run_ref, RunSnapshot,
    RuntimeSnapshot,
};
use crate::models::ModelOption;
use crate::rpc::Client;
use crate::store::{RunRecord, Store};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};
use tauri::Emitter;
use uuid::Uuid;

type Result<T> = std::result::Result<T, String>;
struct Inner {
    connection: String,
    generation: String,
    client: Option<Arc<Client>>,
    version: Option<String>,
    error: Option<String>,
    runs: Vec<RunSnapshot>,
    /// Albion 的原始正文（按运行 ID），可见性过滤从它算出 `run.text`。
    wire_text: HashMap<String, String>,
    models: Vec<ModelOption>,
    default_model: Option<String>,
    last_checkpoint: Instant,
}
pub struct Runtime {
    app: tauri::AppHandle,
    store: Arc<Mutex<Store>>,
    directory: PathBuf,
    target: crate::acp_transport::Target,
    inner: Mutex<Inner>,
    revision: AtomicU64,
}
fn busy(run: &RunSnapshot) -> bool {
    matches!(
        run.record.status.as_str(),
        "starting" | "running" | "cancelling"
    )
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
fn session_tokens(session_id: &str) -> Option<[i64; 4]> {
    let home = std::env::var_os("HERMES_HOME")
        .map(PathBuf::from)
        .or_else(|| crate::service_updates::hermes_repo()?.parent().map(PathBuf::from))?;
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
/// 标准 ACP 的模型选项（configOptions 里 id=model，如 OpenCode）。
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
        let response = client.rpc_timeout(
            "session/list",
            serde_json::json!({}),
            std::time::Duration::from_secs(20),
        )?;
        Ok(crate::codex::native_sessions_from_acp(&response))
    }

    pub fn new(app: tauri::AppHandle, store: Arc<Mutex<Store>>, directory: PathBuf) -> Arc<Self> {
        Self::with_target(
            app,
            store,
            directory,
            crate::acp_transport::Target::windows(),
        )
    }
    pub fn with_target(
        app: tauri::AppHandle,
        store: Arc<Mutex<Store>>,
        directory: PathBuf,
        target: crate::acp_transport::Target,
    ) -> Arc<Self> {
        Arc::new(Self {
            app,
            store,
            directory,
            target,
            revision: AtomicU64::new(1),
            inner: Mutex::new(Inner {
                connection: "disconnected".into(),
                generation: String::new(),
                client: None,
                version: None,
                error: None,
                runs: Vec::new(),
                wire_text: HashMap::new(),
                models: vec![],
                default_model: None,
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
            default_effort: None,
        }
    }
    fn emit(&self) {
        let event = format!("{}-state", self.target.key());
        let _ = self.app.emit(&event, crate::state_events::slim(&event, &self.snapshot()));
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
            let weak = Arc::downgrade(self);
            let exit = weak.clone();
            let message_generation = generation.clone();
            let exit_generation = generation.clone();
            let client = Client::spawn_command(
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
            )?;
            self.inner.lock().unwrap().client = Some(client.clone());
            let initialized = client.rpc("initialize", json!({"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false},"clientInfo":{"name":"personal-agent-hub","version":env!("CARGO_PKG_VERSION")}}))?;
            let cwd = self
                .directory
                .join(format!("{}-workspaces", self.target.key()))
                .join("catalog");
            std::fs::create_dir_all(&cwd).map_err(|_| format!("{} 目录创建失败", self.target.name()))?;
            let cwd = self.target.cwd(&cwd)?;
            let catalog = client.rpc("session/new", json!({"cwd":cwd,"mcpServers":[]}))?;
            let mut inner = self.inner.lock().unwrap();
            if inner.generation != generation || inner.connection != "connecting" {
                return Err(format!("{} 连接已取消", self.target.name()));
            }
            inner.version = initialized["agentInfo"]["version"]
                .as_str()
                .map(String::from);
            inner.models = models_from_response(&catalog);
            crate::pricing::remember(self.target.id(), &inner.models);
            inner.default_model = catalog["models"]["currentModelId"]
                .as_str()
                .map(String::from)
                .or_else(|| crate::dsh::current_option(&catalog, "model"));
            // 同席自己定的默认模型优先（比如固定走某个提供商），目录里没有它才退回 harness 的默认。
            if let Some(preferred) = self
                .store
                .lock()
                .unwrap()
                .agent_default_model(self.target.id())
                .filter(|model| inner.models.iter().any(|entry| &entry.id == model))
            {
                inner.default_model = Some(preferred);
            }
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
        let mut inner = self.inner.lock().unwrap();
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
        drop(inner);
        self.emit();
        let runtime = self.clone();
        let run_id = record.id.clone();
        let text = content.trim().to_owned();
        std::thread::spawn(move || {
            if let Err(error) = runtime.prompt(&client, &run_id, &text) {
                runtime.finish(&run_id, "failed", Some(error));
            }
        });
        Ok(record)
    }
    pub(crate) fn send_discussion(
        self: &Arc<Self>,
        discussion: &str,
        round: u32,
    ) -> Result<RunRecord> {
        let mut inner = self.inner.lock().unwrap();
        if inner.connection != "connected" {
            return Err(format!("{} 尚未连接", self.target.name()));
        }
        let client = inner.client.clone().ok_or("成员接口不可用")?;
        let (record, text) = self.store.lock().unwrap().begin_discussion_turn(
            discussion,
            self.target.id(),
            round,
        )?;
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
        drop(inner);
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

    fn prompt(&self, client: &Client, run_id: &str, content: &str) -> Result<()> {
        let conversation_id = self
            .inner
            .lock()
            .unwrap()
            .runs
            .iter()
            .find(|run| run.record.id == run_id)
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
        let cwd = match settings.native_cwd.as_deref() {
            // 原生会话自己的目录本来就是 agent 侧路径，续聊必须原样传回。
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
        // 绑着的历史会话在库里找不到时（例如刚切到共用真实库、旧绑定指向同席自己的库），
        // 直接开一条新的，别让整个会话发不出消息；bind_thread 会把新 id 写回。
        let response = match settings.native_session_id.as_ref() {
            Some(session_id) => client
                .rpc(
                    "session/load",
                    json!({"sessionId":session_id,"cwd":cwd,"mcpServers":mcp_servers.clone()}),
                )
                .or_else(|_| {
                    client.rpc(
                        "session/new",
                        json!({"cwd":self.target.cwd(&self.directory.join(format!("{}-workspaces", self.target.key())).join(&conversation_id))?,"mcpServers":mcp_servers.clone()}),
                    )
                })?,
            None => client.rpc("session/new", json!({"cwd":cwd,"mcpServers":mcp_servers.clone()}))?,
        };
        let session_id = response["sessionId"]
            .as_str()
            .map(String::from)
            .or_else(|| settings.native_session_id.clone())
            .ok_or_else(|| format!("{} 未返回原生会话 ID", self.target.name()))?;
        let standard = standard_models(&response);
        let models = models_from_response(&response);
        {
            let mut inner = self.inner.lock().unwrap();
            crate::pricing::remember(self.target.id(), &models);
            inner.models = models;
            let run = run_mut(&mut inner.runs, run_id)
                .filter(|run| busy(run))
                .ok_or("运行已结束")?;
            self.store
                .lock()
                .unwrap()
                .bind_thread(&mut run.record, &session_id)?;
            if run.record.status == "cancelling" {
                drop(inner);
                self.finish(run_id, "interrupted", None);
                return Ok(());
            }
        }
        let selected_model = settings
            .model
            .as_ref()
            .cloned()
            .or_else(|| self.inner.lock().unwrap().default_model.clone());
        let applied = if standard {
            // 标准 ACP：模型走 configOptions；只读档切到它自带的 plan 模式（不许改文件），别的档回到默认 build。
            if let Some(model_id) = selected_model.as_ref() {
                client.rpc("session/set_config_option", json!({"sessionId":session_id,"configId":"model","value":model_id}))?;
            }
            if crate::dsh::current_option(&response, "mode").is_some() {
                let read = crate::project_brain::access(&self.store.lock().unwrap().connection, &conversation_id).is_some_and(|(tier, _)| tier == "read");
                client.rpc("session/set_config_option", json!({"sessionId":session_id,"configId":"mode","value":if read {"plan"} else {"build"}}))?;
            }
            Value::Null
        } else {
            if let Some(model_id) = selected_model.as_ref() {
                client.rpc(
                    "session/set_model",
                    json!({"sessionId":session_id,"modelId":model_id}),
                )?;
            }
            client.rpc("session/set_config_option", json!({"sessionId":session_id,"configId":"reasoning_effort","value":settings.reasoning_effort.as_deref().unwrap_or("default")}))?
        };
        let applied = &applied["_meta"]["agentHub"];
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
            let effort = if applied["reasoningConfig"]["enabled"] == false {
                Some("none".into())
            } else {
                applied["reasoningConfig"]["effort"]
                    .as_str()
                    .map(String::from)
            };
            self.store.lock().unwrap().record_model(
                &mut run.record,
                applied["modelId"]
                    .as_str()
                    .map(String::from)
                    .or_else(|| selected_model.clone())
                    .or_else(|| applied["model"].as_str().map(String::from)),
                effort,
            )?;
        }
        if self.target.id() == "albion-wsl" {
            let digest = {
                let store = self.store.lock().unwrap();
                if store.conversation(&conversation_id)?.kind == "direct" {
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
        self.emit();
        let input = crate::attachments::prompt(&project_db, &conversation_id, content, false)?;
        let before = (self.target.id() == "hermes-win").then(|| session_tokens(&session_id)).flatten();
        let result = client.rpc_while_alive(
            "session/prompt",
            json!({"sessionId":session_id,"prompt":input}),
            crate::rpc::QUIET_LIMIT,
        );
        match result {
            Ok(mut value) => {
                if !value["usage"].is_object() {
                    if let Some(usage) = before.zip(session_tokens(&session_id)).and_then(|(before, after)| turn_usage(before, after)) {
                        value["usage"] = usage;
                    }
                }
                self.store.lock().unwrap().record_usage(run_id, &value);
                let mut status = match value["stopReason"].as_str() {
                    Some("cancelled") => "interrupted",
                    Some("end_turn" | "max_tokens" | "max_turn_requests" | "refusal") => {
                        "completed"
                    }
                    _ => "failed",
                };
                let mut error = (status == "failed").then(|| format!("{} 未返回正常完成状态", self.target.name()));
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
            }
            Err(error) => {
                self.finish(run_id, "failed", Some(error.clone()));
                self.stop();
                return Err(error);
            }
        }
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
        if inner.generation != generation {
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
        // 思考块与正文分开累积；Albion 的可见性过滤只作用于正文。
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
        let client = {
            let inner = self.inner.lock().unwrap();
            inner.client.clone()
        };
        if let Some(client) = client {
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
    pub(crate) fn cancel_expected(
        &self,
        conversation_id: &str,
        expected: Option<&str>,
    ) -> Result<RuntimeSnapshot> {
        let (client, session) = {
            let mut inner = self.inner.lock().unwrap();
            let client = inner.client.clone().ok_or_else(|| format!("{} 已断开", self.target.name()))?;
            let run = live_in(&mut inner.runs, conversation_id)
                .filter(|run| expected.is_none_or(|id| run.record.id == id))
                .ok_or("这个会话没有正在进行的回复")?;
            run.record.status = "cancelling".into();
            self.store
                .lock()
                .unwrap()
                .checkpoint(&run.record, &run.text, &run.thought)?;
            (client, run.record.native_thread_id.clone())
        };
        self.emit();
        if let Some(session_id) = session {
            client.write(&json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session_id}}))?;
        }
        Ok(self.snapshot())
    }
    fn lost(&self, generation: &str) {
        let id = {
            let mut inner = self.inner.lock().unwrap();
            if inner.generation != generation {
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
    pub(crate) fn stop_expected(&self, expected: Option<&str>) {
        let (client, active) = {
            let mut inner = self.inner.lock().unwrap();
            if expected.is_some_and(|id| !run_ref(&inner.runs, id).is_some_and(busy)) {
                return;
            }
            inner.generation = Uuid::new_v4().to_string();
            inner.connection = "disconnected".into();
            (inner.client.take(), busy_ids(&inner.runs))
        };
        if let Some(client) = client {
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
    use super::{failure_reply, models_from_response};
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
}
