//! Claude Code 成员：复用本机安装的 `claude` CLI，经 `claude_bridge.mjs` 以 ACP 接入。
//! 原生会话保存在用户自己的 ~/.claude 中，同席只记录会话 ID 与工作目录。
use crate::codex::{
    busy_ids, busy_in, headline, insert_run, live_in, run_mut, RunSnapshot, RuntimeSnapshot,
};
use crate::models::ModelOption;
use crate::rpc::Client;
use crate::store::{RunRecord, Store};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};
use tauri::Emitter;
use uuid::Uuid;

type Result<T> = std::result::Result<T, String>;

pub const AGENT_ID: &str = "claude-win";
const NATIVE_EXE: &str = "node_modules/@anthropic-ai/claude-code/bin/claude.exe";

struct Inner {
    connection: String,
    generation: String,
    client: Option<Arc<Client>>,
    version: Option<String>,
    error: Option<String>,
    runs: Vec<RunSnapshot>,
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

fn absolute_file(path: PathBuf) -> Option<PathBuf> {
    (path.is_absolute() && path.is_file()).then_some(path)
}

/// 原生 claude.exe：`AGENT_HUB_CLAUDE_EXE` 优先；否则 PATH 上的 claude.exe，
/// 或 npm 壳 claude.cmd 同级的包内原生程序；最后是官方安装器的 ~/.local/bin。
pub(crate) fn discover_executable() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("AGENT_HUB_CLAUDE_EXE").filter(|value| !value.is_empty())
    {
        return absolute_file(PathBuf::from(value));
    }
    crate::service_install::on_path(&["claude.exe"])
        .or_else(|| {
            crate::service_install::on_path_find(&["claude.cmd"], |shell| {
                absolute_file(shell.parent()?.join(NATIVE_EXE))
            })
        })
        .or_else(|| {
            let home = std::env::var_os("USERPROFILE")?;
            absolute_file(PathBuf::from(home).join(".local/bin/claude.exe"))
        })
}

/// `claude --version` 的首段，例如 `2.1.296 (Claude Code)` → `2.1.296`。
pub(crate) fn native_version(executable: &Path) -> Option<String> {
    let mut command = Command::new(executable);
    command.arg("--version");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let output = command.output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let version = text.split_whitespace().next()?;
    (output.status.success() && version.chars().next()?.is_ascii_digit())
        .then(|| version.to_owned())
}

/// Node 运行时：`AGENT_HUB_CLAUDE_NODE` 优先，否则与 DSH 同样从 PATH 查找。
fn discover_node() -> Option<PathBuf> {
    std::env::var_os("AGENT_HUB_CLAUDE_NODE")
        .filter(|value| !value.is_empty())
        .and_then(|value| absolute_file(PathBuf::from(value)))
        .or_else(crate::dsh::discover)
}

