//! Dedicated native project turns; private/group harnesses retain their own histories.
use crate::project_store::Attempt;
use crate::project_tools::{tool_specs, Broker};
use crate::rpc::Client;
use crate::store::Store;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, String>;
pub type Notify = Arc<dyn Fn(&str) + Send + Sync>;

pub struct Runner {
    directory: PathBuf,
    store: Arc<Mutex<Store>>,
    notify: Notify,
    /// attempt id → (workflow id, 客户端)。按 attempt 存：并行任务各自独立子进程，
    /// 取消时按 workflow 过滤，不会互相覆盖。
    active: Mutex<HashMap<String, (String, Arc<Client>)>>,
    stopping: AtomicBool,
}
struct Wire {
    attempt: Attempt,
    pieces: Vec<(String, String)>,
    completed: Option<String>,
    failure: Option<String>,
    final_plan: Option<String>,
    last_plan_message: Option<String>,
    disconnected: bool,
    invalid: bool,
    last_save: Instant,
}

/// Only protocol enum variants and a bounded HTTP status may reach the UI.
/// Upstream messages/additionalDetails can contain configuration or credentials.
fn failure_category(error: &Value) -> Option<String> {
    let info = &error["codexErrorInfo"];
    if info.is_null() {
        return None;
    }
    let name = if let Some(name) = info.as_str() {
        let label = match name {
            "contextWindowExceeded" => "上下文超限",
            "sessionBudgetExceeded" | "usageLimitExceeded" => "额度限制",
            "rateLimitExceeded" => "请求频率限制",
            "flexUnavailable" | "serverOverloaded" => "服务繁忙",
            "unauthorized" => "认证失败",
            "badRequest" => "请求不受支持",
            "cyberPolicy" | "misalignmentPolicyViolation" | "tooManyDenials" => "原生策略拒绝",
            "internalServerError" => "服务内部错误",
            "sandboxError" => "原生权限错误",
            _ => "其他原生错误",
        };
        return Some(label.into());
    } else {
        [
            ("httpConnectionFailed", "HTTP 连接失败"),
            ("responseStreamConnectionFailed", "回复连接失败"),
            ("responseStreamDisconnected", "回复连接中断"),
        ]
        .into_iter()
        .find(|(key, _)| info.get(key).is_some())
    };
    if let Some((key, label)) = name {
        let status = info[key]["httpStatusCode"]
            .as_u64()
            .filter(|status| (100..=599).contains(status));
        return Some(
            status.map_or_else(|| label.into(), |status| format!("{label}，HTTP {status}")),
        );
    }
    Some("其他原生错误".into())
}

/// Some ACP providers return quota failures as assistant text and still report `end_turn`.
/// Do not mistake that transport-level error message for a successful project deliverable.
fn provider_error_reply(output: &str) -> Option<&'static str> {
    let output = output.to_ascii_lowercase();
    [
        "http 429",
        "weekly usage limit",
        "rate limit exceeded",
        "rate-limited",
        "too many requests",
        "quota exceeded",
    ]
    .iter()
    .any(|marker| output.contains(marker))
    .then_some("项目成员的模型服务返回额度或频率限制，未完成本阶段")
}

