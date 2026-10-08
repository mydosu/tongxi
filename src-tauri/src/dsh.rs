use crate::codex::{RunSnapshot, RuntimeSnapshot};
use crate::models::ModelOption;
use crate::rpc::Client;
use crate::store::{RunRecord, Store};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;
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
    active: Option<RunSnapshot>,
    models: Vec<ModelOption>,
    default_model: Option<String>,
    default_effort: Option<String>,
    loaded: HashSet<String>,
    last_checkpoint: Instant,
}
pub struct Runtime {
    app: tauri::AppHandle,
    store: Arc<Mutex<Store>>,
    directory: PathBuf,
    node: Option<PathBuf>,
    inner: Mutex<Inner>,
    revision: AtomicU64,
}
fn busy(run: &RunSnapshot) -> bool {
    matches!(
        run.record.status.as_str(),
        "starting" | "running" | "cancelling"
    )
}
fn current_option(response: &Value, id: &str) -> Option<String> {
    response["configOptions"]
        .as_array()?
        .iter()
        .find(|option| option["id"] == id)?["currentValue"]
        .as_str()
        .map(String::from)
}

fn models_from_response(response: &Value) -> Vec<ModelOption> {
    let Some(options) = response["configOptions"].as_array() else {
        return vec![];
    };
    let Some(model) = options.iter().find(|option| option["id"] == "model") else {
        return vec![];
    };
    let mut choices = vec![];
    fn collect(
        value: &Value,
        provider_id: Option<&str>,
        provider_name: Option<&str>,
        result: &mut Vec<ModelOption>,
    ) {
        if let Some(children) = value.as_array() {
            for child in children {
                collect(child, provider_id, provider_name, result);
            }
        } else if let Some(id) = value["value"].as_str() {
            let route_provider = serde_json::from_str::<Value>(id).ok().and_then(|route| {
                route
                    .as_array()
                    .and_then(|parts| parts.first())
                    .and_then(Value::as_str)
                    .map(String::from)
            });
            let provider_id = if provider_id == Some("provider") {
                route_provider.as_deref().or(provider_id)
            } else {
                provider_id
            };
            result.push(ModelOption {
                id: id.into(),
                name: value["name"].as_str().unwrap_or(id).into(),
                provider_id: provider_id.map(String::from),
                provider_name: provider_name.map(String::from),
                efforts: vec![],
                default_effort: None,
            });
        } else if value.is_object() {
            let group_id = value["group"].as_str().or(provider_id);
            let group_name = value["name"].as_str().or(provider_name);
            collect(&value["options"], group_id, group_name, result);
        }
    }
    collect(&model["options"], None, None, &mut choices);
    if let Some(selected) = choices
        .iter_mut()
        .find(|choice| Some(choice.id.as_str()) == model["currentValue"].as_str())
    {
        if let Some(effort) = options
            .iter()
            .find(|option| option["id"] == "reasoning_effort")
        {
            selected.efforts = effort["options"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|entry| {
                    entry["value"]
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .map(String::from)
                })
                .collect();
            selected.default_effort = effort["currentValue"]
                .as_str()
                .filter(|value| !value.is_empty())
                .map(String::from);
        }
    }
    choices
}
/// DSH 家目录：与桥侧解析一致——外层设了 AGENT_HUB_DSH_HOME 就用（测试隔离），
/// 否则用真实 ~/.dsh（必须已存在）；都没有返回 None，调用方回落到同席隔离目录。
fn resolved_dsh_home() -> Option<PathBuf> {
    std::env::var_os("AGENT_HUB_DSH_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .map(|value| PathBuf::from(value).join(".dsh"))
                .filter(|path| path.is_dir())
        })
}

/// 从 DSH 的会话投影缓存里取会话标题：优先会话已定下来的标题，
/// 没有就用会话里第一句用户输入；都没有返回 None（不编、不猜）。
/// 会话 id 两种写法（`session-<uuid>` / 裸 `<uuid>`）都指向同一个 `session-<uuid>.json`。
fn projected_title(home: &std::path::Path, session_id: &str) -> Option<String> {
    let uuid = session_id.strip_prefix("session-").unwrap_or(session_id);
    if uuid.is_empty() || uuid.contains('/') || uuid.contains('\\') {
        return None;
    }
    let path = home
        .join("storages")
        .join("session_projcache")
        .join("sessions")
        .join(format!("session-{uuid}.json"));
    let bytes = std::fs::read(path).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    let rows = &value["record"]["rows"];
    let title = rows["title"]["val"].as_str().map(str::trim);
    if let Some(title) = title.filter(|title| !title.is_empty()) {
        return Some(title.to_owned());
    }
    let first = rows["titleInput"]["val"]["first"]["text"].as_str()?.trim();
    if first.is_empty() {
        None
    } else {
        Some(first.to_owned())
    }
}