/// 同席若是从某个 Claude Code 里启动的，会带着它的 CLAUDE_CODE_* 标记；Claude 继承后会把自己
/// 当成嵌套子会话而不保存对话记录，原生会话就恢复不了，CLI 移交也导不回来。
pub(crate) fn nested_session_markers() -> Vec<std::ffi::OsString> {
    std::env::vars_os()
        .map(|(name, _)| name)
        .filter(|name| {
            let name = name.to_string_lossy().to_ascii_uppercase();
            name.starts_with("CLAUDE_CODE_") || name == "CLAUDECODE"
        })
        .collect()
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

impl Runtime {
    fn bridge(&self, data: &Path) -> Result<Command> {
        let node = self
            .node
            .as_ref()
            .ok_or("未找到 Node 运行时，请检查 PATH 或 AGENT_HUB_CLAUDE_NODE")?;
        let executable = discover_executable()
            .ok_or("未找到 Claude Code，请检查 PATH 或 AGENT_HUB_CLAUDE_EXE")?;
        let mut command = Command::new(node);
        command
            .args([
                "--input-type=module",
                "-e",
                include_str!("claude_bridge.mjs"),
            ])
            .arg(executable)
            .arg(data);
        for name in nested_session_markers() {
            command.env_remove(name);
        }
        Ok(command)
    }
    /// 只读列出 Claude 自己的原生会话（ACP session/list，由桥读取 ~/.claude/projects）。
    pub fn list_sessions(&self) -> Result<Vec<crate::codex::NativeSession>> {
        let client = {
            let inner = self.inner.lock().unwrap();
            inner.client.clone().ok_or("Claude 未连接")?
        };
        let response = client.rpc_timeout("session/list", json!({}), Duration::from_secs(20))?;
        Ok(crate::codex::native_sessions_from_acp(&response))
    }

    pub fn new(app: tauri::AppHandle, store: Arc<Mutex<Store>>, directory: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            app,
            store,
            directory,
            node: discover_node(),
            revision: AtomicU64::new(1),
            inner: Mutex::new(Inner {
                connection: "disconnected".into(),
                generation: String::new(),
                client: None,
                version: None,
                error: None,
                runs: Vec::new(),
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
            executable: discover_executable().map(|path| path.to_string_lossy().into()),
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
        let _ = self.app.emit("claude-state", crate::state_events::slim("claude-state", &self.snapshot()));
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
                return Err("Claude 正在连接".into());
            }
            inner.connection = "connecting".into();
            inner.error = None;
            inner.generation = generation.clone();
        }
        self.emit();
        let result: Result<()> = (|| {
            let mut command = self.bridge(&self.directory.join("claude-native"))?;
            command.current_dir(&self.directory);
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
            let initialized = client.rpc_timeout("initialize", json!({"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false},"clientInfo":{"name":"personal-agent-hub","version":env!("CARGO_PKG_VERSION")}}), Duration::from_secs(90))?;
            let cwd = self.directory.join("claude-workspaces").join("catalog");
            std::fs::create_dir_all(&cwd).map_err(|_| "Claude 目录创建失败")?;
            let catalog = client.rpc("session/new", json!({"cwd":cwd,"mcpServers":[]}))?;
            let default_model = crate::dsh::current_option(&catalog, "model");
            let mut models = crate::dsh::models_from_response(&catalog);
            let session_id = catalog["sessionId"]
                .as_str()
                .ok_or("Claude 未返回会话 ID")?;
            for model in &mut models {
                let response = client.rpc(
                    "session/set_config_option",
                    json!({"sessionId":session_id,"configId":"model","value":model.id}),
                )?;
                if let Some(capability) = crate::dsh::models_from_response(&response)
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
                return Err("Claude 连接已取消".into());
            }
            inner.version = initialized["agentInfo"]["version"]
                .as_str()
                .map(String::from);
            crate::pricing::remember(AGENT_ID, &models);
            inner.models = models;
            inner.default_model = default_model;
            inner.default_effort = None;
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
        let inner = self.inner.lock().map_err(|_| "Claude 状态不可用")?;
        if busy_in(&inner.runs, id) {
            return Err("Claude 正在回复，请完成或停止后再修改会话".into());
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
            .validate_private_target(conversation_id, AGENT_ID)?;
        if let Some(record) = existing {
            self.store
                .lock()
                .unwrap()
                .save_message(conversation_id, message_id, content)?;
            return Ok(record);
        }
        if inner.connection != "connected" {
            return Err("请先连接 Claude".into());
        }
        if busy_in(&inner.runs, conversation_id) {
            return Err("Claude 正在处理这个会话的上一条消息，请等待或停止".into());
        }
        let client = inner.client.clone().ok_or("Claude 接口不可用")?;
        let record = self.store.lock().unwrap().begin_agent_run(
            conversation_id,
            message_id,
            content,
            AGENT_ID,
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
            return Err("Claude 尚未连接".into());
        }
        let client = inner.client.clone().ok_or("Claude 接口不可用")?;
        let (record, text) = self
            .store
            .lock()
            .unwrap()
            .begin_discussion_turn(discussion, AGENT_ID, round)?;
        insert_run(
            &mut inner.runs,
            RunSnapshot {
                record: record.clone(),
                text: String::new(),
                thought: String::new(),
                tool_events: Vec::new(),
            },
        );
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
        let conversation_id = crate::codex::run_ref(&self.inner.lock().unwrap().runs, run_id)
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
            .find(|session| session.agent_id == AGENT_ID)
            .ok_or("Claude 会话不存在")?;
        {
            let inner = self.inner.lock().unwrap();
            let record = &crate::codex::run_ref(&inner.runs, run_id)
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
        // Claude 按工作目录归档会话；接入的原生会话回到它自己的目录续聊。
        let cwd = match settings.native_cwd.as_deref() {
            Some(native) => PathBuf::from(native),
            None => {
                let local = self.store.lock().unwrap().fallback_cwd(
                    &self.directory,
                    &conversation_id,
                    AGENT_ID,
                    "claude",
                    settings.native_session_id.is_some(),
                );
                std::fs::create_dir_all(&local).map_err(|_| "Claude 会话目录创建失败")?;
                local
            }
        };
        let project_db = self.store.lock().unwrap().path.clone();
        let mcp_servers =
            crate::project_tools::project_chat_mcp_server(&project_db, &conversation_id)?
                .map_or_else(|| json!([]), |server| json!([server]));
        let response = if let Some(session_id) = settings.native_session_id.as_ref() {
            client.rpc(
                "session/resume",
                json!({"sessionId":session_id,"cwd":cwd,"mcpServers":mcp_servers}),
            )?
        } else {
            client.rpc("session/new", json!({"cwd":cwd,"mcpServers":mcp_servers}))?
        };
        let session_id = settings
            .native_session_id
            .clone()
            .or_else(|| response["sessionId"].as_str().map(String::from))
            .ok_or("Claude 未返回原生会话 ID")?;
        {
            let mut inner = self.inner.lock().unwrap();
            inner.loaded.insert(session_id.clone());
            let run = run_mut(&mut inner.runs, run_id)
                .filter(|run| busy(run))
                .ok_or("运行已结束")?;
            self.store
                .lock()
                .unwrap()
                .bind_thread(&mut run.record, &session_id)?;
            if settings.native_cwd.is_none() {
                self.store.lock().unwrap().connection.execute(
                    "UPDATE sessions SET native_cwd=?1 WHERE conversation_id=?2 AND agent_id=?3 AND native_cwd IS NULL",
                    rusqlite::params![cwd.to_string_lossy(), conversation_id, AGENT_ID],
                ).map_err(|_| "Claude 会话目录保存失败")?;
            }
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
                .ok_or("Claude 默认模型不可用")?;
            (model, non_empty(settings.reasoning_effort.clone()))
        };
        client.rpc(
            "session/set_config_option",
            json!({"sessionId":session_id,"configId":"model","value":model}),
        )?;
        let applied = client.rpc(
            "session/set_config_option",
            json!({"sessionId":session_id,"configId":"reasoning_effort","value":effort.clone().unwrap_or_default()}),
        )?;
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
            self.store.lock().unwrap().record_model(
                &mut run.record,
                Some(model),
                non_empty(crate::dsh::current_option(&applied, "reasoning_effort")),
            )?;
            self.store
                .lock()
                .unwrap()
                .mark_turn(&mut run.record, run_id)?;
        }
        self.emit();
        let input = crate::attachments::prompt(&project_db, &conversation_id, content, false)?;
        let tier = crate::project_brain::access(&self.store.lock().unwrap().connection, &conversation_id).map_or("full", |(tier, _)| tier);
        let result = client.rpc_while_alive(
            "session/prompt",
            json!({"sessionId":session_id,"prompt":input,"tongxiTier":tier}),
            crate::rpc::QUIET_LIMIT,
        );
        match result {
            Ok(value) => {
                self.store.lock().unwrap().record_usage(run_id, &value);
                let (status, error) = completion_from_stop_reason(value["stopReason"].as_str());
                self.finish(run_id, status, error.map(str::to_owned));
            }
            Err(error) => {
                // 桥仍在：只是这一轮失败（额度、认证等），不必断开整个成员连接。
                self.finish(run_id, "failed", Some(error.clone()));
                return Err(error);
            }
        }
        Ok(())
    }
    fn notification(&self, generation: &str, value: Value) {
        if crate::project_brain::background_notice(&self.store, AGENT_ID, &value) {
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
        let Some(run) = inner.runs.iter_mut().find(|run| {
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
        match update["sessionUpdate"].as_str().unwrap_or("") {
            "agent_thought_chunk" => crate::store::push_thought(&mut run.thought, &run.text, text),
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
        if let Some(run) = run_mut(&mut inner.runs, id).filter(|run| busy(run)) {
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
    /// 把会话交给 CLI 前从桥里卸下这条会话；下次续聊重新载入，接上 CLI 里的新对话。
    pub(crate) fn release_native(&self, session_id: &str) {
        let client = {
            let mut inner = self.inner.lock().unwrap();
            inner.loaded.remove(session_id);
            inner.client.clone()
        };
        if let Some(client) = client {
            let _ = client.rpc("session/close", json!({"sessionId":session_id}));
        }
    }
    pub fn cancel(&self, conversation_id: &str) -> Result<RuntimeSnapshot> {
        self.cancel_expected(conversation_id, None)
    }
    /// 回复进行中插话：桥接把引导写进正在运行的 Claude 进程，Claude 在下一步原生采纳，不打断。
    pub fn steer(&self, conversation_id: &str, content: &str, message_id: &str) -> Result<()> {
        let (client, session) = {
            let inner = self.inner.lock().unwrap();
            let client = inner.client.clone().ok_or("Claude 已断开")?;
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
        let (client, session) = {
            let mut inner = self.inner.lock().unwrap();
            let client = inner.client.clone().ok_or("Claude 已断开")?;
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
            self.finish(&id, "interrupted", Some("Claude 原生连接已断开".into()));
        }
        self.emit();
    }
    pub fn stop(&self) {
        self.stop_expected(None);
    }
    pub(crate) fn stop_expected(&self, expected: Option<&str>) {
        let (client, active) = {
            let mut inner = self.inner.lock().unwrap();
            if expected.is_some_and(|id| !crate::codex::run_ref(&inner.runs, id).is_some_and(busy))
            {
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

fn completion_from_stop_reason(reason: Option<&str>) -> (&'static str, Option<&'static str>) {
    match reason {
        Some("cancelled") => ("interrupted", None),
        Some("end_turn" | "refusal") => ("completed", None),
        Some("max_tokens") => (
            "failed",
            Some("Claude 输出达到 token 上限，返回内容可能不完整"),
        ),
        _ => ("failed", Some("Claude 未返回正常完成状态")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_reasons_map_to_run_status() {
        assert_eq!(
            completion_from_stop_reason(Some("end_turn")),
            ("completed", None)
        );
        assert_eq!(
            completion_from_stop_reason(Some("cancelled")),
            ("interrupted", None)
        );
        assert_eq!(completion_from_stop_reason(Some("max_tokens")).0, "failed");
        assert_eq!(completion_from_stop_reason(None).0, "failed");
    }

    #[test]
    fn bridge_gives_members_the_full_cli() {
        let bridge = include_str!("claude_bridge.mjs");
        assert!(bridge.contains("else args.push('--permission-mode', 'bypassPermissions')"));
        assert!(bridge.contains("const denied = ['AskUserQuestion'"));
        // 宿主 Claude Code 的会话标记不能泄进成员子进程。
        assert!(bridge.contains("key === 'CLAUDECODE'"));
    }

    #[test]
    fn empty_effort_means_cli_default() {
        assert_eq!(non_empty(Some(String::new())), None);
        assert_eq!(non_empty(Some("high".into())).as_deref(), Some("high"));
    }
}
