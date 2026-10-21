//! Project turns reuse each member's native project conversation and connected harness.
use crate::project_store::Attempt;
use crate::project_tools::Broker;
use crate::rpc::Client;
use crate::store::Store;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, String>;
pub type Notify = Arc<dyn Fn(&str) + Send + Sync>;

type ClientProvider = Arc<dyn Fn(&Attempt) -> Result<Arc<Client>> + Send + Sync>;
type SessionReady = Arc<dyn Fn(&Attempt, &str) + Send + Sync>;
type SessionLoaded = Arc<dyn Fn(&Attempt, &str) -> bool + Send + Sync>;

impl Runner {
    /// 用户在成员专属会话里插话：送进这位成员正在跑的项目任务，不打断它。Codex 用 turn/steer、Claude 用 session/steer
    /// 立即送达；Hermes、DSH 没有原生插话，记下来等它这一步说完在同一会话里接着发（返回 false 表示排队）。
    pub fn steer(&self, attempt: &Attempt, text: &str) -> Result<bool> {
        self.steer_with(attempt, &format!("用户插话（发在你的专属会话里，针对你正在做的这项任务）：{text}"), text)
    }
    /// 同席自己插话（如软预算收尾）：原样送达，不加「用户插话」前缀。
    pub fn steer_note(&self, attempt: &Attempt, text: &str) -> Result<bool> {
        self.steer_with(attempt, text, text)
    }
    fn steer_with(&self, attempt: &Attempt, message: &str, text: &str) -> Result<bool> {
        let client = self
            .active
            .lock()
            .unwrap()
            .get(&attempt.id)
            .map(|(_, client)| client.clone())
            .ok_or("项目任务还没真正开始，请稍后再插话")?;
        let thread = attempt
            .native_thread_id
            .as_deref()
            .ok_or("项目任务还没真正开始，请稍后再插话")?;
        let input = json!([{"type":"text","text":message}]);
        match crate::project_store::base_agent(&attempt.agent_id) {
            "codex-win" => {
                let turn = attempt
                    .native_turn_id
                    .as_deref()
                    .ok_or("项目任务还没真正开始，请稍后再插话")?;
                client.rpc("turn/steer", json!({"threadId":thread,"expectedTurnId":turn,"input":input}))?;
            }
            "claude-win" => {
                client.rpc("session/steer", json!({"sessionId":thread,"prompt":input}))?;
            }
            _ => {
                self.queued
                    .lock()
                    .unwrap()
                    .entry(attempt.id.clone())
                    .or_default()
                    .push(text.to_owned());
                return Ok(false);
            }
        }
        Ok(true)
    }
    fn take_queued(&self, attempt: &str) -> Vec<String> {
        self.queued.lock().unwrap().remove(attempt).unwrap_or_default()
    }
}

pub struct Runner {
    clients: ClientProvider,
    session_ready: SessionReady,
    session_loaded: SessionLoaded,
    store: Arc<Mutex<Store>>,
    notify: Notify,
    /// attempt id → (workflow id, 客户端)。每位成员复用自己的连接，
    /// 取消时按任务过滤；不同成员的独立任务不受影响。
    active: Mutex<HashMap<String, (String, Arc<Client>)>>,
    /// attempt id → 用户插话。不支持原生插话的 ACP 成员（Hermes、DSH）：等它这一步说完，在同一会话里接着发给它。
    queued: Mutex<HashMap<String, Vec<String>>>,
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
    /// Codex 报的是线程累计用量；记下本任务开始前的累计，用量取差值。
    usage_base: Option<crate::project_store::Usage>,
}