pub(crate) fn discover() -> Option<PathBuf> {
    let path = std::env::var_os("AGENT_HUB_DSH_NODE")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::split_paths(&std::env::var_os("PATH")?)
                .map(|directory| directory.join("node.exe"))
                .find(|path| path.is_file())
        })?;
    (path.is_absolute()
        && path.is_file()
        && path
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("node.exe")))
    .then_some(path)
}
impl Runtime {
    /// 只读列出该 agent 自己的原生会话（ACP session/list，ACP 标准方法）。
    pub fn list_sessions(&self) -> Result<Vec<crate::codex::NativeSession>> {
        let client = {
            let inner = self.inner.lock().unwrap();
            inner
                .client
                .clone()
                .ok_or_else(|| "DSH 未连接".to_string())?
        };
        let response = client.rpc_timeout(
            "session/list",
            serde_json::json!({}),
            std::time::Duration::from_secs(20),
        )?;
        let mut sessions = crate::codex::native_sessions_from_acp(&response);
        // 面板标题：尽量用 DSH 自己的投影缓存补上（已定标题 > 首句用户输入），
        // 读不到就保持原样，绝不编造。隔离模式下 home 是隔离目录，不碰真实 ~/.dsh。
        let home = resolved_dsh_home().unwrap_or_else(|| self.directory.join("dsh-native"));
        for session in &mut sessions {
            if session
                .title
                .as_deref()
                .is_none_or(|title| title.trim().is_empty())
            {
                session.title = projected_title(&home, &session.id);
            }
        }
        Ok(sessions)
    }

