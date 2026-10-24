use crate::models::{codex_models, validate_selection, ModelOption};
use crate::rpc::Client;
use crate::store::{RunRecord, RunToolEvent, Store};
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

const QUOTA_RESUME_PROMPT: &str = "额度已恢复，请接着完成上一条没做完的任务，保留已经完成的工作。";

fn is_quota_stall(category: &str) -> bool {
    category.starts_with("额度限制") || category.starts_with("请求频率限制")
}

/// 是否到了自动续跑的时间：有 `resets_at` 就等它过 1 分钟；读不到时额度按 30 分钟、
/// 频率限制按 5 分钟后再试。到点超过 12 小时的不再续（多半是早就放下的旧任务）。
fn quota_resume_due(error: &str, failed_at_ms: i64, now: i64) -> bool {
    let failed = failed_at_ms / 1000;
    let due = error
        .split("resets_at=")
        .nth(1)
        .and_then(|rest| {
            rest.split(|c: char| !c.is_ascii_digit())
                .next()?
                .parse::<i64>()
                .ok()
        })
        .map(|at| at + 60)
        .unwrap_or(
            failed
                + if error.contains("请求频率限制") {
                    300
                } else {
                    1800
                },
        );
    now >= due && now < due + 12 * 3600
}

#[derive(Clone, Serialize)]
pub struct RunSnapshot {
    #[serde(flatten)]
    pub record: RunRecord,
    pub text: String,
    /// 后端推理块；没有推理的 runtime 留空。
    #[serde(default)]
    pub thought: String,
    #[serde(default)]
    pub tool_events: Vec<RunToolEvent>,
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
    /// 最近一次（优先正在进行的）运行，旧界面与群聊判断沿用它。
    pub active: Option<RunSnapshot>,
    /// 同一成员可同时在多个同席会话里回复；每个会话保留最近一次运行。
    #[serde(default)]
    pub runs: Vec<RunSnapshot>,
    pub models: Vec<ModelOption>,
    pub default_model: Option<String>,
    pub default_effort: Option<String>,
}

/// 把 Codex 的累计用量换成这一回合的，按 ACP 回包的样子返回；base 记回合开始前的累计，回合里第一次报时定下。
fn turn_usage(usage: &Value, base: &mut Option<[i64; 4]>) -> Value {
    const KEYS: [&str; 4] = ["inputTokens", "cachedInputTokens", "cacheWriteInputTokens", "outputTokens"];
    let part = |name: &str| KEYS.map(|key| usage[name][key].as_i64().unwrap_or(0));
    let (total, last) = (part("total"), part("last"));
    let base = *base.get_or_insert(std::array::from_fn(|index| total[index] - last[index]));
    let turn: [i64; 4] = std::array::from_fn(|index| total[index] - base[index]);
    json!({"usage": {"inputTokens": turn[0], "cachedReadTokens": turn[1], "cachedWriteTokens": turn[2], "outputTokens": turn[3]},
           "_meta": {"contextTokens": last[0] + last[3]}})
}

struct ActiveRun {
    snapshot: RunSnapshot,
    items: BTreeMap<String, String>,
    order: Vec<String>,
    last_checkpoint: Instant,
    /// 最后一次收到这一回合的通知；太久没动静就判卡住。
    heard: Instant,
    /// 这一回合开始前整个会话的累计用量（输入、缓存读、缓存写、输出），回合用量 = 现在的累计 - 它。
    usage_base: Option<[i64; 4]>,
}

fn busy(status: &str) -> bool {
    matches!(status, "starting" | "running" | "cancelling")
}