impl Wire {
    fn completed_message(&mut self, id: &str, text: &str, phase: Option<&str>) {
        self.text(id, text, true);
        if matches!(self.attempt.stage.as_str(), "plan" | "repair")
            && self.attempt.agent_id == "codex-win"
        {
            self.last_plan_message = Some(text.into());
            if phase == Some("final_answer") {
                self.final_plan = Some(text.into());
            }
        }
    }
    fn finish_plan_output(&mut self) {
        if !matches!(self.attempt.stage.as_str(), "plan" | "repair")
            || self.attempt.agent_id != "codex-win"
        {
            return;
        }
        if let Some(text) = self
            .final_plan
            .take()
            .or_else(|| self.last_plan_message.take())
        {
            self.pieces.clear();
            self.text("codex-plan-final", &text, true);
        }
    }
    fn text(&mut self, id: &str, text: &str, replace: bool) {
        if let Some((_, old)) = self.pieces.iter_mut().find(|(item, _)| item == id) {
            if replace {
                *old = text.into();
            } else {
                old.push_str(text);
            }
        } else {
            self.pieces.push((id.into(), text.into()));
        }
        self.attempt.output = self
            .pieces
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        if self.attempt.output.len() > 128_000 {
            self.invalid = true;
        }
    }
    fn append_thought(&mut self, text: &str) {
        if self.attempt.thought.len().saturating_add(text.len()) > 128_000 {
            self.invalid = true;
            return;
        }
        self.attempt.thought.push_str(text);
    }
    fn accepts(&self, params: &Value) -> bool {
        self.attempt.native_thread_id.as_deref() == params["threadId"].as_str()
            && params["threadId"].is_string()
            && self.attempt.native_turn_id.as_deref().is_none_or(|turn| {
                params["turnId"].as_str() == Some(turn)
                    || params["turn"]["id"].as_str() == Some(turn)
            })
    }
}

fn persist(store: &Arc<Mutex<Store>>, notify: &Notify, wire: &mut Wire, force: bool) {
    if !force && wire.last_save.elapsed() < Duration::from_millis(350) {
        return;
    }
    if store
        .lock()
        .unwrap()
        .checkpoint_attempt(&wire.attempt)
        .is_err()
    {
        wire.invalid = true;
        return;
    }
    wire.last_save = Instant::now();
    notify(&wire.attempt.workflow_id);
}

fn codex_command(executable: &Path) -> Result<Command> {
    // 不关闭 Codex CLI 的原生工具、MCP 或插件；项目 turn 会显式启用完整工具权限。
    let mut command = Command::new(executable);
    command.args(["app-server", "--listen", "stdio://"]);
    Ok(command)
}