    pub fn new(app: tauri::AppHandle, store: Arc<Mutex<Store>>, directory: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            app,
            store,
            directory,
            node: discover(),
            revision: AtomicU64::new(1),
            inner: Mutex::new(Inner {
                connection: "disconnected".into(),
                generation: String::new(),
                client: None,
                version: None,
                error: None,
                active: None,
                models: vec![],
                default_model: None,
                default_effort: None,
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
            executable: self.node.as_ref().map(|path| path.to_string_lossy().into()),
            version: inner.version.clone(),
            error: inner.error.clone(),
            active: inner.active.clone(),
            models: inner.models.clone(),
            default_model: inner.default_model.clone(),
            default_effort: inner.default_effort.clone(),
        }
    }
    fn emit(&self) {
        let _ = self.app.emit("dsh-state", self.snapshot());
    }
    pub fn connect(self: &Arc<Self>) -> Result<RuntimeSnapshot> {
        let node = self
            .node
            .as_ref()
            .ok_or("未找到 Windows DSH Node 环境，请检查安装或 AGENT_HUB_DSH_NODE")?;
        let installation = crate::service_install::dsh_path(&self.directory)?;
        if !installation.is_absolute() || !installation.join("package.json").is_file() {
            return Err("DSH 安装目录不可用，请检查 AGENT_HUB_DSH_INSTALLATION".into());
        }
        let generation = Uuid::new_v4().to_string();
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.connection == "connected" {
                drop(inner);
                return Ok(self.snapshot());
            }
            if inner.connection == "connecting" {
                return Err("DSH 正在连接".into());
            }
            inner.connection = "connecting".into();
            inner.error = None;
            inner.generation = generation.clone();
        }
        self.emit();
        let result: Result<()> = (|| {
            let mut command = Command::new(node);
            command
                .args(["--input-type=module", "-e", include_str!("dsh_bridge.mjs")])
                .arg(&installation)
                .arg(self.directory.join("dsh-native"));
            command.current_dir(&self.directory);
            command.env(
                "AGENT_HUB_DSH_AUTH_BRIDGE",
                include_str!("dsh_hermes_auth.py"),
            );
            command.env_remove("AGENT_HUB_DSH_PROJECT_MODE");
            // 共用她自己的 DSH 家目录（同一个会话库）：外层设了就照用（测试隔离），
            // 否则指向真实的 ~/.dsh；库不存在时不给变量，桥回退到同席隔离目录。
            let dsh_home = std::env::var_os("AGENT_HUB_DSH_HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("USERPROFILE")
                        .or_else(|| std::env::var_os("HOME"))
                        .map(|value| PathBuf::from(value).join(".dsh"))
                        .filter(|path| path.is_dir())
                });
            if let Some(home) = dsh_home {
                command.env("AGENT_HUB_DSH_HOME", home);
            }
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
            let cwd = self.directory.join("dsh-workspaces").join("catalog");
            std::fs::create_dir_all(&cwd).map_err(|_| "DSH 目录创建失败")?;
            let catalog = client.rpc("session/new", json!({"cwd":cwd,"mcpServers":[]}))?;
            let default_model = current_option(&catalog, "model");
            let default_effort = current_option(&catalog, "reasoning_effort");
            let mut models = models_from_response(&catalog);
            let session_id = catalog["sessionId"].as_str().ok_or("DSH 未返回会话 ID")?;
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
            client.rpc("session/close", json!({"sessionId":session_id}))?;
            let mut inner = self.inner.lock().unwrap();
            if inner.generation != generation || inner.connection != "connecting" {
                return Err("DSH 连接已取消".into());
            }
            inner.version = std::fs::read(installation.join("package.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .and_then(|manifest| manifest["version"].as_str().map(String::from))
                .or_else(|| {
                    initialized["agentInfo"]["version"]
                        .as_str()
                        .map(String::from)
                });
            inner.models = models;
            inner.default_model = default_model;
            inner.default_effort = default_effort;
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
        let inner = self.inner.lock().map_err(|_| "DSH 状态不可用")?;
        if inner
            .active
            .as_ref()
            .is_some_and(|run| run.record.conversation_id == id && busy(run))
        {
            return Err("DSH 正在回复，请完成或停止后再修改会话".into());
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
            .validate_private_target(conversation_id, "dsh-win")?;
        if let Some(record) = existing {
            self.store
                .lock()
                .unwrap()
                .save_message(conversation_id, message_id, content)?;
            return Ok(record);
        }
        if inner.connection != "connected" {
            return Err("请先连接 DSH".into());
        }
        if inner.active.as_ref().is_some_and(busy) {
            return Err("DSH 正在处理上一条消息，请等待或停止".into());
        }
        let client = inner.client.clone().ok_or("DSH 接口不可用")?;
        let record = self.store.lock().unwrap().begin_agent_run(
            conversation_id,
            message_id,
            content,
            "dsh-win",
        )?;
        inner.active = Some(RunSnapshot {
            record: record.clone(),
            text: String::new(),
            thought: String::new(),
        });
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
        if inner.connection != "connected" || inner.active.as_ref().is_some_and(busy) {
            return Err("DSH 尚未连接或正在其他会话回复".into());
        }
        let client = inner.client.clone().ok_or("DSH 接口不可用")?;
        let (record, text) = self
            .store
            .lock()
            .unwrap()
            .begin_discussion_turn(discussion, "dsh-win", round)?;
        inner.active = Some(RunSnapshot {
            record: record.clone(),
            text: String::new(),
            thought: String::new(),
        });
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
            .active
            .as_ref()
            .filter(|run| run.record.id == run_id)
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
            .find(|session| session.agent_id == "dsh-win")
            .ok_or("DSH 会话不存在")?;
        {
            let inner = self.inner.lock().unwrap();
            let record = &inner
                .active
                .as_ref()
                .filter(|run| run.record.id == run_id)
                .ok_or("运行已结束")?
                .record;
            settings.model = record.model.clone();
            settings.reasoning_effort = record.reasoning_effort.clone();
            crate::models::validate_selection(
                &inner.models,
                settings.model.as_deref(),
                settings.reasoning_effort.as_deref(),
                inner.default_model.as_deref(),
            )?;
        }
        // 接入的原生会话要回到它自己的目录：DSH 按 cwd 分桶存会话，换目录 resume 会被拒。
        let cwd = match settings.native_cwd.as_deref() {
            Some(native) => std::path::PathBuf::from(native),
            None => {
                let local = self.directory.join("dsh-workspaces").join(&conversation_id);
                std::fs::create_dir_all(&local).map_err(|_| "DSH 会话目录创建失败")?;
                local
            }
        };
        let response = if let Some(session_id) = settings.native_session_id.as_ref() {
            let loaded = self.inner.lock().unwrap().loaded.contains(session_id);
            if loaded {
                Value::Null
            } else {
                client.rpc(
                    "session/resume",
                    json!({"sessionId":session_id,"cwd":cwd,"mcpServers":[]}),
                )?
            }
        } else {
            client.rpc("session/new", json!({"cwd":cwd,"mcpServers":[]}))?
        };
        let session_id = settings
            .native_session_id
            .clone()
            .or_else(|| response["sessionId"].as_str().map(String::from))
            .ok_or("DSH 未返回原生会话 ID")?;
        {
            let mut inner = self.inner.lock().unwrap();
            inner.loaded.insert(session_id.clone());
            let run = inner
                .active
                .as_mut()
                .filter(|run| run.record.id == run_id && busy(run))
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
        let (model, effort) = {
            let inner = self.inner.lock().unwrap();
            let model = settings
                .model
                .clone()
                .or_else(|| inner.default_model.clone())
                .ok_or("DSH 默认模型不可用")?;
            let effort = settings.reasoning_effort.clone().or_else(|| {
                inner
                    .models
                    .iter()
                    .find(|entry| entry.id == model)
                    .and_then(|entry| entry.default_effort.clone())
            });
            (model, effort)
        };
        client.rpc(
            "session/set_config_option",
            json!({"sessionId":session_id,"configId":"model","value":model}),
        )?;
        let applied = if let Some(effort) = &effort {
            client.rpc(
                "session/set_config_option",
                json!({"sessionId":session_id,"configId":"reasoning_effort","value":effort}),
            )?
        } else {
            json!({"configOptions":[]})
        };
        {
            let mut inner = self.inner.lock().unwrap();
            let run = inner
                .active
                .as_mut()
                .filter(|run| run.record.id == run_id && busy(run))
                .ok_or("运行已结束")?;
            if run.record.status == "cancelling" {
                drop(inner);
                self.finish(run_id, "interrupted", None);
                return Ok(());
            }
            self.store.lock().unwrap().record_model(
                &mut run.record,
                Some(model),
                current_option(&applied, "reasoning_effort"),
            )?;
            self.store
                .lock()
                .unwrap()
                .mark_turn(&mut run.record, run_id)?;
        }
        self.emit();
        let result = client.rpc_timeout(
            "session/prompt",
            json!({"sessionId":session_id,"prompt":[{"type":"text","text":content}]}),
            Duration::from_secs(600),
        );
        match result {
            Ok(value) => {
                let (status, error) = completion_from_stop_reason(value["stopReason"].as_str());
                self.finish(run_id, status, error.map(str::to_owned));
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
        if value["method"] != "session/update" {
            return;
        }
        let params = &value["params"];
        let mut inner = self.inner.lock().unwrap();
        if inner.generation != generation {
            return;
        }
        let checkpoint = inner.last_checkpoint.elapsed() >= Duration::from_millis(350);
        let Some(run) = inner.active.as_mut().filter(|run| {
            busy(run) && run.record.native_thread_id.as_deref() == params["sessionId"].as_str()
        }) else {
            return;
        };
        let update = &params["update"];
        if update["content"]["type"] != "text" {
            return;
        }
        let text = update["content"]["text"].as_str().unwrap_or("");
        // ACP 把「思考」和「正文」分成两种 chunk：思考进可折叠的思考块，正文进气泡。
        match update["sessionUpdate"].as_str().unwrap_or("") {
            "agent_thought_chunk" => run.thought.push_str(text),
            "agent_message_chunk" => run.text.push_str(text),
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
        if let Some(run) = inner
            .active
            .as_mut()
            .filter(|run| run.record.id == id && busy(run))
        {
            run.record.status = status.into();
            run.record.error = error;
            let _ = self
                .store
                .lock()
                .unwrap()
                .checkpoint(&run.record, &run.text, &run.thought);
        }
        drop(inner);
        self.emit();
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
            let client = inner.client.clone().ok_or("DSH 已断开")?;
            let run = inner
                .active
                .as_mut()
                .filter(|run| {
                    run.record.conversation_id == conversation_id
                        && busy(run)
                        && expected.is_none_or(|id| run.record.id == id)
                })
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
            inner
                .active
                .as_ref()
                .filter(|run| busy(run))
                .map(|run| run.record.id.clone())
        };
        if let Some(id) = id {
            self.finish(&id, "interrupted", Some("DSH 原生连接已断开".into()));
        }
        self.emit();
    }
    pub fn stop(&self) {
        self.stop_expected(None);
    }
    pub(crate) fn stop_expected(&self, expected: Option<&str>) {
        let (client, active) = {
            let mut inner = self.inner.lock().unwrap();
            if expected.is_some_and(|id| {
                !inner
                    .active
                    .as_ref()
                    .is_some_and(|run| run.record.id == id && busy(run))
            }) {
                return;
            }
            inner.generation = Uuid::new_v4().to_string();
            inner.connection = "disconnected".into();
            (
                inner.client.take(),
                inner
                    .active
                    .as_ref()
                    .filter(|run| busy(run))
                    .map(|run| run.record.id.clone()),
            )
        };
        if let Some(client) = client {
            client.stop();
        }
        if let Some(id) = active {
            self.finish(&id, "interrupted", None);
        }
        self.emit();
    }
}

fn completion_from_stop_reason(reason: Option<&str>) -> (&'static str, Option<&'static str>) {
    match reason {
        Some("cancelled") => ("interrupted", None),
        Some("end_turn" | "max_turn_requests" | "refusal") => ("completed", None),
        Some("max_tokens") => (
            "failed",
            Some("DSH 输出达到上游 token 上限，返回内容可能不完整"),
        ),
        _ => ("failed", Some("DSH 未返回正常完成状态")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `projected_title` 的取值顺序：已定标题 > 会话说第一句 > None；带路径分隔的 id 一律拒绝。
    #[test]
    fn projected_title_prefers_stored_title_then_first_input() {
        let root = std::env::temp_dir().join(format!("dsh-projcache-{}", Uuid::new_v4()));
        let dir = root
            .join("storages")
            .join("session_projcache")
            .join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        let write = |id: &str, body: &str| {
            std::fs::write(dir.join(format!("session-{id}.json")), body).unwrap()
        };
        write("set-1", r#"{"record":{"rows":{"title":{"val":"标题一"}}}}"#);
        write(
            "fallback-2",
            r#"{"record":{"rows":{"title":{"val":null},"titleInput":{"val":{"first":{"text":"只回两个字：在的。"}}}}}}"#,
        );
        write(
            "blank-3",
            r#"{"record":{"rows":{"title":{"val":"   "},"titleInput":{"val":{"first":{"text":"第一句"}}}}}}"#,
        );
        assert_eq!(
            projected_title(&root, "session-set-1").as_deref(),
            Some("标题一")
        );
        assert_eq!(
            projected_title(&root, "fallback-2").as_deref(),
            Some("只回两个字：在的。")
        );
        assert_eq!(projected_title(&root, "blank-3").as_deref(), Some("第一句"));
        assert_eq!(projected_title(&root, "missing"), None);
        assert_eq!(projected_title(&root, "a/b"), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn native_catalog_keeps_opaque_routes_and_model_dependent_efforts() {
        let response = json!({"configOptions":[
            {"id":"model","currentValue":"[\"provider\",\"flash\"]","options":[{"group":"provider","options":[{"value":"[\"provider\",\"flash\"]","name":"Flash"},{"value":"[\"provider\",\"pro\"]","name":"Pro"},{"value":"[\"provider-two\",\"flash\"]","name":"Other Flash"}]},{"group":"provider","options":[{"value":"[\"provider-two\",\"pro\"]","name":"Other Pro"}]}]},
            {"id":"reasoning_effort","currentValue":"high","options":[{"value":"off"},{"value":"low"},{"value":"high"}]}
        ]});
        let models = models_from_response(&response);
        assert_eq!(models.len(), 4);
        assert_eq!(models[0].provider_id.as_deref(), Some("provider"));
        assert_eq!(models[2].provider_id.as_deref(), Some("provider-two"));
        assert_eq!(models[0].efforts, ["off", "low", "high"]);
        assert!(models[1].efforts.is_empty());
        assert_eq!(models[0].default_effort.as_deref(), Some("high"));
        assert_eq!(
            current_option(&response, "model").as_deref(),
            Some("[\"provider\",\"flash\"]")
        );
    }

    #[test]
    fn project_bridge_does_not_publish_the_old_output_token_cap() {
        let bridge = include_str!("dsh_bridge.mjs");
        assert!(bridge.contains("contextWindow: 262144"));
        assert!(bridge.contains("maxTokensField: 'max_tokens'"));
        assert!(!bridge.contains("maxTokens: 16384"));
    }

    #[test]
    fn max_tokens_is_reported_as_incomplete_instead_of_success() {
        assert_eq!(
            completion_from_stop_reason(Some("max_tokens")),
            (
                "failed",
                Some("DSH 输出达到上游 token 上限，返回内容可能不完整")
            )
        );
        assert_eq!(
            completion_from_stop_reason(Some("end_turn")),
            ("completed", None)
        );
        assert_eq!(
            completion_from_stop_reason(Some("cancelled")),
            ("interrupted", None)
        );
    }
}