/// 各成员运行表的公共操作：一个成员可以同时在多个会话里回复，按运行 ID 或会话找到对应运行。
pub(crate) trait Run {
    fn record(&self) -> &RunRecord;
}
impl Run for RunSnapshot {
    fn record(&self) -> &RunRecord {
        &self.record
    }
}
impl Run for ActiveRun {
    fn record(&self) -> &RunRecord {
        &self.snapshot.record
    }
}
pub(crate) fn run_busy<T: Run>(run: &T) -> bool {
    busy(&run.record().status)
}
/// 新运行替换同一会话里已结束的旧运行；已结束的运行最多留 16 条，供界面收尾显示。
pub(crate) fn insert_run<T: Run>(runs: &mut Vec<T>, run: T) {
    let conversation = run.record().conversation_id.clone();
    runs.retain(|old| old.record().conversation_id != conversation || run_busy(old));
    runs.push(run);
    while runs.iter().filter(|old| !run_busy(*old)).count() > 16 {
        let Some(index) = runs.iter().position(|old| !run_busy(old)) else {
            break;
        };
        runs.remove(index);
    }
}
pub(crate) fn run_mut<'a, T: Run>(runs: &'a mut [T], id: &str) -> Option<&'a mut T> {
    runs.iter_mut().find(|run| run.record().id == id)
}
pub(crate) fn run_ref<'a, T: Run>(runs: &'a [T], id: &str) -> Option<&'a T> {
    runs.iter().find(|run| run.record().id == id)
}
/// 这个会话正在进行的运行（同一会话同时只会有一条）。
pub(crate) fn live_in<'a, T: Run>(runs: &'a mut [T], conversation: &str) -> Option<&'a mut T> {
    runs.iter_mut()
        .find(|run| run.record().conversation_id == conversation && run_busy(*run))
}
pub(crate) fn busy_in<T: Run>(runs: &[T], conversation: &str) -> bool {
    runs.iter()
        .any(|run| run.record().conversation_id == conversation && run_busy(run))
}
pub(crate) fn busy_ids<T: Run>(runs: &[T]) -> Vec<String> {
    runs.iter()
        .filter(|run| run_busy(*run))
        .map(|run| run.record().id.clone())
        .collect()
}
/// 快照里的 `active`：最近一条正在进行的运行，没有就取最近一条。
pub(crate) fn headline(runs: &[RunSnapshot]) -> Option<RunSnapshot> {
    runs.iter()
        .rev()
        .find(|run| run_busy(*run))
        .or_else(|| runs.last())
        .cloned()
}
pub(crate) fn codex_tool_event(params: &Value, completed: bool) -> Option<RunToolEvent> {
    let item = params.get("item")?;
    let kind = item["type"].as_str()?;
    if matches!(
        kind,
        "agentMessage"
            | "reasoning"
            | "reasoningSummary"
            | "userMessage"
            | "hookPrompt"
            | "plan"
            | "functionCallOutput"
            | "subAgentActivity"
            | "enteredReviewMode"
            | "exitedReviewMode"
            | "contextCompaction"
    ) {
        return None;
    }
    let event_id = item["id"].as_str()?.to_owned();
    let title = match kind {
        "commandExecution" => item["command"].as_str().unwrap_or("运行命令").to_owned(),
        "mcpToolCall" => format!(
            "{} · {}",
            item["server"]
                .as_str()
                .or_else(|| item["serverName"].as_str())
                .unwrap_or("MCP"),
            item["tool"]
                .as_str()
                .or_else(|| item["toolName"].as_str())
                .unwrap_or("工具调用")
        ),
        "fileChange" => "修改文件".to_owned(),
        "dynamicToolCall" => item["tool"]
            .as_str()
            .or_else(|| item["toolName"].as_str())
            .unwrap_or("项目工具调用")
            .to_owned(),
        "webSearch" => format!("网页搜索 · {}", item["query"].as_str().unwrap_or("")),
        _ => format!("Agent 工具 · {kind}"),
    };
    let input_value = item
        .get("arguments")
        .or_else(|| item.get("changes"))
        .or_else(|| item.get("query"));
    let input = input_value
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string())
        })
        .map(|text| text.chars().take(8000).collect());
    let output = [
        "aggregatedOutput",
        "output",
        "result",
        "text",
        "contentItems",
    ]
    .iter()
    .find_map(|key| tool_content_text(&item[*key]))
    .map(|text| text.chars().take(8000).collect());
    let status = if !completed {
        "running"
    } else if item["status"]
        .as_str()
        .is_some_and(|status| status == "failed" || status == "error")
        || item["exitCode"].as_i64().is_some_and(|code| code != 0)
        || item["success"] == false
    {
        "failed"
    } else {
        "completed"
    };
    Some(RunToolEvent {
        event_id,
        kind: kind.into(),
        title,
        input,
        output,
        status: status.into(),
        created_at: crate::store::now(),
    })
}

