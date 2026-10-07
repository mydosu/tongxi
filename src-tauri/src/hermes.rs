use crate::codex::{RunSnapshot, RuntimeSnapshot};
use crate::models::ModelOption;
use crate::rpc::Client;
use crate::store::{RunRecord, Store};
use serde_json::{json, Value};
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
    active: Option<RunSnapshot>,
    wire_text: String,
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
fn models_from_response(response: &Value) -> Vec<ModelOption> {
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

#[cfg(test)]
mod model_catalog_tests {
    use super::models_from_response;
    use serde_json::json;

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
impl Runtime {
    /// 只读列出该 agent 自己的原生会话（ACP session/list，ACP 标准方法）。
    pub fn list_sessions(&self) -> Result<Vec<crate::codex::NativeSession>> {
        let client = {
            let inner = self.inner.lock().unwrap();
            inner
                .client
                .clone()
                .ok_or_else(|| "Hermes 未连接".to_string())?
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
                active: None,
                wire_text: String::new(),
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
            active: inner.active.clone(),
            models: inner.models.clone(),
            default_model: inner.default_model.clone(),
            default_effort: None,
        }
    }
    fn emit(&self) {
        let _ = self
            .app
            .emit(&format!("{}-state", self.target.key()), self.snapshot());
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
            std::fs::create_dir_all(&cwd).map_err(|_| "Hermes 目录创建失败")?;
            let cwd = self.target.cwd(&cwd)?;
            let catalog = client.rpc("session/new", json!({"cwd":cwd,"mcpServers":[]}))?;
            let mut inner = self.inner.lock().unwrap();
            if inner.generation != generation || inner.connection != "connecting" {
                return Err("Hermes 连接已取消".into());
            }
            inner.version = initialized["agentInfo"]["version"]
                .as_str()
                .map(String::from);
            inner.models = models_from_response(&catalog);
            inner.default_model = catalog["models"]["currentModelId"]
                .as_str()
                .map(String::from);
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
        let inner = self.inner.lock().map_err(|_| "Hermes 状态不可用")?;
        if inner
            .active
            .as_ref()
            .is_some_and(|run| run.record.conversation_id == id && busy(run))
        {
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
        if inner.active.as_ref().is_some_and(busy) {
            return Err(format!(
                "{} 正在处理上一条消息，请等待或停止",
                self.target.name()
            ));
        }
        let client = inner.client.clone().ok_or("Hermes 接口不可用")?;
        let record = self.store.lock().unwrap().begin_agent_run(
            conversation_id,
            message_id,
            content,
            self.target.id(),
        )?;
        inner.active = Some(RunSnapshot {
            record: record.clone(),
            text: String::new(),
            thought: String::new(),
        });
        inner.wire_text.clear();
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
            return Err(format!("{} 尚未连接或正在其他会话回复", self.target.name()));
        }
        let client = inner.client.clone().ok_or("成员接口不可用")?;
        let (record, text) = self.store.lock().unwrap().begin_discussion_turn(
            discussion,
            self.target.id(),
            round,
        )?;
        inner.active = Some(RunSnapshot {
            record: record.clone(),
            text: String::new(),
            thought: String::new(),
        });
        inner.wire_text.clear();
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
            .find(|session| session.agent_id == self.target.id())
            .ok_or("Hermes 会话不存在")?;
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
        let cwd = match settings.native_cwd.as_deref() {
            // 原生会话自己的目录本来就是 agent 侧路径，续聊必须原样传回。
            Some(native) => native.to_owned(),
            None => {
                let local = self
                    .directory
                    .join(format!("{}-workspaces", self.target.key()))
                    .join(&conversation_id);
                std::fs::create_dir_all(&local).map_err(|_| "Hermes 会话目录创建失败")?;
                self.target.cwd(&local)?
            }
        };
        // 绑着的历史会话在库里找不到时（例如刚切到共用真实库、旧绑定指向同席自己的库），
        // 直接开一条新的，别让整个会话发不出消息；bind_thread 会把新 id 写回。
        let response = match settings.native_session_id.as_ref() {
            Some(session_id) => client
                .rpc(
                    "session/load",
                    json!({"sessionId":session_id,"cwd":cwd,"mcpServers":[]}),
                )
                .or_else(|_| {
                    client.rpc(
                        "session/new",
                        json!({"cwd":self.target.cwd(&self.directory.join(format!("{}-workspaces", self.target.key())).join(&conversation_id))?,"mcpServers":[]}),
                    )
                })?,
            None => client.rpc("session/new", json!({"cwd":cwd,"mcpServers":[]}))?,
        };
        let session_id = response["sessionId"]
            .as_str()
            .map(String::from)
            .or_else(|| settings.native_session_id.clone())
            .ok_or("Hermes 未返回原生会话 ID")?;
        let models = models_from_response(&response);
        {
            let mut inner = self.inner.lock().unwrap();
            inner.models = models;
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
        let selected_model = settings
            .model
            .as_ref()
            .cloned()
            .or_else(|| self.inner.lock().unwrap().default_model.clone());
        if let Some(model_id) = selected_model
            .filter(|model| response["models"]["currentModelId"].as_str() != Some(model.as_str()))
        {
            client.rpc(
                "session/set_model",
                json!({"sessionId":session_id,"modelId":model_id}),
            )?;
        }
        let applied = client.rpc("session/set_config_option", json!({"sessionId":session_id,"configId":"reasoning_effort","value":settings.reasoning_effort.as_deref().unwrap_or("default")}))?;
        let applied = &applied["_meta"]["agentHub"];
        if applied["toolCount"].as_u64() != Some(0) {
            return Err("Hermes 聊天模式工具隔离检查失败".into());
        }
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
                applied["model"].as_str().map(String::from),
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
            if applied["toolCount"].as_u64() != Some(0)
                || applied["digest_injected"].as_bool() != Some(!digest.trim().is_empty())
            {
                return Err("Hermes 摘要注入检查失败".into());
            }
        }
        self.emit();
        let result = client.rpc_timeout(
            "session/prompt",
            json!({"sessionId":session_id,"prompt":[{"type":"text","text":content}]}),
            Duration::from_secs(600),
        );
        match result {
            Ok(value) => {
                let status = match value["stopReason"].as_str() {
                    Some("cancelled") => "interrupted",
                    Some("end_turn" | "max_tokens" | "max_turn_requests" | "refusal") => {
                        "completed"
                    }
                    _ => "failed",
                };
                self.finish(
                    run_id,
                    status,
                    (status == "failed").then(|| "Hermes 未返回正常完成状态".into()),
                );
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
        let Inner {
            active, wire_text, ..
        } = &mut *inner;
        let Some(run) = active.as_mut().filter(|run| {
            busy(run) && run.record.native_thread_id.as_deref() == params["sessionId"].as_str()
        }) else {
            return;
        };
        let update = &params["update"];
        if update["content"]["type"] != "text" {
            return;
        }
        let text = update["content"]["text"].as_str().unwrap_or("");
        // 思考块与正文分开累积；Albion 的可见性过滤只作用于正文。
        match update["sessionUpdate"].as_str().unwrap_or("") {
            "agent_thought_chunk" => run.thought.push_str(text),
            "agent_message_chunk" => {
                if self.target.id() == "albion-wsl" {
                    wire_text.push_str(text);
                    run.text = crate::chat_text::albion_visible(wire_text);
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
        let visible = (self.target.id() == "albion-wsl")
            .then(|| crate::chat_text::albion_complete(&inner.wire_text));
        if let Some(run) = inner
            .active
            .as_mut()
            .filter(|run| run.record.id == id && busy(run))
        {
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
            let client = inner.client.clone().ok_or("Hermes 已断开")?;
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
            self.finish(&id, "interrupted", Some("Hermes 原生连接已断开".into()));
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