impl Runner {
    pub fn new(directory: PathBuf, store: Arc<Mutex<Store>>, notify: Notify) -> Arc<Self> {
        Arc::new(Self {
            directory,
            store,
            notify,
            active: Mutex::new(HashMap::new()),
            stopping: AtomicBool::new(false),
        })
    }
    pub fn cancelled(&self, workflow: &str) -> bool {
        self.stopping.load(Ordering::SeqCst)
            || self
                .store
                .lock()
                .unwrap()
                .workflow(workflow)
                .map(|job| job.status == "cancelling" || !crate::project_store::live(&job.status))
                .unwrap_or(true)
    }
    pub fn cancel(&self, workflow: &str) {
        // 先把要停的客户端 clone 到锁外再逐个 stop，别抱着锁调 stop()。
        let clients = self
            .active
            .lock()
            .unwrap()
            .values()
            .filter(|(workflow_id, _)| workflow_id == workflow)
            .map(|(_, client)| client.clone())
            .collect::<Vec<_>>();
        for client in clients {
            client.stop();
        }
    }
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        let clients = self
            .active
            .lock()
            .unwrap()
            .values()
            .map(|(_, client)| client.clone())
            .collect::<Vec<_>>();
        for client in clients {
            client.stop();
        }
    }

    pub fn run(
        &self,
        attempt: Attempt,
        root: &Path,
        prompt: &str,
        deadline: Instant,
    ) -> Result<Attempt> {
        if self.cancelled(&attempt.workflow_id) {
            return Err("项目协作已停止".into());
        }
        let wire = Arc::new(Mutex::new(Wire {
            attempt,
            pieces: vec![],
            completed: None,
            failure: None,
            final_plan: None,
            last_plan_message: None,
            disconnected: false,
            invalid: false,
            last_save: Instant::now(),
        }));
        let folder = self
            .directory
            .join("project-native")
            .join(&wire.lock().unwrap().attempt.id);
        std::fs::create_dir_all(&folder).map_err(|_| "项目原生会话目录创建失败")?;
        let agent = wire.lock().unwrap().attempt.agent_id.clone();
        let mut command = match agent.as_str() {
            "codex-win" => {
                codex_command(&crate::codex::discover_executable().ok_or("未找到 Codex 原生程序")?)?
            }
            "hermes-win" => crate::acp_transport::Target::windows().project_command(&folder)?,
            "dsh-win" => {
                let mut command =
                    Command::new(crate::dsh::discover().ok_or("未找到 DSH Node 环境")?);
                // 编码子进程必须使用当前安装记录里的 DSH SDK，避免更新后私聊已用新 SDK 而编码仍解析旧路径。
                let installation = crate::service_install::dsh_path(&self.directory)?;
                command
                    .args(["--input-type=module", "-e", include_str!("dsh_bridge.mjs")])
                    .arg(installation)
                    .arg(folder.join("dsh-native"));
                command
                    .env(
                        "AGENT_HUB_DSH_AUTH_BRIDGE",
                        include_str!("dsh_hermes_auth.py"),
                    )
                    .env("AGENT_HUB_DSH_PROJECT_MODE", "1");
                command
            }
            _ => return Err("不支持此项目成员".into()),
        };
        command.current_dir(&folder);
        let notifications = wire.clone();
        let exited = wire.clone();
        let store = self.store.clone();
        let notify = self.notify.clone();
        let is_codex = agent == "codex-win";
        let client = Client::spawn_command(
            command,
            move |value| {
                let mut state = notifications.lock().unwrap();
                let params = &value["params"];
                let mut force = false;
                if is_codex {
                    if !state.accepts(params) {
                        return;
                    }
                    if state.attempt.native_turn_id.is_none() && value["method"] != "turn/started" {
                        return;
                    }
                    match value["method"].as_str().unwrap_or("") {
                        "turn/started" => {
                            if let Some(turn) = params["turn"]["id"].as_str() {
                                state.attempt.native_turn_id = Some(turn.into());
                                state.attempt.status = "running".into();
                                force = true;
                            }
                        }
                        "item/agentMessage/delta" => {
                            if let (Some(id), Some(text)) =
                                (params["itemId"].as_str(), params["delta"].as_str())
                            {
                                state.text(id, text, false);
                            }
                        }
                        "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta" => {
                            if let Some(text) = params["delta"].as_str() {
                                state.append_thought(text);
                            }
                        }
                        "item/completed" if params["item"]["type"] == "agentMessage" => {
                            if let (Some(id), Some(text)) = (
                                params["item"]["id"].as_str(),
                                params["item"]["text"].as_str(),
                            ) {
                                state.completed_message(id, text, params["item"]["phase"].as_str());
                            }
                        }
                        "turn/completed" => {
                            state.completed =
                                Some(params["turn"]["status"].as_str().unwrap_or("failed").into());
                            if let Some(category) = failure_category(&params["turn"]["error"]) {
                                state.failure = Some(category);
                            }
                            if state.completed.as_deref() == Some("completed") {
                                state.finish_plan_output();
                            }
                            force = true;
                        }
                        "error" => {
                            if let Some(category) = failure_category(&params["error"]) {
                                state.failure = Some(category);
                            }
                            return;
                        }
                        _ => return,
                    }
                } else {
                    if value["method"] != "session/update"
                        || state.attempt.native_thread_id.as_deref() != params["sessionId"].as_str()
                        || params["sessionId"].is_null()
                    {
                        return;
                    }
                    let update = &params["update"];
                    if update["content"]["type"] != "text" {
                        return;
                    }
                    let text = update["content"]["text"].as_str().unwrap_or("");
                    match update["sessionUpdate"].as_str().unwrap_or("") {
                        "agent_message_chunk" => state.text("acp-message", text, false),
                        "agent_thought_chunk" => state.append_thought(text),
                        _ => return,
                    }
                }
                persist(&store, &notify, &mut state, force);
            },
            move || {
                exited.lock().unwrap().disconnected = true;
            },
        )?;
        let (workflow, attempt_id) = {
            let state = wire.lock().unwrap();
            (state.attempt.workflow_id.clone(), state.attempt.id.clone())
        };
        self.active
            .lock()
            .unwrap()
            .insert(attempt_id.clone(), (workflow.clone(), client.clone()));
        let outcome = (|| -> Result<()> {
            if self.cancelled(&workflow) {
                return Err("项目协作已停止".into());
            }
            if is_codex {
                self.codex_turn(&client, &wire, root, prompt, deadline)
            } else {
                self.acp_turn(&client, &wire, root, prompt, deadline)
            }
        })();
        // Terminal state and lease release happen only after the owned process tree stops.
        client.stop();
        self.active.lock().unwrap().remove(&attempt_id);
        let mut result = wire.lock().unwrap().attempt.clone();
        if self.cancelled(&workflow) {
            result.status = "interrupted".into();
            result.error = Some("项目协作已停止，已保留现有文件和回复".into());
        } else if outcome.is_err() || wire.lock().unwrap().invalid {
            result.status = "failed".into();
            result.error = Some(
                outcome
                    .err()
                    .unwrap_or_else(|| "原生项目输出或状态保存无效".into()),
            );
        } else {
            result.status = "completed".into();
        }
        self.store.lock().unwrap().checkpoint_attempt(&result)?;
        (self.notify)(&workflow);
        Ok(result)
    }

    fn codex_turn(
        &self,
        client: &Arc<Client>,
        wire: &Arc<Mutex<Wire>>,
        root: &Path,
        prompt: &str,
        deadline: Instant,
    ) -> Result<()> {
        let stage = wire.lock().unwrap().attempt.stage.clone();
        if !matches!(stage.as_str(), "plan" | "implement" | "repair" | "review") {
            return Err("当前项目步骤不支持此原生成员".into());
        }
        // Agent Hub 的附加文件工具按阶段区分；CLI 自带工具不受此集合限制。
        let writable = matches!(stage.as_str(), "implement" | "repair");
        client.rpc("initialize",json!({"clientInfo":{"name":"agent_hub_project","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}}))?;
        client.write(&json!({"method":"initialized","params":{}}))?;
        let db = self.store.lock().unwrap().path.clone();
        let attempt = wire.lock().unwrap().attempt.clone();
        let broker = Arc::new(Mutex::new(Broker::open(&db, &attempt.id)?));
        let binding = wire.clone();
        client.set_request_handler(Arc::new(move |method, params| {
            if method != "item/tool/call" {
                return Some(Ok(match method {
                    "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                        json!({"decision":"decline"})
                    }
                    "item/tool/requestUserInput" => json!({"answers":{}}),
                    _ => return None,
                }));
            }
            let state = binding.lock().unwrap();
            let authorized = !state.invalid
                && state.attempt.native_thread_id.as_deref() == params["threadId"].as_str()
                && state.attempt.native_turn_id.is_some()
                && state.attempt.native_turn_id.as_deref() == params["turnId"].as_str();
            drop(state);
            let result = if authorized {
                broker.lock().unwrap().call(
                    params["tool"].as_str().unwrap_or(""),
                    params["arguments"].clone(),
                )
            } else {
                Err("项目工具调用身份不匹配".into())
            };
            let (text, success) = match result {
                Ok(value) => (value.to_string(), true),
                Err(error) => (error, false),
            };
            Some(Ok(
                json!({"contentItems":[{"type":"inputText","text":text}],"success":success}),
            ))
        }));
        let tools = tool_specs()
            .into_iter()
            .filter(|tool| {
                let name = tool["name"].as_str().unwrap_or("");
                writable || matches!(name, "hub_list" | "hub_read" | "hub_message" | "hub_inbox")
            })
            .map(|mut tool| {
                tool["type"] = json!("function");
                tool["deferLoading"] = json!(false);
                tool
            })
            .collect::<Vec<_>>();
        let instructions = match stage.as_str() {
            "plan" => {
                "你是本项目群的规划成员 Codex。先理解本轮明确目标，再分析需求和必要源码，之后输出结构化方案。可使用本机 Codex CLI 已启用的工具、MCP 与插件，也可用 hub_list/hub_read 查看项目文件，使用 hub_message(to_agent,content) 联系项目成员、hub_inbox() 读取消息。框架会自动执行合格方案，不等待用户再次确认。"
            }
            "implement" => {
                "你是本项目的实现者。按当前任务目标完成实现，使用本机 Codex CLI 已启用的工具、MCP 与插件，也可用 hub_list/hub_read/hub_write/hub_edit/hub_delete 操作项目文件，用 hub_message/hub_inbox 与其他项目成员直接沟通。完成后简短说明实现内容；最终由验收角色对照需求与源码复核。"
            }
            "review" => {
                "你是本项目的验收者。依据本轮目标、方案和源码独立判断项目是否达到目的。可使用本机 Codex CLI 已启用的工具、MCP 与插件，并用 hub_message/hub_inbox 与执行者直接沟通。按提示词要求的结构化 JSON 给出结论。"
            }
            _ => {
                "你是本项目的修复者。验收角色对照用户需求和源码后指出了具体缺陷；据此完成修复。可使用本机 Codex CLI 已启用的工具、MCP 与插件，并用 hub_message/hub_inbox 与验收者沟通。修复后由验收角色再次复核。"
            }
        };
        let thread=client.rpc("thread/start",json!({"cwd":root,"approvalPolicy":"never","sandbox":"danger-full-access","ephemeral":true,"model":attempt.model,"config":{"model_reasoning_effort":attempt.reasoning_effort},"developerInstructions":instructions,"dynamicTools":tools}))?;
        let thread_id = thread["thread"]["id"]
            .as_str()
            .ok_or("Codex 项目会话 ID 缺失")?
            .to_owned();
        {
            let mut state = wire.lock().unwrap();
            state.attempt.native_thread_id = Some(thread_id.clone());
            persist(&self.store, &self.notify, &mut state, true);
        }
        if self.cancelled(&attempt.workflow_id) {
            return Err("项目协作已停止".into());
        }
        let turn=client.rpc("turn/start",json!({"threadId":thread_id,"input":[{"type":"text","text":prompt}],"model":attempt.model,"effort":attempt.reasoning_effort,"approvalPolicy":"never","sandboxPolicy":{"type":"dangerFullAccess"}}))?;
        let turn_id = turn["turn"]["id"]
            .as_str()
            .ok_or("Codex 项目运行 ID 缺失")?
            .to_owned();
        {
            let mut state = wire.lock().unwrap();
            if state
                .attempt
                .native_turn_id
                .as_ref()
                .is_some_and(|id| id != &turn_id)
            {
                return Err("Codex 项目运行身份不匹配".into());
            }
            state.attempt.native_turn_id = Some(turn_id);
            state.attempt.status = "running".into();
            persist(&self.store, &self.notify, &mut state, true);
        }
        loop {
            let state = wire.lock().unwrap();
            if let Some(status) = &state.completed {
                return if status == "completed" {
                    Ok(())
                } else {
                    Err(state.failure.as_ref().map_or_else(
                        || "Codex 项目运行未正常完成".into(),
                        |category| format!("Codex 项目运行未正常完成（{category}）"),
                    ))
                };
            }
            if state.invalid || state.disconnected {
                return Err("Codex 项目连接或状态无效".into());
            }
            drop(state);
            if self.cancelled(&attempt.workflow_id) {
                return Err("项目协作已停止".into());
            }
            if Instant::now() >= deadline {
                return Err("项目协作超时，已停止当前原生任务".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn acp_turn(
        &self,
        client: &Arc<Client>,
        wire: &Arc<Mutex<Wire>>,
        root: &Path,
        prompt: &str,
        deadline: Instant,
    ) -> Result<()> {
        client.rpc("initialize",json!({"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false},"clientInfo":{"name":"agent_hub_project","version":env!("CARGO_PKG_VERSION")}}))?;
        let attempt = wire.lock().unwrap().attempt.clone();
        let dsh = attempt.agent_id == "dsh-win";
        let project_tools = matches!(attempt.agent_id.as_str(), "dsh-win" | "hermes-win");
        let db = self.store.lock().unwrap().path.clone();
        let servers = if project_tools {
            json!([{"name":"agent_hub","command":std::env::current_exe().map_err(|_|"项目工具程序不可用")?,"args":["--project-tools",db.to_string_lossy(),attempt.id],"env":[]}])
        } else {
            json!([])
        };
        let response = client.rpc("session/new", json!({"cwd":root,"mcpServers":servers}))?;
        let session = response["sessionId"]
            .as_str()
            .ok_or("项目 ACP 会话 ID 缺失")?
            .to_owned();
        {
            let mut state = wire.lock().unwrap();
            state.attempt.native_thread_id = Some(session.clone());
            persist(&self.store, &self.notify, &mut state, true);
        }
        if dsh {
            if let Some(model) = &attempt.model {
                client.rpc(
                    "session/set_config_option",
                    json!({"sessionId":session,"configId":"model","value":model}),
                )?;
            }
            if let Some(effort) = &attempt.reasoning_effort {
                client.rpc(
                    "session/set_config_option",
                    json!({"sessionId":session,"configId":"reasoning_effort","value":effort}),
                )?;
            }
        } else {
            if let Some(model) = attempt.model.as_ref().filter(|model| {
                response["models"]["currentModelId"].as_str() != Some(model.as_str())
            }) {
                client.rpc(
                    "session/set_model",
                    json!({"sessionId":session,"modelId":model}),
                )?;
            }
            client.rpc("session/set_config_option",json!({"sessionId":session,"configId":"reasoning_effort","value":attempt.reasoning_effort.as_deref().unwrap_or("default")}))?;
        }
        if self.cancelled(&attempt.workflow_id) {
            return Err("项目协作已停止".into());
        }
        // Use the workflow's remaining budget, not a shorter per-prompt 10m cap:
        // a high-effort DSH tool turn may legitimately take longer while streaming progress.
        let timeout = deadline.saturating_duration_since(Instant::now());
        if timeout.is_zero() {
            return Err("项目协作超时".into());
        }
        let sent_wire = wire.clone();
        let store = self.store.clone();
        let notify = self.notify.clone();
        let result = match client.rpc_timeout_with_sent(
            "session/prompt",
            json!({"sessionId":session,"prompt":[{"type":"text","text":prompt}]}),
            timeout,
            move || {
                let mut state = sent_wire.lock().unwrap();
                state.attempt.native_turn_id = Some(state.attempt.id.clone());
                state.attempt.status = "running".into();
                persist(&store, &notify, &mut state, true);
            },
        ) {
            Ok(result) => result,
            Err(error) if error.contains("session/prompt 超时") => {
                return Err(
                    "项目成员的推理或工具调用超过当前工作流剩余时间；现有项目记录与文件改动已保留，可直接继续协作。".into(),
                )
            }
            Err(error) => return Err(error),
        };
        if result["stopReason"] != "end_turn" {
            return Err("项目成员未正常完成任务，已保留已有内容".into());
        }
        if provider_error_reply(&wire.lock().unwrap().attempt.output).is_some() {
            return Err("项目成员的模型服务返回额度或频率限制，未完成本阶段".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn wire() -> Wire {
        Wire {
            attempt: Attempt {
                id: "test".into(),
                workflow_id: "workflow".into(),
                task_id: None,
                agent_id: "codex-win".into(),
                stage: "implement".into(),
                status: "running".into(),
                native_thread_id: Some("thread".into()),
                native_turn_id: Some("turn".into()),
                model: None,
                reasoning_effort: None,
                thought: String::new(),
                output: String::new(),
                error: None,
            },
            pieces: vec![],
            completed: None,
            failure: None,
            final_plan: None,
            last_plan_message: None,
            disconnected: false,
            invalid: false,
            last_save: Instant::now(),
        }
    }

    #[test]
    fn project_reasoning_chunks_append_separately_from_visible_output() {
        let mut state = wire();
        state.append_thought("先检查工作区。");
        state.text("acp-message", "我开始处理。", false);
        state.append_thought("再验证恢复路径。");
        assert_eq!(state.attempt.thought, "先检查工作区。再验证恢复路径。");
        assert_eq!(state.attempt.output, "我开始处理。");
    }

    #[test]
    fn acp_provider_quota_text_is_not_treated_as_a_completed_project_turn() {
        assert!(provider_error_reply("HTTP 429: weekly usage limit reached").is_some());
        assert!(provider_error_reply("rate-limited after 3 attempts").is_some());
        assert!(provider_error_reply("验收通过，所有改动符合需求").is_none());
    }

    #[test]
    fn project_native_events_reject_other_thread_and_other_turn() {
        let state = wire();
        assert!(state.accepts(&json!({"threadId":"thread","turnId":"turn"})));
        assert!(state.accepts(&json!({"threadId":"thread","turn":{"id":"turn"}})));
        for params in [
            json!({"threadId":"other","turnId":"turn"}),
            json!({"threadId":"thread","turnId":"other"}),
            json!({"turnId":"turn"}),
            json!({"threadId":"thread"}),
        ] {
            assert!(!state.accepts(&params));
        }
    }
    #[test]
    fn project_native_text_replaces_final_item_and_retains_item_order() {
        let mut state = wire();
        state.text("first", "par", false);
        state.text("first", "tial", false);
        state.text("second", "second", false);
        state.text("first", "final", true);
        assert_eq!(state.attempt.output, "final\n\nsecond");
        state.text("third", &"a".repeat(128_001), true);
        assert!(state.invalid);
    }
    #[test]
    fn native_failure_metadata_never_exposes_upstream_text_or_unknown_variants() {
        let message = "PRIVATE_UPSTREAM_CONFIGURATION";
        assert_eq!(failure_category(&json!({"message":message})), None);
        assert_eq!(
            failure_category(&json!({"codexErrorInfo":"usageLimitExceeded","message":message})),
            Some("额度限制".into())
        );
        assert_eq!(
            failure_category(
                &json!({"codexErrorInfo":{"responseStreamDisconnected":{"httpStatusCode":503,"message":message}},"additionalDetails":message})
            ),
            Some("回复连接中断，HTTP 503".into())
        );
        for info in [
            json!(message),
            json!({message:{"httpStatusCode":401}}),
            json!({"httpConnectionFailed":{"httpStatusCode":99999,"message":message}}),
        ] {
            let category = failure_category(&json!({"codexErrorInfo":info})).unwrap();
            assert!(!category.contains(message));
            assert!(!category.contains("99999"));
        }
    }
    #[test]
    fn codex_plan_uses_final_answer_without_progress_preamble() {
        let mut state = wire();
        state.attempt.stage = "plan".into();
        state.completed_message("progress", "先检查源码", Some("commentary"));
        state.text("final", "{", false);
        state.completed_message(
            "final",
            "{\"summary\":\"方案\",\"tasks\":[]}",
            Some("final_answer"),
        );
        state.finish_plan_output();
        assert_eq!(state.attempt.output, "{\"summary\":\"方案\",\"tasks\":[]}");
        let mut implementation = wire();
        implementation.completed_message("progress", "正在实现", Some("commentary"));
        implementation.completed_message("final", "已实际修改文件", Some("final_answer"));
        implementation.finish_plan_output();
        assert_eq!(implementation.attempt.output, "正在实现\n\n已实际修改文件");
    }
    #[test]
    fn legacy_plan_phase_uses_last_completed_item_and_still_requires_typed_json() {
        let mut state = wire();
        state.attempt.stage = "plan".into();
        state.completed_message("progress", "先分析", None);
        state.completed_message("last", "未返回方案", None);
        state.finish_plan_output();
        assert_eq!(state.attempt.output, "未返回方案");
        assert!(crate::project_store::parse_plan(&state.attempt.output).is_err());
    }
}