fn tool_content_text(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_owned());
    }
    let blocks = value
        .as_array()
        .or_else(|| value.get("content")?.as_array())
        .or_else(|| value.get("contentItems")?.as_array())?;
    let text = blocks
        .iter()
        .filter_map(|block| {
            block
                .get("text")
                .and_then(Value::as_str)
                .or_else(|| block.get("content")?.get("text")?.as_str())
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}
pub(crate) fn acp_tool_event(update: &Value) -> Option<RunToolEvent> {
    let update_kind = update["sessionUpdate"].as_str()?;
    if !matches!(update_kind, "tool_call" | "tool_call_update") {
        return None;
    }
    let call = update
        .get("toolCall")
        .or_else(|| update.get("tool_call"))
        .unwrap_or(update);
    let event_id = call["toolCallId"]
        .as_str()
        .or_else(|| call["id"].as_str())?
        .to_owned();
    let kind = call["kind"]
        .as_str()
        .or_else(|| call["name"].as_str())
        .or_else(|| call["toolName"].as_str())
        .unwrap_or("tool")
        .to_owned();
    let title = call["title"]
        .as_str()
        .or_else(|| call["name"].as_str())
        .or_else(|| call["toolName"].as_str())
        .unwrap_or("tool")
        .to_owned();
    let value_text = |value: &Value| {
        value
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| value.to_string())
    };
    let input = ["rawInput", "input", "arguments"]
        .iter()
        .find_map(|key| call.get(*key).filter(|value| !value.is_null()))
        .map(value_text)
        .map(|text| text.chars().take(8000).collect());
    let output = ["rawOutput", "output", "result"]
        .iter()
        .find_map(|key| call.get(*key).filter(|value| !value.is_null()))
        .map(value_text)
        .or_else(|| {
            call.get("content")?
                .as_array()
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter_map(|block| {
                            block
                                .get("content")
                                .unwrap_or(block)
                                .get("text")
                                .and_then(Value::as_str)
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .filter(|text| !text.is_empty())
        })
        .map(|text| text.chars().take(8000).collect());
    let status = match call["status"].as_str().unwrap_or("running") {
        "failed" | "error" => "failed",
        "completed" | "success" => "completed",
        _ => "running",
    };
    Some(RunToolEvent {
        event_id,
        kind,
        title,
        input,
        output,
        status: status.into(),
        created_at: crate::store::now(),
    })
}
fn remember_tool_event(run: &mut ActiveRun, event: RunToolEvent, store: &Arc<Mutex<Store>>) {
    let event = upsert_tool_event(&mut run.snapshot.tool_events, &event);
    crate::store::push_tool(&mut run.snapshot.thought, &run.snapshot.text, &event.event_id);
    let _ = store
        .lock()
        .unwrap()
        .append_tool_event(&run.snapshot.record, &event);
}
pub(crate) fn upsert_tool_event(
    events: &mut Vec<RunToolEvent>,
    event: &RunToolEvent,
) -> RunToolEvent {
    if let Some(existing) = events.iter_mut().find(|old| old.event_id == event.event_id) {
        if event.kind != "tool" {
            existing.kind = event.kind.clone();
        }
        if event.title != "tool" {
            existing.title = event.title.clone();
        }
        if event.input.is_some() {
            existing.input = event.input.clone();
        }
        if event.output.is_some() {
            existing.output = event.output.clone();
        }
        existing.status = event.status.clone();
        existing.clone()
    } else {
        events.push(event.clone());
        event.clone()
    }
}
fn apply_reasoning_delta(run: &mut ActiveRun, method: &str, params: &Value) -> bool {
    if !matches!(
        method,
        "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta"
    ) {
        return false;
    }
    if let Some(delta) = params["delta"].as_str() {
        run.thought(delta);
    }
    true
}

impl ActiveRun {
    /// 推理增量直接累加：思考块不分段、不参与正文拼接。
    fn thought(&mut self, delta: &str) {
        crate::store::push_thought(&mut self.snapshot.thought, &self.snapshot.text, delta);
    }
    fn text(&mut self, item: &str, text: &str, replace: bool) {
        if !self.items.contains_key(item) {
            if !self.order.is_empty() {
                crate::store::push_break(&mut self.snapshot.thought, &self.snapshot.text);
            }
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
    runs: Vec<ActiveRun>,
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
                    // 用户在 Codex 里起的会话名（name）优先，没有再用首条消息预览。
                    title: ["name", "title", "preview"]
                        .iter()
                        .find_map(|key| item[*key].as_str().filter(|text| !text.trim().is_empty()))
                        .map(str::to_string),
                    cwd: item["cwd"].as_str().map(str::to_string),
                    // app-server 给的是 Unix 秒；界面按 ISO 时间解析。
                    updated_at: ["updatedAt", "createdAt"].iter().find_map(|key| {
                        item[*key]
                            .as_str()
                            .map(str::to_string)
                            .or_else(|| item[*key].as_i64().map(crate::quota_reset::iso_utc))
                    }),
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
                runs: Vec::new(),
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

    pub(crate) fn shared_project_client(&self) -> Result<Arc<Client>> {
        let inner = self.inner.lock().unwrap();
        if inner.connection != "connected" {
            return Err("项目成员尚未连接".into());
        }
        inner
            .client
            .clone()
            .ok_or_else(|| "项目成员接口不可用".into())
    }
    pub fn snapshot(&self) -> RuntimeSnapshot {
        let inner = self.inner.lock().unwrap();
        let runs: Vec<RunSnapshot> = inner.runs.iter().map(|run| run.snapshot.clone()).collect();
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
            active: headline(&runs),
            runs,
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
            let request_weak = Arc::downgrade(self);
            client.set_request_handler(Arc::new(move |method, params| match method {
                "item/tool/call" => match request_weak.upgrade() {
                    Some(runtime) => runtime.project_chat_tool(params),
                    None => Some(Err("Agent Hub 已关闭".into())),
                },
                "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                    Some(Ok(json!({"decision":"accept"})))
                }
                "item/tool/requestUserInput" => Some(
                    request_weak
                        .upgrade()
                        .ok_or("同席已关闭".into())
                        .and_then(|runtime| runtime.private_user_input(params)),
                ),
                _ => None,
            }));
            {
                self.inner.lock().unwrap().client = Some(client.clone());
            }
            client.rpc("initialize", json!({"clientInfo":{"name":"personal_agent_hub","title":"同席 Agent Hub","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}}))?;
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
                json!({"ephemeral":true,"approvalPolicy":"never","sandbox":"danger-full-access"}),
            )?;
            let version = native_version(&executable);
            let mut inner = self.inner.lock().unwrap();
            if inner.connection_id != connection_id || inner.connection != "connecting" {
                return Err("Codex 连接已取消".into());
            }
            inner.version = version;
            crate::pricing::remember("codex-win", &models);
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
        if busy_in(&inner.runs, id) {
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
        if busy_in(&inner.runs, conversation_id) {
            return Err("Codex 正在处理这个会话的上一条消息，请等待或停止".into());
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
        insert_run(
            &mut inner.runs,
            ActiveRun {
                snapshot: RunSnapshot {
                    record: record.clone(),
                    text: String::new(),
                    thought: String::new(),
                    tool_events: Vec::new(),
                },
                items: BTreeMap::new(),
                order: Vec::new(),
                last_checkpoint: Instant::now(),
                heard: Instant::now(),
                usage_base: None,
            },
        );
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

    /// 额度重置后自动接着做：私聊里因额度或频率限制停下、之后没人接手的运行，
    /// 到重置时间在原会话里补发一条“继续”。启动时也会把关闭期间到点的补上。
    pub fn watch_quota_resume(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        std::thread::spawn(move || {
            let mut last_connect: Option<Instant> = None;
            loop {
                std::thread::sleep(Duration::from_secs(30));
                let Some(runtime) = weak.upgrade() else {
                    return;
                };
                runtime.resume_after_quota(&mut last_connect);
                runtime.abort_quiet_runs();
            }
        });
    }

    /// 回合连续 QUIET_LIMIT 没有任何通知就判卡住：先试着打断原生回合，再记失败，免得任务一直挂在「在做」。
    fn abort_quiet_runs(&self) {
        let (client, quiet) = {
            let inner = self.inner.lock().unwrap();
            let quiet = inner
                .runs
                .iter()
                .filter(|run| run_busy(*run) && run.heard.elapsed() >= crate::rpc::QUIET_LIMIT)
                .map(|run| {
                    let record = &run.snapshot.record;
                    (record.id.clone(), record.native_thread_id.clone(), record.native_turn_id.clone())
                })
                .collect::<Vec<_>>();
            (inner.client.clone(), quiet)
        };
        for (run_id, thread_id, turn_id) in quiet {
            if let (Some(client), Some(thread_id), Some(turn_id)) = (&client, thread_id, turn_id) {
                let _ = client.rpc("turn/interrupt", json!({"threadId":thread_id,"turnId":turn_id}));
            }
            self.abort(
                &run_id,
                format!("Codex 已 {} 分钟没有任何输出，判定卡住", crate::rpc::QUIET_LIMIT.as_secs() / 60),
            );
        }
    }

    fn resume_after_quota(self: &Arc<Self>, last_connect: &mut Option<Instant>) {
        let Ok(Ok(stalled)) = self
            .store
            .lock()
            .map(|store| store.quota_stalled_runs("codex-win"))
        else {
            return;
        };
        let now = crate::quota_reset::now_secs();
        let due = stalled
            .into_iter()
            .filter(|(_, error, failed_at)| quota_resume_due(error, *failed_at, now))
            .collect::<Vec<_>>();
        if due.is_empty() {
            return;
        }
        let connection = self.inner.lock().unwrap().connection.clone();
        if connection != "connected" {
            // 断开了就自己连一次；连不上隔 10 分钟再试，不反复拉起进程。
            if connection != "disconnected"
                || last_connect.is_some_and(|at| at.elapsed() < Duration::from_secs(600))
            {
                return;
            }
            *last_connect = Some(Instant::now());
            if self.connect().is_err() {
                return;
            }
        }
        for (conversation_id, _, _) in due {
            let _ = self.send(
                &conversation_id,
                &Uuid::new_v4().to_string(),
                QUOTA_RESUME_PROMPT,
            );
        }
    }

    pub(crate) fn send_discussion(
        self: &Arc<Self>,
        discussion: &str,
        round: u32,
    ) -> Result<RunRecord> {
        let mut inner = self.inner.lock().map_err(|_| "运行状态不可用")?;
        if inner.connection != "connected" {
            return Err("Codex 尚未连接".into());
        }
        let client = inner.client.clone().ok_or("Codex 接口不可用")?;
        let (record, text) =
            self.store
                .lock()
                .unwrap()
                .begin_discussion_turn(discussion, "codex-win", round)?;
        insert_run(
            &mut inner.runs,
            ActiveRun {
                snapshot: RunSnapshot {
                    record: record.clone(),
                    text: String::new(),
                    thought: String::new(),
                    tool_events: Vec::new(),
                },
                items: BTreeMap::new(),
                order: Vec::new(),
                last_checkpoint: Instant::now(),
                heard: Instant::now(),
                usage_base: None,
            },
        );
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
            let run = run_ref(&inner.runs, run_id).ok_or("运行已取消")?;
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
        // 同席作为 CLI 外壳，不阻止 Codex 使用本机文件、命令、MCP、插件或网络工具。
        let mut params = json!({"cwd":cwd,"approvalPolicy":"never","sandbox":"danger-full-access"});
        let project_db = self.store.lock().unwrap().path.clone();
        let project_tools =
            crate::project_tools::project_chat_dynamic_tools(&project_db, &conversation_id)?;
        if previous.is_none() && !project_tools.is_empty() {
            params["dynamicTools"] = json!(project_tools);
        }
        let (model, effort) = {
            let inner = self.inner.lock().unwrap();
            let settings = &run_ref(&inner.runs, run_id)
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
        // 有的模型默认不出思考摘要（summary=none），同席就看不到思考；显式要 auto。
        params["config"] = json!({"model_reasoning_effort":effort,"model_reasoning_summary":"auto"});
        if let Some(server) =
            crate::project_tools::project_chat_mcp_server(&project_db, &conversation_id)?
        {
            params["config"]["mcp_servers.tongxi_project"] =
                json!({"command":server["command"],"args":server["args"]});
        }
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
            let run = run_mut(&mut inner.runs, run_id)
                .filter(|run| run_busy(*run))
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
        let input = crate::attachments::prompt(&project_db, &conversation_id, content, true)?;
        let user_message = run_ref(&self.inner.lock().unwrap().runs, run_id)
            .map(|run| run.snapshot.record.user_message_id.clone());
        let turn = client.rpc("turn/start", json!({"threadId":thread_id,"clientUserMessageId":user_message,"input":input,"model":model,"effort":effort,"summary":"auto","approvalPolicy":"never","sandboxPolicy":{"type":"dangerFullAccess"}}))?;
        let turn_id = turn["turn"]["id"]
            .as_str()
            .ok_or("Codex 未返回运行 ID")?
            .to_owned();
        // A loaded thread's resume response can describe the previous turn.
        // Record the acknowledged turn/start overrides for this run instead.
        {
            let mut inner = self.inner.lock().unwrap();
            if let Some(run) = run_mut(&mut inner.runs, run_id) {
                self.store
                    .lock()
                    .unwrap()
                    .record_model(&mut run.snapshot.record, model, effort)?;
            }
        }
        let cancelling = {
            let mut inner = self.inner.lock().unwrap();
            if let Some(run) = run_mut(&mut inner.runs, run_id).filter(|run| run_busy(*run)) {
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

    /// 把会话交给 CLI 前卸下 app-server 里已载入的线程；下次 thread/resume 会从磁盘重读 CLI 写下的内容。
    pub(crate) fn release_native(&self, thread_id: &str) {
        let client = self.inner.lock().unwrap().client.clone();
        if let Some(client) = client {
            let _ = client.rpc("thread/unsubscribe", json!({"threadId":thread_id}));
        }
    }
    pub fn cancel(&self, conversation_id: &str) -> Result<RuntimeSnapshot> {
        self.cancel_expected(conversation_id, None)
    }
    /// 回复进行中插话：用原生 turn/steer 把引导追加进当前回合，不打断。
    pub fn steer(&self, conversation_id: &str, content: &str, message_id: &str) -> Result<()> {
        let (client, thread_id, turn_id) = {
            let inner = self.inner.lock().unwrap();
            let client = inner.client.clone().ok_or("Codex 未连接")?;
            let run = inner
                .runs
                .iter()
                .find(|run| {
                    run.snapshot.record.conversation_id == conversation_id
                        && run_busy(*run)
                        && run.snapshot.record.status != "cancelling"
                })
                .ok_or("这个会话没有正在进行的回复")?;
            (
                client,
                run.snapshot.record.native_thread_id.clone(),
                run.snapshot.record.native_turn_id.clone(),
            )
        };
        let (Some(thread_id), Some(turn_id)) = (thread_id, turn_id) else {
            return Err("回复还没真正开始，请稍后再插话".into());
        };
        let db = self.store.lock().unwrap().path.clone();
        let input = crate::attachments::prompt(&db, conversation_id, content, true)?;
        client.rpc(
            "turn/steer",
            json!({"threadId":thread_id,"expectedTurnId":turn_id,"input":input}),
        )?;
        let mut inner = self.inner.lock().unwrap();
        if let Some(run) = live_in(&mut inner.runs, conversation_id) {
            let snapshot = &mut run.snapshot;
            crate::store::push_steer(&mut snapshot.thought, &snapshot.text, message_id);
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
        let (client, thread_id, turn_id, run_id) = {
            let mut inner = self.inner.lock().unwrap();
            let client = inner.client.clone().ok_or("Codex 未连接")?;
            let run = live_in(&mut inner.runs, conversation_id)
                .filter(|run| expected.is_none_or(|id| run.snapshot.record.id == id))
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

    fn private_user_input(&self, params: &Value) -> Result<Value> {
        let run = {
            let inner = self.inner.lock().unwrap();
            let run = inner
                .runs
                .iter()
                .find(|run| {
                    run.snapshot.record.native_thread_id.as_deref() == params["threadId"].as_str()
                        && run_busy(*run)
                })
                .ok_or("没有正在运行的私聊")?;
            if run
                .snapshot
                .record
                .native_turn_id
                .as_deref()
                .is_some_and(|id| Some(id) != params["turnId"].as_str())
            {
                return Err("私聊问题身份不匹配".into());
            }
            run.snapshot.record.id.clone()
        };
        let db = self.store.lock().unwrap().path.clone();
        crate::chat_questions::ask_native(&db, &run, params)
    }

    fn project_chat_tool(&self, params: &Value) -> Option<Result<Value>> {
        let (conversation_id, thread_id, turn_id) = {
            let inner = self.inner.lock().unwrap();
            let run = inner.runs.iter().find(|run| {
                run.snapshot.record.native_thread_id.as_deref() == params["threadId"].as_str()
            })?;
            let record = &run.snapshot.record;
            if record.native_thread_id.as_deref() != params["threadId"].as_str()
                || record.native_turn_id.as_deref() != params["turnId"].as_str()
            {
                return Some(Err("项目 Agent 工具调用身份不匹配".into()));
            }
            (
                record.conversation_id.clone(),
                record.native_thread_id.clone().unwrap_or_default(),
                record.native_turn_id.clone().unwrap_or_default(),
            )
        };
        let _ = (thread_id, turn_id);
        let db = self.store.lock().unwrap().path.clone();
        let result = crate::project_tools::call_from_project_chat(
            &db,
            &conversation_id,
            params["tool"].as_str().unwrap_or(""),
            params["arguments"].clone(),
        );
        Some(result.map(|value| json!({"contentItems":[{"type":"inputText","text":value.to_string()}],"success":true})))
    }

    fn notification(&self, connection_id: &str, value: Value) {
        let mut inner = self.inner.lock().unwrap();
        if inner.connection_id != connection_id {
            return;
        }
        let params = &value["params"];
        let Some(index) = inner.runs.iter().position(|run| run.accepts(params)) else {
            return;
        };
        let run = &mut inner.runs[index];
        run.heard = Instant::now();
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
                apply_reasoning_delta(run, method, params);
            }
            "item/started" => {
                if let Some(event) = codex_tool_event(params, false) {
                    remember_tool_event(run, event, &self.store);
                }
            }
            "item/completed" => {
                if let Some(event) = codex_tool_event(params, true) {
                    remember_tool_event(run, event, &self.store);
                } else if params["item"]["type"] == "agentMessage" {
                    if let (Some(item), Some(text)) = (
                        params["item"]["id"].as_str(),
                        params["item"]["text"].as_str(),
                    ) {
                        run.text(item, text, true);
                    }
                }
            }
            "item/agentMessage/delta" => {
                if let (Some(item), Some(delta)) =
                    (params["itemId"].as_str(), params["delta"].as_str())
                {
                    run.text(item, delta, false);
                }
            }
            // Codex 的用量按整个会话累计报（total），外加最近一次模型调用（last）。
            "thread/tokenUsage/updated" => {
                let usage = turn_usage(&params["tokenUsage"], &mut run.usage_base);
                self.store.lock().unwrap().record_usage(&run.snapshot.record.id, &usage);
            }
            "turn/completed" => {
                run.snapshot.record.status = match params["turn"]["status"].as_str() {
                    Some("completed") => "completed",
                    Some("interrupted") => "interrupted",
                    _ => "failed",
                }
                .into();
                if run.snapshot.record.status == "failed" {
                    let category =
                        failure_category(&params["turn"]["error"])
                            .unwrap_or_else(|| "原生执行失败".into());
                    run.snapshot.record.error = Some(if is_quota_stall(&category) {
                        format!(
                            "Codex {category}，额度恢复后同席会自动继续；也可调整当前模型后重试"
                        )
                    } else {
                        format!("Codex {category}，已保留会话记录，可调整当前模型后重试")
                    });
                }
                final_event = true;
            }
            _ => return,
        }
        let run = &mut inner.runs[index];
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
        if let Some(run) = run_mut(&mut inner.runs, run_id).filter(|run| run_busy(*run)) {
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
            let Some(run) = run_mut(&mut inner.runs, run_id).filter(|run| run_busy(*run)) else {
                return;
            };
            run.snapshot.record.status = "failed".into();
            run.snapshot.record.error = Some(error.clone());
            let _ = self.store.lock().unwrap().checkpoint(
                &run.snapshot.record,
                &run.snapshot.text,
                &run.snapshot.thought,
            );
            // 其他会话可能还在同一连接上回复：只有没有别的运行时才断开重来。
            if inner.runs.iter().any(|run| run_busy(run)) {
                None
            } else {
                inner.connection_id.clear();
                inner.connection = "disconnected".into();
                inner.error = Some(error);
                inner.client.take()
            }
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
            busy_ids(&inner.runs)
        };
        for id in run_id {
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
            if expected.is_some_and(|id| !run_ref(&inner.runs, id).is_some_and(run_busy)) {
                return;
            }
            inner.connection_id.clear();
            inner.connection = "disconnected".into();
            inner.error = None;
            (inner.client.take(), busy_ids(&inner.runs))
        };
        if let Some(client) = client {
            client.stop();
        }
        for id in run_id {
            self.finish(&id, "interrupted", Some("连接已主动断开".into()));
        }
        self.emit();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn quota_resume_waits_for_the_reset_time_and_skips_stale_runs() {
        let error = "Codex 额度限制（额度预计 10月14日 14:29 重置，resets_at=1792006140），额度恢复后同席会自动继续";
        let failed = 1_791_993_022_941;
        assert!(!super::quota_resume_due(error, failed, 1_792_006_140));
        assert!(super::quota_resume_due(error, failed, 1_792_006_200));
        assert!(!super::quota_resume_due(
            error,
            failed,
            1_792_006_200 + 13 * 3600
        ));
        // 读不到重置时间：额度 30 分钟、频率限制 5 分钟后再试。
        let at = failed / 1000;
        assert!(!super::quota_resume_due(
            "Codex 额度限制，…",
            failed,
            at + 1700
        ));
        assert!(super::quota_resume_due(
            "Codex 额度限制，…",
            failed,
            at + 1800
        ));
        assert!(super::quota_resume_due(
            "Codex 请求频率限制，…",
            failed,
            at + 300
        ));
        assert!(super::is_quota_stall("额度限制（额度预计…）"));
        assert!(!super::is_quota_stall("服务繁忙"));
    }
    #[test]
    fn sparse_other_kind_completion_keeps_the_original_acp_tool_name() {
        let started = super::acp_tool_event(&serde_json::json!({"sessionUpdate":"tool_call","toolCallId":"call","kind":"other","title":"mcp__agent_hub_project__hub_memory_read","rawInput":{"uri":"project-note"}})).unwrap();
        let completed = super::acp_tool_event(&serde_json::json!({"sessionUpdate":"tool_call_update","toolCallId":"call","kind":"other","status":"completed","content":[{"content":{"type":"text","text":"read result"}}]})).unwrap();
        let mut events = vec![started];
        let merged = super::upsert_tool_event(&mut events, &completed);
        assert_eq!(merged.title, "mcp__agent_hub_project__hub_memory_read");
        assert!(merged.input.is_some());
        assert_eq!(merged.output.as_deref(), Some("read result"));
    }
    #[test]
    fn current_mcp_fields_show_the_actual_tool_and_user_events_are_not_tools() {
        let event = super::codex_tool_event(&serde_json::json!({"item":{"type":"mcpToolCall","id":"call","server":"tongxi_project","tool":"hub_memory_write","status":"completed","arguments":{},"result":{"content":[{"type":"text","text":"stored"}]}}}), true).unwrap();
        assert_eq!(event.title, "tongxi_project · hub_memory_write");
        assert_eq!(event.output.as_deref(), Some("stored"));
        for kind in ["userMessage", "plan", "hookPrompt", "functionCallOutput"] {
            assert!(super::codex_tool_event(
                &serde_json::json!({"item":{"type":kind,"id":"metadata"}}),
                true
            )
            .is_none());
        }
    }
    #[test]
    fn sparse_acp_tool_updates_preserve_identity_input_and_capture_content_output() {
        let started = super::acp_tool_event(&serde_json::json!({"sessionUpdate":"tool_call","toolCallId":"one","kind":"execute","title":"python check.py","rawInput":{"command":"python check.py"},"status":"in_progress"})).unwrap();
        let mut events = vec![];
        super::upsert_tool_event(&mut events, &started);
        let finished = super::acp_tool_event(&serde_json::json!({"sessionUpdate":"tool_call_update","toolCallId":"one","status":"completed","content":[{"type":"content","content":{"type":"text","text":"3 assertions passed"}}]})).unwrap();
        let merged = super::upsert_tool_event(&mut events, &finished);
        assert_eq!(events.len(), 1);
        assert_eq!(merged.title, "python check.py");
        assert_eq!(merged.kind, "execute");
        assert!(merged.input.unwrap().contains("python check.py"));
        assert_eq!(merged.output.as_deref(), Some("3 assertions passed"));
        assert_eq!(merged.status, "completed");
        assert_eq!(merged.created_at, started.created_at);
    }
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
                tool_events: Vec::new(),
            },
            items: BTreeMap::new(),
            order: Vec::new(),
            last_checkpoint: Instant::now(),
            heard: Instant::now(),
            usage_base: None,
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
    #[test]
    fn reasoning_and_summary_deltas_are_saved_in_the_codex_thought_block() {
        let mut run = active();
        assert!(!apply_reasoning_delta(
            &mut run,
            "item/agentMessage/delta",
            &json!({"delta":"answer"})
        ));
        assert!(apply_reasoning_delta(
            &mut run,
            "item/reasoning/textDelta",
            &json!({"delta":"先分析。"})
        ));
        assert!(apply_reasoning_delta(
            &mut run,
            "item/reasoning/summaryTextDelta",
            &json!({"delta":"再归纳。"})
        ));
        assert_eq!(run.snapshot.thought, "先分析。再归纳。");
    }
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

#[cfg(test)]
mod turn_usage_tests {
    #[test]
    fn turn_usage_counts_only_this_turn_out_of_the_session_totals() {
        let report = |total: [i64; 4], last: [i64; 4]| {
            let side = |n: [i64; 4]| serde_json::json!({"inputTokens": n[0], "cachedInputTokens": n[1], "cacheWriteInputTokens": n[2], "outputTokens": n[3]});
            serde_json::json!({"total": side(total), "last": side(last)})
        };
        let mut base = None;
        // 会话之前已经用了 50000 输入；这一回合第一次调用 12000 输入（10000 命中缓存）、300 输出。
        let first = super::turn_usage(&report([62000, 40000, 0, 1300], [12000, 10000, 0, 300]), &mut base);
        assert_eq!(first["usage"]["inputTokens"], 12000);
        let second = super::turn_usage(&report([75000, 52000, 0, 1500], [13000, 12000, 0, 200]), &mut base);
        assert_eq!(second["usage"]["inputTokens"], 25000);
        assert_eq!(second["usage"]["cachedReadTokens"], 22000);
        assert_eq!(second["usage"]["outputTokens"], 500);
        assert_eq!(second["_meta"]["contextTokens"], 13200, "上下文取最近一次调用");
        let again = super::turn_usage(&report([75000, 52000, 0, 1500], [13000, 12000, 0, 200]), &mut base);
        assert_eq!(again, second, "同一个数报两遍不重复算");
    }
}