/// Only protocol enum variants and a bounded HTTP status may reach the UI.
/// Upstream messages/additionalDetails can contain configuration or credentials.
pub(crate) fn failure_category(error: &Value) -> Option<String> {
    let info = &error["codexErrorInfo"];
    if info.is_null() {
        return None;
    }
    let name = if let Some(name) = info.as_str() {
        let label = match name {
            "contextWindowExceeded" => "上下文超限",
            "sessionBudgetExceeded" | "usageLimitExceeded" | "rateLimitExceeded" => {
                let label = if name == "rateLimitExceeded" {
                    "请求频率限制"
                } else {
                    "额度限制"
                };
                // 只从原文里取重置时间，原文本身不外传。
                let (offset, now) = crate::quota_reset::local_offset();
                let reset = crate::quota_reset::reset_at(&error.to_string(), now, offset)
                    .map(crate::quota_reset::marker)
                    .unwrap_or_default();
                return Some(format!("{label}{reset}"));
            }
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
    // 免费档：额度小得一项任务都撑不住，等重置也没用，不按「额度」报（否则会原地等），让调度换人或暂停。
    if output.chars().count() < 4000 && ["free_tier", "free tier"].iter().any(|marker| output.contains(marker)) {
        return Some("项目成员走的是免费档线路，用量很快到上限，不适合做项目任务，未完成本阶段");
    }
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
    .or_else(|| {
        // Hermes 把服务商的报错当成回复交回来（回合照常结束）。只认短回复里服务商原话的写法，
        // 免得交付摘要里正常提到 401、余额的被误判。
        (output.chars().count() < 800
            && [
                "provider said: http 4",
                "rejected your api key",
                "invalid api key",
                "insufficient balance",
                "insufficient_quota",
                "payment required",
            ]
            .iter()
            .any(|marker| output.contains(marker)))
        .then_some("项目成员的模型服务拒绝请求（Key 无效或余额不足），这条线路暂时不能用，未完成本阶段")
    })
}

impl Wire {
    fn completed_message(&mut self, id: &str, text: &str, phase: Option<&str>) {
        self.text(id, text, true);
        if matches!(self.attempt.stage.as_str(), "plan" | "repair")
            && crate::project_store::base_agent(&self.attempt.agent_id) == "codex-win"
        {
            self.last_plan_message = Some(text.into());
            if phase == Some("final_answer") {
                self.final_plan = Some(text.into());
            }
        }
    }
    fn finish_plan_output(&mut self) {
        if !matches!(self.attempt.stage.as_str(), "plan" | "repair")
            || crate::project_store::base_agent(&self.attempt.agent_id) != "codex-win"
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
        // 思考记录到上限就不再往后记（只是过程展示），不影响这次运行的结果。
        // 原来判成整次运行无效：长任务做完、推送了也被记成失败。
        if self.attempt.thought.len().saturating_add(text.len()) > 128_000 {
            return;
        }
        crate::store::push_thought(&mut self.attempt.thought, &self.attempt.output, text);
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

fn usage_json_last(params: &Value) -> Value {
    params["tokenUsage"]["last"].clone()
}

/// Codex 最后一次模型调用的 token 数＝当前会话上下文的大小。
fn usage_context(last: &Value) -> u64 {
    last["totalTokens"].as_u64().unwrap_or(0)
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

impl Runner {
    pub fn new(
        clients: ClientProvider,
        session_ready: SessionReady,
        session_loaded: SessionLoaded,
        store: Arc<Mutex<Store>>,
        notify: Notify,
    ) -> Arc<Self> {
        Arc::new(Self {
            clients,
            session_ready,
            session_loaded,
            store,
            notify,
            active: Mutex::new(HashMap::new()),
            queued: Mutex::new(HashMap::new()),
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
    fn attempt_cancelled(&self, workflow: &str, attempt: &str) -> bool {
        if self.cancelled(workflow) {
            return true;
        }
        match self.store.lock().unwrap().attempt(attempt) {
            Ok(state) => matches!(state.status.as_str(), "cancelling" | "interrupted"),
            Err(_) => true,
        }
    }
    fn wire_cancelled(&self, wire: &Arc<Mutex<Wire>>) -> bool {
        let state = wire.lock().unwrap();
        let (workflow, attempt) = (state.attempt.workflow_id.clone(), state.attempt.id.clone());
        drop(state);
        self.attempt_cancelled(&workflow, &attempt)
    }
    fn interrupt(&self, attempt_id: &str, client: Arc<Client>) {
        let attempt = self.store.lock().unwrap().attempt(attempt_id);
        if let Ok(attempt) = attempt {
            if let Some(session) = attempt.native_thread_id {
                std::thread::spawn(move || {
                    if crate::project_store::base_agent(&attempt.agent_id) == "codex-win" {
                        if let Some(turn) = attempt.native_turn_id {
                            let _ = client
                                .rpc("turn/interrupt", json!({"threadId":session,"turnId":turn}));
                        }
                    } else {
                        let _ = client.write(
                            &json!({"method":"session/cancel","params":{"sessionId":session}}),
                        );
                    }
                });
            }
        }
    }
    pub fn cancel(&self, workflow: &str) {
        let _=self.store.lock().unwrap().connection.execute("UPDATE project_questions SET status='cancelled',answered_at=?1 WHERE workflow_id=?2 AND status='pending'",rusqlite::params![crate::store::now(),workflow]);
        let active = self
            .active
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, (owner, _))| owner == workflow)
            .map(|(id, (_, client))| (id.clone(), client.clone()))
            .collect::<Vec<_>>();
        for (attempt, client) in active {
            self.interrupt(&attempt, client);
        }
    }
    pub fn cancel_attempt(&self, attempt: &str) {
        let _=self.store.lock().unwrap().connection.execute("UPDATE project_questions SET status='cancelled',answered_at=?1 WHERE attempt_id=?2 AND status='pending'",rusqlite::params![crate::store::now(),attempt]);
        let client = self
            .active
            .lock()
            .unwrap()
            .get(attempt)
            .map(|(_, client)| client.clone());
        if let Some(client) = client {
            self.interrupt(attempt, client);
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
            usage_base: None,
        }));
        let agent = wire.lock().unwrap().attempt.agent_id.clone();
        let client = (self.clients)(&wire.lock().unwrap().attempt)?;
        let previous_handler = client.swap_request_handler(None);
        let notifications = wire.clone();
        let store = self.store.clone();
        let notify = self.notify.clone();
        let is_codex = crate::project_store::base_agent(&agent) == "codex-win";
        let manager = wire.lock().unwrap().attempt.activity.as_deref()
            == Some(crate::coordinator_policy::MANAGER_ACTIVITY);
        let subscription = client.listen(Arc::new(move |value| {
            let mut state = notifications.lock().unwrap();
            if value["method"] == "agent_hub/transport_closed" {
                state.disconnected = true;
                return;
            }
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
                    "item/started" => {
                        if let Some(event) = crate::codex::codex_tool_event(params, false) {
                            if manager {
                                state.invalid = true;
                                return;
                            }
                            crate::codex::upsert_tool_event(&mut state.attempt.tool_events, &event);
                            force = true;
                        } else {
                            return;
                        }
                    }
                    "item/completed" => {
                        if let Some(event) = crate::codex::codex_tool_event(params, true) {
                            if manager {
                                state.invalid = true;
                                return;
                            }
                            crate::codex::upsert_tool_event(&mut state.attempt.tool_events, &event);
                        } else if params["item"]["type"] == "agentMessage" {
                            if let (Some(id), Some(text)) = (
                                params["item"]["id"].as_str(),
                                params["item"]["text"].as_str(),
                            ) {
                                state.completed_message(id, text, params["item"]["phase"].as_str());
                            }
                        } else {
                            return;
                        }
                        force = true;
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
                    "thread/tokenUsage/updated" => {
                        let usage = &params["tokenUsage"];
                        let Some(total) = crate::project_store::Usage::from_codex(&usage["total"]) else {
                            return;
                        };
                        if state.usage_base.is_none() {
                            // 第一条通知的 last 已经是本任务的第一次模型调用，起点要把它扣掉。
                            let last = crate::project_store::Usage::from_codex(&usage["last"]).unwrap_or_default();
                            state.usage_base = Some(total.since(&last));
                        }
                        let mut usage = total.since(state.usage_base.as_ref().unwrap());
                        usage.context = usage_context(&usage_json_last(params));
                        state.attempt.usage = Some(usage);
                    }
                    _ => return,
                }
            } else {
                if state.attempt.native_thread_id.as_deref() != params["sessionId"].as_str()
                    || params["sessionId"].is_null()
                {
                    return;
                }
                if value["method"] != "session/update" {
                    return;
                }
                let update = &params["update"];
                if update["sessionUpdate"] == "usage_update" {
                    // DSH 只报上下文占用：记成近似值；已有回合结束时的精确用量就不覆盖。
                    let used = update["used"].as_u64().unwrap_or(0);
                    if used > 0 && state.attempt.usage.as_ref().is_none_or(|usage| usage.context_only) {
                        state.attempt.usage = Some(crate::project_store::Usage {
                            // Claude 桥接另报本回合累计（spent），软预算按它看；DSH 只有上下文占用。
                            input: update["spent"].as_u64().unwrap_or(used),
                            context: used,
                            cost_usd: update["cost"]["amount"].as_f64().filter(|_| update["cost"]["currency"] == "USD"),
                            // 占位会被回合结束的精确用量替换、继承这里的口径：Claude 的是 API 等值，不能标成服务商额度。
                            cost_kind: (crate::project_store::base_agent(&state.attempt.agent_id) != "claude-win")
                                .then(|| "provider".into()),
                            context_only: true,
                            ..Default::default()
                        });
                    } else {
                        return;
                    }
                } else if let Some(event) = crate::codex::acp_tool_event(update) {
                    crate::codex::upsert_tool_event(&mut state.attempt.tool_events, &event);
                    force = true;
                } else {
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
            }
            persist(&store, &notify, &mut state, force);
        }));
        let (workflow, attempt_id) = {
            let state = wire.lock().unwrap();
            (state.attempt.workflow_id.clone(), state.attempt.id.clone())
        };
        self.active
            .lock()
            .unwrap()
            .insert(attempt_id.clone(), (workflow.clone(), client.clone()));
        let mut next_prompt = prompt.to_owned();
        let mut handled = std::collections::HashSet::new();
        let mut handled_updates = serde_json::from_str::<Value>(prompt)
            .ok()
            .and_then(|value| value["user_updates"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|update| update["id"].as_str().map(String::from))
            .collect::<std::collections::HashSet<_>>();
        let outcome = loop {
            if self.attempt_cancelled(&workflow, &attempt_id) {
                break Err("项目协作已停止".into());
            }
            {
                let mut state = wire.lock().unwrap();
                state.completed = None;
                state.failure = None;
                state.attempt.native_turn_id = None;
                if (!handled.is_empty() || !handled_updates.is_empty())
                    && matches!(state.attempt.stage.as_str(), "plan" | "repair" | "review")
                {
                    state.pieces.clear();
                    state.attempt.output.clear();
                    state.final_plan = None;
                    state.last_plan_message = None;
                }
            }
            let effective = deadline + self.store.lock().unwrap().project_wait_duration(&workflow);
            let result = if is_codex {
                self.codex_turn(&client, &wire, root, &next_prompt, deadline)
            } else {
                self.acp_turn(&client, &wire, root, &next_prompt, effective)
            };
            if result.is_err() {
                break result;
            }
            let questions = {
                let store = self.store.lock().unwrap();
                store
                    .workflow(&workflow)
                    .and_then(|job| store.project_questions(&job.conversation_id))
            };
            let questions = match questions {
                Ok(questions) => questions
                    .into_iter()
                    .filter(|question| {
                        question.workflow_id == workflow
                            && question.attempt_id.as_deref() == Some(attempt_id.as_str())
                            && question.delivery != "native"
                            && !handled.contains(&question.id)
                    })
                    .collect::<Vec<_>>(),
                Err(error) => break Err(error),
            };
            if questions.is_empty() {
                let continuation = (|| -> Result<Option<String>> {
                    let planning = {
                        let state = wire.lock().unwrap();
                        state.attempt.stage == "plan"
                            && state.attempt.activity.as_deref()
                                != Some(crate::coordinator_policy::MANAGER_ACTIVITY)
                            && !state
                                .attempt
                                .activity
                                .as_deref()
                                .is_some_and(|value| value.starts_with("task:"))
                            && state.attempt.activity.as_deref() != Some("discussion")
                    };
                    if !planning {
                        return Ok(None);
                    }
                    let store = self.store.lock().unwrap();
                    let updates = store
                        .project_user_updates(&workflow)?
                        .into_iter()
                        .filter(|update| !handled_updates.contains(&update.id))
                        .collect::<Vec<_>>();
                    if updates.is_empty() {
                        return Ok(None);
                    }
                    let roles = crate::project_store::roles_of(&store.workflow(&workflow)?)?;
                    handled_updates.extend(updates.iter().map(|update| update.id.clone()));
                    Ok(Some(format!("用户在本轮规划期间补充了以下要求：{}\n当前分工：{}\n请复用已有方案与检查证据，按补充要求修正当前方案与任务分配；不要重新开始一轮协作，不要重复已完成的摸底。需求仍不清楚时 hub_ask_user 提问并等待。最终方案使用既定 JSON 格式。",json!(updates),json!(roles))))
                })();
                match continuation {
                    Ok(Some(prompt)) => {
                        next_prompt = prompt;
                        continue;
                    }
                    Ok(None) => break Ok(()),
                    Err(error) => break Err(error),
                }
            }
            let mut answers = Vec::new();
            for question in questions {
                loop {
                    if self.attempt_cancelled(&workflow, &attempt_id) {
                        break;
                    }
                    let current = self
                        .store
                        .lock()
                        .unwrap()
                        .project_questions(&question.conversation_id)
                        .ok()
                        .and_then(|questions| {
                            questions.into_iter().find(|row| row.id == question.id)
                        });
                    if let Some(current) = current {
                        if current.status == "answered" {
                            answers
                                .push(json!({"question":current.prompt,"answer":current.answer}));
                            handled.insert(current.id);
                            break;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            if self.attempt_cancelled(&workflow, &attempt_id) {
                break Err("项目协作已停止".into());
            }
            let continuation = (|| -> Result<String> {
                let store = self.store.lock().unwrap();
                let updates = store.project_user_updates(&workflow)?;
                let roles = crate::project_store::roles_of(&store.workflow(&workflow)?)?;
                handled_updates.extend(updates.iter().map(|update| update.id.clone()));
                Ok(format!("{}\n用户已回答：{}\n本轮最新补充要求：{}\n当前分工：{}\n从停下的位置继续同一项目目标，不重做已完成工具操作。",prompt,json!(answers),json!(updates),json!(roles)))
            })();
            match continuation {
                Ok(prompt) => next_prompt = prompt,
                Err(error) => break Err(error),
            }
        };
        client.unlisten(subscription);
        client.swap_request_handler(previous_handler);
        self.active.lock().unwrap().remove(&attempt_id);
        let mut result = wire.lock().unwrap().attempt.clone();
        if self.attempt_cancelled(&workflow, &attempt_id) {
            result.status = "interrupted".into();
            result.error = Some(if self.cancelled(&workflow) {
                "项目协作已停止，已保留现有文件和回复".into()
            } else {
                "用户单独中断了此 Agent；依赖此任务的后续工作会等待恢复".into()
            });
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
        let db = self.store.lock().unwrap().path.clone();
        let attempt = wire.lock().unwrap().attempt.clone();
        let manager =
            attempt.activity.as_deref() == Some(crate::coordinator_policy::MANAGER_ACTIVITY);
        let broker = if manager {
            None
        } else {
            Some(Arc::new(Mutex::new(Broker::open(&db, &attempt.id)?)))
        };
        let binding = wire.clone();
        let db_for_question = db.clone();
        let attempt_for_question = attempt.id.clone();
        client.set_request_handler(Arc::new(move |method, params| {
            if manager {
                return Some(if method.contains("requestApproval") {
                    Ok(json!({"decision":"decline"}))
                } else {
                    Err("管家仅调度，不能调用项目或原生工具".into())
                });
            }
            if method != "item/tool/call" {
                return Some(Ok(match method {
                    "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                        json!({"decision":"decline"})
                    }
                    "item/tool/requestUserInput" => {
                        return Some(crate::project_questions::ask_native(
                            &db_for_question,
                            &attempt_for_question,
                            params,
                        ))
                    }
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
                broker.as_ref().unwrap().lock().unwrap().call(
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
        let (chat, previous, project_root) = self.project_session(&attempt)?;
        let mut params = json!({"cwd":project_root,"approvalPolicy":"never","sandbox":"danger-full-access","model":attempt.model,"config":{"model_reasoning_effort":attempt.reasoning_effort,"model_reasoning_summary":"auto"},"developerInstructions":"你是该项目的协作成员。同一项目的用户私聊、团队任务和历史共用此会话。按当前消息决定职责；仅在当前请求明确要求结构化计划或验收时返回对应JSON，正常交流自然回答。模型与提供商使用项目配置。"});
        if manager {
            params["sandbox"] = json!("read-only");
            params["approvalPolicy"] = json!("untrusted");
            params["ephemeral"] = json!(true);
            params["developerInstructions"] = json!(crate::coordinator_policy::MANAGER_INSTRUCTION);
            params["dynamicTools"] = json!([]);
        }
        let loaded = previous
            .as_ref()
            .is_some_and(|session| manager && (self.session_loaded)(&attempt, session));
        if let Some(thread) = previous.clone().filter(|_| !manager) {
            params["threadId"] = json!(thread);
            params["excludeTurns"] = json!(true);
        } else if !manager {
            params["dynamicTools"] = json!(crate::project_tools::project_chat_dynamic_tools(
                &db, &chat
            )?);
        }
        let thread = if loaded {
            json!({"thread":{"id":previous}})
        } else {
            client.rpc(
                if params.get("threadId").is_some() {
                    "thread/resume"
                } else {
                    "thread/start"
                },
                params,
            )?
        };
        let thread_id = thread["thread"]["id"]
            .as_str()
            .ok_or("Codex 项目会话 ID 缺失")?
            .to_owned();
        self.bind_project_session(&chat, &attempt, &thread_id, &project_root)?;
        if manager {
            (self.session_ready)(&attempt, &thread_id);
        }
        {
            let mut state = wire.lock().unwrap();
            state.attempt.native_thread_id = Some(thread_id.clone());
            persist(&self.store, &self.notify, &mut state, true);
        }
        if self.wire_cancelled(wire) {
            return Err("项目协作已停止".into());
        }
        // Keep this shape compatible with the installed app-server protocol. Tools
        // remain disabled in the manager process and rejected by the request handler.
        let sandbox = if manager {
            json!({"type":"readOnly","networkAccess":false})
        } else {
            json!({"type":"dangerFullAccess"})
        };
        let turn=client.rpc("turn/start",json!({"threadId":thread_id,"input":[{"type":"text","text":prompt}],"cwd":root,"model":attempt.model,"effort":attempt.reasoning_effort,"summary":"auto","approvalPolicy":if manager{"untrusted"}else{"never"},"sandboxPolicy":sandbox}))?;
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
        let mut cancelling_since = None;
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
            if self.wire_cancelled(wire) {
                let since = cancelling_since.get_or_insert_with(Instant::now);
                if since.elapsed() > Duration::from_secs(15) {
                    client.stop();
                    return Err("原生停止未确认，已终止连接并保留项目会话".into());
                }
            }
            let workflow = wire.lock().unwrap().attempt.workflow_id.clone();
            if Instant::now()
                >= deadline + self.store.lock().unwrap().project_wait_duration(&workflow)
            {
                client.stop();
                return Err("项目协作超时，已停止当前原生任务".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn project_session(&self, attempt: &Attempt) -> Result<(String, Option<String>, PathBuf)> {
        let mut store = self.store.lock().unwrap();
        let workflow = store.workflow(&attempt.workflow_id)?;
        let project = store.project(&workflow.project_id)?;
        if attempt.activity.as_deref() == Some(crate::coordinator_policy::MANAGER_ACTIVITY) {
            let cwd = crate::coordinator_clients::manager_directory(
                &store.path,
                &workflow.id,
                &attempt.agent_id,
            )?;
            std::fs::create_dir_all(&cwd).map_err(|_| "管家目录创建失败")?;
            // 调度每回合都拿到完整状态（账本、原话、任务、成员结果），续接旧会话只会把每回合的输入
            // 越叠越长、缓存过期后整段重读，所以每回合新开会话；最近一回合的会话记在本次协作上。
            return Ok((workflow.id.clone(), None, cwd));
        }
        let chats = store.ensure_project_agent_chats(&workflow.conversation_id)?;
        let chat = chats
            .iter()
            .find(|chat| chat.agent_id == attempt.agent_id)
            .ok_or("项目成员会话不存在")?;
        let session = store
            .detail(&chat.conversation_id)?
            .sessions
            .into_iter()
            .find(|session| session.agent_id == crate::project_store::base_agent(&attempt.agent_id))
            .ok_or("项目成员原生会话不存在")?;
        // 调度让这项从零上下文开始：新开原生会话，之后这个位置就续接新会话。
        let native = session.native_session_id.filter(|_| !store.clears_context(attempt));
        Ok((
            chat.conversation_id.clone(),
            native,
            PathBuf::from(project.root),
        ))
    }
    fn bind_project_session(
        &self,
        chat: &str,
        attempt: &Attempt,
        session: &str,
        root: &Path,
    ) -> Result<()> {
        if attempt.activity.as_deref() == Some(crate::coordinator_policy::MANAGER_ACTIVITY) {
            return self.store.lock().unwrap().bind_manager_session(
                &attempt.workflow_id,
                &attempt.agent_id,
                session,
                root,
            );
        }
        self.store.lock().unwrap().connection.execute("UPDATE sessions SET native_session_id=?1,native_cwd=?2 WHERE conversation_id=?3 AND agent_id=?4",rusqlite::params![session,root.to_string_lossy(),chat,crate::project_store::base_agent(&attempt.agent_id)]).map_err(|_|"项目会话绑定失败")?;
        Ok(())
    }

    fn acp_turn(
        &self,
        client: &Arc<Client>,
        wire: &Arc<Mutex<Wire>>,
        root: &Path,
        prompt: &str,
        deadline: Instant,
    ) -> Result<()> {
        let attempt = wire.lock().unwrap().attempt.clone();
        // DSH 与 Claude 的桥都以 session/resume 续接，并用 config option 切换模型与强度。
        let base = crate::project_store::base_agent(&attempt.agent_id);
        let dsh = matches!(base, "dsh-win" | "claude-win");
        let manager =
            attempt.activity.as_deref() == Some(crate::coordinator_policy::MANAGER_ACTIVITY);
        let db = self.store.lock().unwrap().path.clone();
        let (chat, previous, project_root) = self.project_session(&attempt)?;
        let servers = if manager {
            json!([])
        } else {
            crate::project_tools::project_chat_mcp_server(&db, &chat)?
                .map_or_else(|| json!([]), |server| json!([server]))
        };
        // Claude 每轮新起进程、会话按 ID 续接，可以直接在本轮工作目录（任务工作树）里运行，
        // 文件自然落在工作树而不是项目根目录；DSH/Hermes 的会话绑定目录，仍用项目根目录。
        let session_cwd = if base == "claude-win" && !manager {
            root.to_path_buf()
        } else {
            project_root.clone()
        };
        let response = if let Some(session) = &previous {
            if (dsh || manager)
                && (self.session_loaded)(&attempt, session)
                && base != "claude-win"
            {
                Value::Null
            } else {
                client.rpc(
                    if dsh {
                        "session/resume"
                    } else {
                        "session/load"
                    },
                    json!({"sessionId":session,"cwd":session_cwd,"mcpServers":servers}),
                )?
            }
        } else {
            client.rpc(
                "session/new",
                json!({"cwd":session_cwd,"mcpServers":servers}),
            )?
        };
        let session = previous
            .or_else(|| response["sessionId"].as_str().map(String::from))
            .ok_or("项目 ACP 会话 ID 缺失")?;
        self.bind_project_session(&chat, &attempt, &session, &project_root)?;
        (self.session_ready)(&attempt, &session);
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
            } else if base == "claude-win" {
                // 未指定强度＝回到 CLI 默认，不沿用同一会话上一轮的显式强度。
                client.rpc(
                    "session/set_config_option",
                    json!({"sessionId":session,"configId":"reasoning_effort","value":""}),
                )?;
            }
        } else {
            if let Some(model) = attempt.model.as_ref() {
                client.rpc(
                    "session/set_model",
                    json!({"sessionId":session,"modelId":model}),
                )?;
            }
            client.rpc("session/set_config_option",json!({"sessionId":session,"configId":"reasoning_effort","value":attempt.reasoning_effort.as_deref().unwrap_or("default")}))?;
        }
        if self.wire_cancelled(wire) {
            return Err("项目协作已停止".into());
        }
        // Use the workflow's remaining budget, not a shorter per-prompt 10m cap:
        // a high-effort DSH tool turn may legitimately take longer while streaming progress.
        let timeout = deadline.saturating_duration_since(Instant::now());
        if timeout.is_zero() {
            return Err("项目协作超时".into());
        }
        let mut work_prompt = if manager {
            prompt.to_owned()
        } else {
            format!("本轮工作目录：{}。所有实现、修改和测试只在此目录；项目原生会话保持同一会话，目录可能随任务切换。\n{}",root.display(),prompt)
        };
        // 用户插话排着队时，这一步说完后在同一会话里接着发给它，直到没有新的插话。
        loop {
        let timeout = deadline.saturating_duration_since(Instant::now());
        if timeout.is_zero() {
            return Err("项目协作超时".into());
        }
        let sent_wire = wire.clone();
        let store = self.store.clone();
        let notify = self.notify.clone();
        let prompt_started = crate::store::now();
        let result = match client.rpc_timeout_with_sent(
            "session/prompt",
            json!({"sessionId":session,"prompt":[{"type":"text","text":work_prompt}]}),
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
                client.stop();
                return Err(
                    "项目成员的推理或工具调用超过当前工作流剩余时间；现有项目记录与文件改动已保留，可直接继续协作。".into(),
                );
            }
            Err(error) => return Err(error),
        };
        // DSH 的回包不带用量：从它自己的会话日志里把这一轮的模型调用加起来。
        let usage = crate::project_store::Usage::from_acp(&result).or_else(|| {
            (base == "dsh-win")
                .then(|| crate::dsh::session_usage(&session, prompt_started))
                .flatten()
        });
        if let Some(usage) = usage {
            let mut state = wire.lock().unwrap();
            state.attempt.usage = Some(match state.attempt.usage.take() {
                // 之前只有上下文占位（DSH usage_update）：精确用量到了就替换，不累加；占位带的服务商费用留着。
                Some(before) if before.context_only => crate::project_store::Usage {
                    cost_usd: usage.cost_usd.or(before.cost_usd),
                    cost_kind: usage.cost_kind.clone().or(before.cost_kind),
                    ..usage
                },
                // 插话后续的那几步用量累加；上下文取最后一步的。
                Some(before) => crate::project_store::Usage {
                    input: before.input + usage.input,
                    cached: before.cached + usage.cached,
                    cache_write: before.cache_write + usage.cache_write,
                    output: before.output + usage.output,
                    reasoning: before.reasoning + usage.reasoning,
                    cost_usd: match (before.cost_usd, usage.cost_usd) {
                        (Some(a), Some(b)) => Some(a + b),
                        (a, b) => a.or(b),
                    },
                    ..usage
                },
                None => usage,
            });
            persist(&self.store, &self.notify, &mut state, true);
        }
        if result["stopReason"] != "end_turn" {
            return Err("项目成员未正常完成任务，已保留已有内容".into());
        }
        let steers = self.take_queued(&attempt.id);
        if steers.is_empty() {
            break;
        }
        work_prompt = format!(
            "用户在你做这项任务时插话（发在你的专属会话里）：
{}
按这些话调整后继续把当前任务做完，最后照原要求给出交付摘要。",
            steers.join("
")
        );
        }
        if let Some(error) = provider_error_reply(&wire.lock().unwrap().attempt.output) {
            return Err(error.into());
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
                created_at: 1,
                activity: None,
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
                tool_events: Vec::new(),
                output: String::new(),
                error: None,
                usage: None,
            },
            pieces: vec![],
            completed: None,
            failure: None,
            final_plan: None,
            last_plan_message: None,
            disconnected: false,
            invalid: false,
            last_save: Instant::now(),
            usage_base: None,
        }
    }

    #[test]
    fn project_reasoning_chunks_append_separately_from_visible_output() {
        let mut state = wire();
        state.append_thought("先检查工作区。");
        state.text("acp-message", "我开始处理。", false);
        state.append_thought("再验证恢复路径。");
        assert_eq!(
            state.attempt.thought,
            "先检查工作区。\u{1e}6\u{1f}再验证恢复路径。"
        );
        assert_eq!(state.attempt.output, "我开始处理。");
    }

    #[test]
    fn acp_provider_quota_text_is_not_treated_as_a_completed_project_turn() {
        assert!(provider_error_reply("HTTP 429: weekly usage limit reached").is_some());
        assert!(provider_error_reply("rate-limited after 3 attempts").is_some());
        assert!(provider_error_reply("验收通过，所有改动符合需求").is_none());
        let rejected = "DeepSeek rejected your API key, so the model can't be reached. Update it in Settings → Providers, or run `hermes setup` in a terminal.

Provider said: HTTP 401: Invalid 'Authorization' header or token.";
        assert!(provider_error_reply(rejected).is_some_and(|error| error.contains("Key 无效")));
        let free = "Quota exceeded for metric: generativelanguage.googleapis.com/generate_content_free_tier_input_token_count, limit: 250000
Your Google API key is on the free tier";
        let error = provider_error_reply(free).unwrap();
        assert!(error.contains("免费档") && !crate::coordinator_policy::quota_exhausted(error));
        assert!(crate::capability::environmental("failed", Some(error)));
        // 交付摘要里正常提到 401 不算。
        assert!(provider_error_reply("K12 已上线：未带 token 返回 401，并带 WWW-Authenticate: Bearer").is_none());
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
    fn long_thought_stops_recording_without_failing_the_run() {
        let mut state = wire();
        state.append_thought(&"a".repeat(127_990));
        let before = state.attempt.thought.len();
        state.append_thought("再想一点");
        assert_eq!(state.attempt.thought.len(), before);
        assert!(!state.invalid);
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
