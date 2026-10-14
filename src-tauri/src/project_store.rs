//! Project registration, persistent tasks and leases shared by all writers.
use crate::store::{now, Store};
use rusqlite::{params, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;

type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Debug, Serialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub root: String,
    pub summary_enabled: bool,
    pub created_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionChoice {
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub rationale: String,
}

/// 一个角色选定的成员与可选参数；model/effort 为 None 表示由该成员自动选型。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RoleChoice {
    pub agent: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
}

/// 这次工作流的角色：规划 / 所有实现任务与修复 / 验收。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Roles {
    pub plan: RoleChoice,
    pub implement: RoleChoice,
    pub review: RoleChoice,
}

/// 可参与项目的 Windows 成员；阿尔比恩（WSL Pi）不加入项目工作流。
pub const EXECUTOR_AGENTS: [&str; 4] = ["codex-win", "hermes-win", "dsh-win", "claude-win"];
/// project_tasks.agent_id 的约束只允许 Codex/DSH/Claude（v18 起含 Claude），Hermes 用 assigned_agent 列存放。
const TASK_ROW_BASE_AGENTS: [&str; 3] = ["codex-win", "dsh-win", "claude-win"];
pub const REVIEWER_AGENTS: [&str; 4] = EXECUTOR_AGENTS;

impl Roles {
    /// 缺省组合＝旧行为：Codex 规划、DSH 执行、Hermes 验收。
    pub fn defaults() -> Self {
        let choice = |agent: &str| RoleChoice {
            agent: agent.into(),
            model: None,
            effort: None,
        };
        Self {
            plan: choice("codex-win"),
            implement: choice("dsh-win"),
            review: choice("hermes-win"),
        }
    }

    pub fn parse(raw: Option<&str>) -> Result<Self> {
        let Some(text) = raw.filter(|text| !text.trim().is_empty()) else {
            return Ok(Self::defaults());
        };
        let roles: Self =
            serde_json::from_str(text).map_err(|_| "角色配置不是有效的 JSON".to_string())?;
        roles.validate()?;
        Ok(roles)
    }

    pub fn validate(&self) -> Result<()> {
        if !REVIEWER_AGENTS.contains(&self.plan.agent.as_str()) {
            return Err("规划角色只能选 Codex、Hermes、DSH 或 Claude".into());
        }
        if !EXECUTOR_AGENTS.contains(&self.implement.agent.as_str()) {
            return Err("执行角色只能选 Codex、Hermes、DSH 或 Claude".into());
        }
        if !REVIEWER_AGENTS.contains(&self.review.agent.as_str()) {
            return Err("验收角色只能选 Codex、Hermes、DSH 或 Claude".into());
        }
        Ok(())
    }
}

/// 工作流上取角色配置；没有就用缺省组合（老工作流照旧能续跑）。
pub fn roles_of(workflow: &Workflow) -> Result<Roles> {
    Roles::parse(workflow.roles.as_deref())
}

/// New implementation plans always use the executor selected for this workflow.
/// A model suggestion belongs to its original agent, so clear it when reassigned.
#[cfg(test)]
pub fn bind_plan_to_executor(plan: &mut Plan, executor: &str) -> Result<()> {
    if !EXECUTOR_AGENTS.contains(&executor) {
        return Err("实现角色只能选 Codex、Hermes、DSH 或 Claude".into());
    }
    for task in &mut plan.tasks {
        if task.agent_id != executor {
            task.agent_id = executor.to_owned();
            task.execution = None;
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PlannedTask {
    pub title: String,
    pub agent_id: String,
    pub instructions: String,
    pub files: Vec<String>,
    pub depends_on: Vec<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<ExecutionChoice>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub summary: String,
    pub tasks: Vec<PlannedTask>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Review {
    pub approved: bool,
    pub summary: String,
    pub issues: Vec<String>,
}

/// Accept one JSON document, optionally in one Markdown fence with brief prose.
/// Never guess between multiple objects or extract commands from explanatory text.
pub(crate) fn json_document(raw: &str, limit: usize) -> Result<&str> {
    let raw = raw.trim();
    if raw.len() > limit + 2000 {
        return Err("结构化 JSON 结果过长".into());
    }
    let document = if let Some(start) = raw.find("```") {
        let opening = &raw[start..];
        let newline = opening.find('\n').ok_or("结构化 JSON 代码块不完整")?;
        if !matches!(opening[..newline].trim(), "```json" | "```") {
            return Err("结构化结果需要 JSON 代码块".into());
        }
        let rest = &opening[newline + 1..];
        let end = rest.find("```").ok_or("结构化 JSON 代码块不完整")?;
        let prefix = &raw[..start];
        let suffix = &rest[end + 3..];
        if prefix.len() + suffix.len() > 2000
            || suffix.contains("```")
            || prefix
                .chars()
                .chain(suffix.chars())
                .any(|c| "{}[]".contains(c))
        {
            return Err("结构化结果包含多个文档或不明确的额外内容".into());
        }
        rest[..end].trim()
    } else if raw.starts_with('{') {
        raw
    } else {
        // Some native clients prepend a short progress sentence before their final JSON.
        // Accept only one balanced top-level object, with bounded prose around it and no
        // additional JSON-shaped content. The typed serde parse below remains authoritative.
        let start = raw.find('{').ok_or("未找到结构化 JSON 对象")?;
        let prefix = &raw[..start];
        if prefix.len() > 2000
            || prefix.contains("```")
            || prefix.chars().any(|c| "{}[]".contains(c))
        {
            return Err("结构化结果包含不明确的前置内容".into());
        }
        let mut depth = 0usize;
        let mut in_string = false;
        let mut escaped = false;
        let mut end = None;
        for (offset, ch) in raw[start..].char_indices() {
            if in_string {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    in_string = false;
                }
                continue;
            }
            match ch {
                '"' => in_string = true,
                '{' => depth += 1,
                '}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        end = Some(start + offset + ch.len_utf8());
                        break;
                    }
                }
                _ => {}
            }
        }
        let end = end.ok_or("结构化 JSON 对象不完整")?;
        let suffix = &raw[end..];
        if suffix.len() > 2000
            || suffix.contains("```")
            || suffix.chars().any(|c| "{}[]".contains(c))
        {
            return Err("结构化结果包含多个文档或不明确的后置内容".into());
        }
        &raw[start..end]
    };
    if document.len() > limit {
        return Err("结构化 JSON 结果过长".into());
    }
    Ok(document)
}

pub fn parse_review(raw: &str) -> Result<Review> {
    let raw = json_document(raw, 32_000)?;
    let review: Review = serde_json::from_str(raw)
        .or_else(|_| serde_json::from_str(&quote_bare_object_keys(raw)))
        .map_err(|_| "验收成员未返回有效的结构化结果")?;
    if review.summary.trim().is_empty()
        || review.summary.chars().count() > 2000
        || review.issues.len() > 12
        || review
            .issues
            .iter()
            .any(|s| s.trim().is_empty() || s.chars().count() > 2000)
        || (review.approved && !review.issues.is_empty())
    {
        return Err("验收结果的摘要或问题列表无效".into());
    }
    Ok(review)
}

/// Some native agents emit a JSON object with bare property names. Quote keys only;
/// never evaluate code, rewrite strings, or infer an approval from prose.
fn quote_bare_object_keys(raw: &str) -> String {
    let chars: Vec<char> = raw.chars().collect();
    let mut result = String::with_capacity(raw.len());
    let (mut index, mut quoted, mut escaped, mut key) = (0, false, false, false);
    while index < chars.len() {
        let ch = chars[index];
        if quoted {
            result.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                quoted = false;
            }
            index += 1;
            continue;
        }
        if key && (ch.is_ascii_alphabetic() || ch == '_') {
            let start = index;
            while index < chars.len()
                && (chars[index].is_ascii_alphanumeric() || chars[index] == '_')
            {
                index += 1;
            }
            let mut colon = index;
            while colon < chars.len() && chars[colon].is_whitespace() {
                colon += 1;
            }
            let quote = chars.get(colon) == Some(&':');
            if quote {
                result.push('"');
            }
            result.extend(chars[start..index].iter());
            if quote {
                result.push('"');
            }
            key = false;
            continue;
        }
        if ch == '"' {
            quoted = true;
            key = false;
        } else if matches!(ch, '{' | ',') {
            key = true;
        } else if !ch.is_whitespace() {
            key = false;
        }
        result.push(ch);
        index += 1;
    }
    result
}

#[derive(Clone, Debug, Serialize)]
pub struct Task {
    pub id: String,
    pub workflow_id: String,
    pub position: u32,
    pub title: String,
    pub agent_id: String,
    pub instructions: String,
    pub files: Vec<String>,
    pub depends_on: Vec<u32>,
    pub status: String,
    pub output: String,
    pub error: Option<String>,
    /// 该任务的模型/强度覆盖，NULL＝用角色配置或自动选型。
    pub model: Option<String>,
    pub effort: Option<String>,
    /// 独立 git worktree 的路径与分支名，NULL＝还没建（B2 才写入）。
    pub worktree: Option<String>,
    pub branch: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Workflow {
    pub id: String,
    pub project_id: String,
    pub conversation_id: String,
    pub user_message_id: String,
    pub request: String,
    pub status: String,
    pub plan: Option<Plan>,
    /// 这次工作流的角色配置 JSON（规划/执行/验收的成员+模型+强度），NULL＝沿用旧行为。
    pub roles: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coordination: Option<crate::coordinator_store::Coordination>,
    pub summary: String,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub tasks: Vec<Task>,
    pub attempts: Vec<Attempt>,
    pub changes: Vec<Change>,
    pub agent_messages: Vec<AgentMessage>,
    pub team_messages: Vec<TeamMessage>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProjectAgentChat {
    pub group_conversation_id: String,
    pub agent_id: String,
    pub conversation_id: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct TeamMessage {
    pub id: String,
    pub project_id: String,
    pub conversation_id: String,
    pub workflow_id: Option<String>,
    pub attempt_id: Option<String>,
    pub source_message_id: Option<String>,
    pub sender_agent: Option<String>,
    pub kind: String,
    pub content: String,
    pub created_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct RoomAgentMessage {
    pub id: String,
    pub sender_agent: String,
    pub recipient_agent: String,
    pub content: String,
    pub created_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct AgentMessage {
    pub id: String,
    pub workflow_id: String,
    pub attempt_id: String,
    pub sender_agent: String,
    pub recipient_agent: String,
    pub content: String,
    pub created_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Change {
    pub attempt_id: String,
    pub path: String,
    pub operation: String,
    pub before_hash: Option<String>,
    pub after_hash: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Attempt {
    pub id: String,
    pub created_at: i64,
    pub workflow_id: String,
    pub task_id: Option<String>,
    pub agent_id: String,
    pub stage: String,
    pub activity: Option<String>,
    pub status: String,
    pub native_thread_id: Option<String>,
    pub native_turn_id: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub thought: String,
    pub tool_events: Vec<crate::store::RunToolEvent>,
    pub output: String,
    pub error: Option<String>,
}

pub fn live(status: &str) -> bool {
    matches!(
        status,
        "queued" | "planning" | "running" | "verifying" | "reviewing" | "cancelling"
    )
}

pub fn migrate(tx: &Transaction<'_>) -> Result<()> {
    // Legacy `checks` columns stay in existing databases for non-destructive upgrades; the project workflow no longer reads or executes them.
    tx.execute_batch("CREATE TABLE IF NOT EXISTS projects (
        id TEXT PRIMARY KEY,name TEXT NOT NULL,root TEXT NOT NULL,root_key TEXT NOT NULL UNIQUE,
        checks TEXT NOT NULL,summary_enabled INTEGER NOT NULL DEFAULT 0 CHECK(summary_enabled IN (0,1)),created_at INTEGER NOT NULL);
      CREATE TABLE IF NOT EXISTS conversation_projects (
        conversation_id TEXT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
        project_id TEXT NOT NULL REFERENCES projects(id),roles TEXT);
      CREATE TABLE IF NOT EXISTS workflows (
        id TEXT PRIMARY KEY,project_id TEXT NOT NULL REFERENCES projects(id),
        conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
        user_message_id TEXT NOT NULL UNIQUE REFERENCES messages(id) ON DELETE CASCADE,
        request TEXT NOT NULL,status TEXT NOT NULL CHECK(status IN ('queued','planning','running','verifying','reviewing','cancelling','completed','failed','interrupted')),
        plan TEXT,roles TEXT,summary TEXT NOT NULL DEFAULT '',error TEXT,created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL);
      CREATE UNIQUE INDEX IF NOT EXISTS one_conversation_workflow ON workflows(conversation_id) WHERE status IN ('queued','planning','running','verifying','reviewing','cancelling');
      CREATE TABLE IF NOT EXISTS project_tasks (
        id TEXT PRIMARY KEY,workflow_id TEXT NOT NULL REFERENCES workflows(id) ON DELETE CASCADE,
        position INTEGER NOT NULL,title TEXT NOT NULL,agent_id TEXT NOT NULL CHECK(agent_id IN ('codex-win','dsh-win')),
        instructions TEXT NOT NULL,files TEXT NOT NULL,depends_on TEXT NOT NULL,
        status TEXT NOT NULL CHECK(status IN ('queued','running','completed','failed','interrupted','skipped')),
        output TEXT NOT NULL DEFAULT '',error TEXT,assigned_agent TEXT,
        model TEXT,effort TEXT,worktree TEXT,branch TEXT,UNIQUE(workflow_id,position));
      CREATE TABLE IF NOT EXISTS project_leases (
        workflow_id TEXT PRIMARY KEY REFERENCES workflows(id) ON DELETE CASCADE,
        project_id TEXT NOT NULL UNIQUE REFERENCES projects(id),root_key TEXT NOT NULL UNIQUE,acquired_at INTEGER NOT NULL);
      CREATE TABLE IF NOT EXISTS project_attempts (
        id TEXT PRIMARY KEY,workflow_id TEXT NOT NULL REFERENCES workflows(id) ON DELETE CASCADE,
        task_id TEXT REFERENCES project_tasks(id) ON DELETE CASCADE,
        agent_id TEXT NOT NULL CHECK(agent_id IN ('hermes-win','codex-win','dsh-win')),
        stage TEXT NOT NULL CHECK(stage IN ('plan','implement','verify','review','repair')),
        status TEXT NOT NULL CHECK(status IN ('starting','running','cancelling','completed','failed','interrupted')),
        native_thread_id TEXT,native_turn_id TEXT,model TEXT,reasoning_effort TEXT,
        output TEXT NOT NULL DEFAULT '',thought TEXT NOT NULL DEFAULT '',checks TEXT NOT NULL DEFAULT '[]',error TEXT,created_at INTEGER NOT NULL);
      CREATE TABLE IF NOT EXISTS project_attempt_events (
        attempt_id TEXT NOT NULL REFERENCES project_attempts(id) ON DELETE CASCADE,
        event_id TEXT NOT NULL,kind TEXT NOT NULL,title TEXT NOT NULL,input TEXT,output TEXT,
        status TEXT NOT NULL,created_at INTEGER NOT NULL,PRIMARY KEY(attempt_id,event_id));
      CREATE INDEX IF NOT EXISTS project_attempt_event_order ON project_attempt_events(attempt_id,created_at,event_id);
      CREATE TABLE IF NOT EXISTS project_changes (
        attempt_id TEXT NOT NULL REFERENCES project_attempts(id) ON DELETE CASCADE,
        path TEXT NOT NULL,operation TEXT NOT NULL,before_hash TEXT,after_hash TEXT,
        PRIMARY KEY(attempt_id,path));
      CREATE TABLE IF NOT EXISTS project_questions (
        id TEXT PRIMARY KEY,workflow_id TEXT NOT NULL REFERENCES workflows(id) ON DELETE CASCADE,
        conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,agent_id TEXT NOT NULL,
        prompt TEXT NOT NULL,options TEXT NOT NULL DEFAULT '[]',status TEXT NOT NULL,answer TEXT,created_at INTEGER NOT NULL,answered_at INTEGER,attempt_id TEXT REFERENCES project_attempts(id) ON DELETE CASCADE,delivery TEXT NOT NULL DEFAULT 'deferred');
      CREATE TABLE IF NOT EXISTS project_sessions (
        project_id TEXT NOT NULL REFERENCES projects(id),agent_id TEXT NOT NULL,
        stage TEXT NOT NULL,native_session_id TEXT,PRIMARY KEY(project_id,agent_id,stage));
      CREATE TABLE IF NOT EXISTS project_agent_chats (
        project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
        group_conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
        agent_id TEXT NOT NULL CHECK(agent_id IN ('hermes-win','codex-win','dsh-win')),
        conversation_id TEXT NOT NULL UNIQUE REFERENCES conversations(id) ON DELETE CASCADE,
        created_at INTEGER NOT NULL,PRIMARY KEY(project_id,group_conversation_id,agent_id));
      CREATE TABLE IF NOT EXISTS project_agent_messages (
        id TEXT PRIMARY KEY,workflow_id TEXT NOT NULL REFERENCES workflows(id) ON DELETE CASCADE,
        attempt_id TEXT NOT NULL REFERENCES project_attempts(id) ON DELETE CASCADE,
        sender_agent TEXT NOT NULL CHECK(sender_agent IN ('hermes-win','codex-win','dsh-win')),
        recipient_agent TEXT NOT NULL CHECK(recipient_agent IN ('hermes-win','codex-win','dsh-win')),
        content TEXT NOT NULL,created_at INTEGER NOT NULL);
      CREATE TABLE IF NOT EXISTS project_team_messages (
        id TEXT PRIMARY KEY,project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
        conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
        workflow_id TEXT REFERENCES workflows(id) ON DELETE CASCADE,
        attempt_id TEXT REFERENCES project_attempts(id) ON DELETE CASCADE,
        source_message_id TEXT UNIQUE,
        sender_agent TEXT CHECK(sender_agent IS NULL OR sender_agent IN ('hermes-win','codex-win','dsh-win')),
        message_kind TEXT NOT NULL DEFAULT 'agent' CHECK(message_kind IN ('agent','system','user')),
        content TEXT NOT NULL,created_at INTEGER NOT NULL);
      CREATE INDEX IF NOT EXISTS project_agent_inbox ON project_agent_messages(workflow_id,recipient_agent,created_at);
      CREATE INDEX IF NOT EXISTS project_team_feed ON project_team_messages(conversation_id,created_at,id);")
        .map_err(|e|e.to_string())?;
    // 每任务独立工作树后，同一成员会并行跑多条 attempt：旧的「同一成员只允许一条活动 attempt」
    // 唯一索引会挡住并行，独占改由 begin_attempt 的按任务软件判定负责，这里幂等删掉。
    tx.execute_batch("DROP INDEX IF EXISTS one_project_agent_attempt;")
        .map_err(|e| e.to_string())?;
    // 已经存在的库不会因为 CREATE TABLE IF NOT EXISTS 拿到新列，只能逐列 ALTER。
    // 加列前先查 PRAGMA table_info：同一份迁移重复跑（含 v9 降级升回来）也不会炸，
    // 全新库因为建表语句里已经带上了这些列，这里同样什么都不做。
    for (table, column, definition) in [
        ("conversation_projects", "roles", "TEXT"),
        ("workflows", "roles", "TEXT"),
        ("project_tasks", "model", "TEXT"),
        ("project_tasks", "effort", "TEXT"),
        ("project_tasks", "worktree", "TEXT"),
        ("project_tasks", "branch", "TEXT"),
        ("project_tasks", "assigned_agent", "TEXT"),
        ("project_attempts", "thought", "TEXT NOT NULL DEFAULT ''"),
        ("project_team_messages", "recipient_agent", "TEXT"),
        ("project_attempts", "activity", "TEXT"),
        ("project_questions", "attempt_id", "TEXT"),
        (
            "project_questions",
            "delivery",
            "TEXT NOT NULL DEFAULT 'deferred'",
        ),
        ("project_questions", "answered_at", "INTEGER"),
    ] {
        add_column(tx, table, column, definition)?;
    }
    crate::coordinator_store::migrate(tx)?;
    Ok(())
}

/// 幂等加列：列已存在就跳过，老库补列、新库不重复加。
pub(crate) fn add_column(tx: &Transaction<'_>, table: &str, column: &str, definition: &str) -> Result<()> {
    let mut statement = tx
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|e| e.to_string())?;
    let exists = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| e.to_string())?
        .filter_map(std::result::Result::ok)
        .any(|name| name == column);
    drop(statement);
    if !exists {
        tx.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {definition};"
        ))
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub(crate) fn key(path: &Path) -> String {
    let value = path
        .to_string_lossy()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_owned();
    if cfg!(windows) {
        let value = value.to_lowercase();
        if let Some(rest) = value.strip_prefix("//?/unc/") {
            format!("//{rest}")
        } else {
            value.strip_prefix("//?/").unwrap_or(&value).to_owned()
        }
    } else {
        value
    }
}
fn overlaps(left: &str, right: &str) -> bool {
    left == right
        || left.starts_with(&format!("{right}/"))
        || right.starts_with(&format!("{left}/"))
}
pub fn protected_relative(path: &str) -> Result<String> {
    let path = path.replace('\\', "/");
    if path.is_empty()
        || path.len() > 400
        || path.chars().any(|c| ":<>\"|?*".contains(c))
        || path.chars().any(char::is_control)
        || Path::new(&path).is_absolute()
    {
        return Err("任务文件必须是项目内的相对路径".into());
    }
    for part in path.split('/') {
        let lower = part.to_lowercase();
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.ends_with(' ')
            || part.ends_with('.')
            || matches!(
                lower.as_str(),
                ".git"
                    | ".codex"
                    | ".hermes"
                    | ".ssh"
                    | "node_modules"
                    | "agent-keys.json"
                    | "credentials.json"
                    | "auth.json"
            )
            || lower == ".env"
            || lower.starts_with(".env.")
            || lower.ends_with(".pem")
            || lower.ends_with(".key")
            || lower.starts_with("id_rsa")
            || lower.starts_with("id_ed25519")
        {
            return Err("路径越界、受保护或包含不支持的段".into());
        }
        let stem = lower.split('.').next().unwrap_or("");
        if matches!(
            stem,
            "con"
                | "prn"
                | "aux"
                | "nul"
                | "com1"
                | "com2"
                | "com3"
                | "com4"
                | "com5"
                | "com6"
                | "com7"
                | "com8"
                | "com9"
                | "lpt1"
                | "lpt2"
                | "lpt3"
                | "lpt4"
                | "lpt5"
                | "lpt6"
                | "lpt7"
                | "lpt8"
                | "lpt9"
        ) {
            return Err("不支持 Windows 保留路径名".into());
        }
    }
    Ok(path)
}

pub fn task_path(root: &Path, relative: &str) -> Result<PathBuf> {
    let relative = protected_relative(relative)?;
    let root = root.canonicalize().map_err(|_| "项目目录不可用")?;
    let target = root.join(relative);
    let mut ancestor = target.as_path();
    let mut missing = Vec::new();
    while !ancestor.exists() {
        missing.push(ancestor.file_name().ok_or("路径无效")?.to_owned());
        ancestor = ancestor.parent().ok_or("路径无效")?;
    }
    let mut resolved = ancestor.canonicalize().map_err(|_| "无法确认目标路径")?;
    if !resolved.starts_with(&root) {
        return Err("目标通过链接越出项目目录".into());
    }
    for part in missing.iter().rev() {
        resolved.push(part);
    }
    // Canonical existing aliases must also respect protected-name exclusions.
    let canonical_relative = resolved.strip_prefix(&root).map_err(|_| "目标越出项目")?;
    protected_relative(&canonical_relative.to_string_lossy())?;
    Ok(resolved)
}

pub fn parse_plan(raw: &str) -> Result<Plan> {
    let json = json_document(raw, 64_000)?;
    let mut plan: Plan = serde_json::from_str(json)
        .or_else(|_| serde_json::from_str(&quote_bare_object_keys(json)))
        .map_err(|_| "规划成员未返回有效的结构化任务，已有回复已保留")?;
    // 模型偶尔会把“依赖第 N 项”按 1 基任务编号输出。若 0 基解释不成立、
    // 但所有依赖都合法地指向更早的一基任务，则在入口统一换算为内部 0 基位置。
    let zero_based = plan.tasks.iter().enumerate().all(|(index, task)| {
        task.depends_on
            .iter()
            .all(|dependency| (*dependency as usize) < index)
    });
    let one_based = plan.tasks.iter().enumerate().all(|(index, task)| {
        task.depends_on
            .iter()
            .all(|dependency| *dependency > 0 && (*dependency as usize) <= index)
    });
    if !zero_based && one_based {
        for task in &mut plan.tasks {
            for dependency in &mut task.depends_on {
                *dependency -= 1;
            }
        }
    }
    validate_plan(&plan)?;
    Ok(plan)
}

fn validate_plan(plan: &Plan) -> Result<()> {
    if plan.summary.trim().is_empty() || plan.summary.chars().count() > 2000 || plan.tasks.len() > 5
    {
        return Err("协作结果需包含摘要，项目任务最多 5 项".into());
    }
    for (index, task) in plan.tasks.iter().enumerate() {
        if !EXECUTOR_AGENTS.contains(&task.agent_id.as_str())
            || task.title.trim().is_empty()
            || task.title.chars().count() > 100
            || task.instructions.trim().is_empty()
            || task.instructions.chars().count() > 8000
            || task.files.is_empty()
            || task.files.len() > 5
            || task.depends_on.iter().any(|d| *d as usize >= index)
        {
            return Err(
                "新任务需为可执行的实现者（Codex、Hermes、DSH 或 Claude）、1—5 个文件且依赖有效"
                    .into(),
            );
        }
        // execution 是规划给出的选型建议，用户可以在确认面板里改成别的；这里不再拒收。
        for (i, file) in task.files.iter().enumerate() {
            protected_relative(file)?;
            let normalized = protected_relative(file)?;
            if task.files[..i].iter().any(|old| {
                let old = protected_relative(old).unwrap_or_default();
                if cfg!(windows) {
                    old.eq_ignore_ascii_case(&normalized)
                } else {
                    old == normalized
                }
            }) {
                return Err("任务文件重复".into());
            }
        }
        for (i, dependency) in task.depends_on.iter().enumerate() {
            if task.depends_on[..i].contains(dependency) {
                return Err("任务依赖重复".into());
            }
        }
    }
    Ok(())
}

/// 任务查询的列顺序与 `task_from_row` 一一对应。
const TASK_COLUMNS: &str = "id,workflow_id,position,title,COALESCE(assigned_agent,agent_id) AS agent_id,instructions,files,depends_on,status,output,error,model,effort,worktree,branch";

fn task_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
    let files: String = row.get(6)?;
    let depends: String = row.get(7)?;
    Ok(Task {
        id: row.get(0)?,
        workflow_id: row.get(1)?,
        position: row.get(2)?,
        title: row.get(3)?,
        agent_id: row.get(4)?,
        instructions: row.get(5)?,
        files: serde_json::from_str(&files).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(e))
        })?,
        depends_on: serde_json::from_str(&depends).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, Box::new(e))
        })?,
        status: row.get(8)?,
        output: row.get(9)?,
        error: row.get(10)?,
        model: row.get(11)?,
        effort: row.get(12)?,
        worktree: row.get(13)?,
        branch: row.get(14)?,
    })
}

/// 可空文本覆盖：空白视为未设置（写回 NULL），超长或含控制字符直接拒绝。
fn optional_text(value: Option<String>, limit: usize, label: &str) -> Result<Option<String>> {
    let Some(text) = value else {
        return Ok(None);
    };
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    if text.chars().count() > limit || text.chars().any(char::is_control) {
        return Err(format!("{label}无效"));
    }
    Ok(Some(text.to_owned()))
}

impl Store {
    pub fn projects(&self) -> Result<Vec<Project>> {
        let mut stmt = self
            .connection
            .prepare("SELECT id FROM projects ORDER BY created_at,id")
            .map_err(|e| e.to_string())?;
        let ids = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        ids.iter().map(|id| self.project(id)).collect()
    }
    pub fn project(&self, id: &str) -> Result<Project> {
        self.connection
            .query_row(
                "SELECT id,name,root,summary_enabled,created_at FROM projects WHERE id=?1",
                [id],
                |r| {
                    Ok(Project {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        root: r.get(2)?,
                        summary_enabled: r.get(3)?,
                        created_at: r.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(|e| e.to_string())?
            .ok_or("项目不存在".into())
    }
    pub fn register_project(&mut self, name: &str, root: &str) -> Result<Project> {
        let name = name.trim();
        let root = Path::new(root);
        if name.is_empty()
            || name.chars().count() > 80
            || name.chars().any(char::is_control)
            || !root.is_absolute()
            || root
                .components()
                .filter(|p| matches!(p, Component::Normal(_)))
                .count()
                < 2
        {
            return Err("请输入项目名称和具体项目目录的绝对路径".into());
        }
        let root = root.canonicalize().map_err(|_| "项目目录不存在")?;
        if !root.is_dir() {
            return Err("项目路径必须是目录".into());
        }
        if let Some(data) = self.path.parent().filter(|p| p.is_absolute()) {
            let data = data.canonicalize().map_err(|_| "应用数据目录不可用")?;
            if overlaps(&key(&root), &key(&data)) {
                return Err("项目目录不能与同席数据目录重叠".into());
            }
        }
        let root_key = key(&root);
        for blocked in ["/.codex", "/.hermes", "/.ssh", "/windows", "/program files"] {
            if root_key.contains(blocked) {
                return Err("不能绑定系统或 agent 配置目录".into());
            }
        }
        let existing = self
            .connection
            .query_row(
                "SELECT id FROM projects WHERE root_key=?1",
                [&root_key],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if let Some(id) = existing {
            let project = self.project(&id)?;
            if project.name != name {
                return Err("此目录已登记，不能以不同名称重复绑定".into());
            }
            return Ok(project);
        }
        let id = Uuid::new_v4().to_string();
        self.connection.execute("INSERT INTO projects(id,name,root,root_key,checks,created_at) VALUES(?1,?2,?3,?4,'[]',?5)",params![id,name,root.to_string_lossy(),root_key,now()]).map_err(|e|e.to_string())?;
        self.project(&id)
    }
    pub fn conversation_project(&self, room: &str) -> Result<Option<Project>> {
        let id = self
            .connection
            .query_row(
                "SELECT project_id FROM conversation_projects WHERE conversation_id=?1",
                [room],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        id.map(|id| self.project(&id)).transpose()
    }
    pub fn conversation_project_roles(&self, room: &str) -> Result<Option<Roles>> {
        let text = self
            .connection
            .query_row(
                "SELECT roles FROM conversation_projects WHERE conversation_id=?1",
                [room],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(|_| "项目角色配置读取失败")?
            .flatten();
        text.map(|text| Roles::parse(Some(&text))).transpose()
    }
    pub fn set_conversation_project_roles(&mut self, room: &str, roles: &Roles) -> Result<()> {
        roles.validate()?;
        let conversation = self.conversation(room)?;
        if conversation.archived || conversation.kind != "group" {
            return Err("项目角色只能设置在未归档群聊中".into());
        }
        if [
            &roles.plan.agent,
            &roles.implement.agent,
            &roles.review.agent,
        ]
        .iter()
        .any(|agent| !conversation.members.contains(agent))
        {
            return Err("规划、执行、验收成员必须属于当前群聊".into());
        }
        self.guard_discussion(room)?;
        // A workflow stores its Roles snapshot when it starts; editing project defaults only affects the next workflow.
        if self.conversation_project(room)?.is_none() {
            return Err("请先为群聊绑定一个 Git 项目".into());
        }
        let encoded = serde_json::to_string(roles).map_err(|_| "项目角色配置序列化失败")?;
        let changed = self
            .connection
            .execute(
                "UPDATE conversation_projects SET roles=?1 WHERE conversation_id=?2",
                params![encoded, room],
            )
            .map_err(|_| "项目角色配置保存失败")?;
        if changed == 0 {
            return Err("群聊项目绑定不存在".into());
        }
        Ok(())
    }
    pub fn bind_project(&mut self, room: &str, project: Option<&str>) -> Result<()> {
        let conversation = self.conversation(room)?;
        if conversation.kind != "group" {
            return Err("只有群聊才能绑定项目".into());
        }
        if conversation.archived {
            return Err("先恢复归档会话再绑定项目".into());
        }
        self.guard_discussion(room)?;
        self.guard_workflow(room)?;
        let previous = self.conversation_project(room)?.map(|project| project.id);
        if previous.as_deref() != project {
            self.connection
                .execute(
                    "DELETE FROM project_coordinator_settings WHERE conversation_id=?1",
                    [room],
                )
                .map_err(|_| "管家配置重置失败")?;
        }
        if let Some(project) = project {
            self.project(project)?;
            self.connection.execute("INSERT INTO conversation_projects(conversation_id,project_id,roles) VALUES(?1,?2,NULL) ON CONFLICT(conversation_id) DO UPDATE SET project_id=excluded.project_id,roles=CASE WHEN conversation_projects.project_id=excluded.project_id THEN conversation_projects.roles ELSE NULL END",params![room,project]).map_err(|e|e.to_string())?;
        } else {
            self.connection
                .execute(
                    "DELETE FROM conversation_projects WHERE conversation_id=?1",
                    [room],
                )
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
    pub fn guard_workflow(&self, room: &str) -> Result<()> {
        let active=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM workflows WHERE conversation_id=?1 AND status IN ('queued','planning','running','verifying','reviewing','cancelling'))",[room],|r|r.get::<_,bool>(0)).map_err(|e|e.to_string())?;
        if active {
            return Err("项目协作进行中，请先停止再修改或发送新需求".into());
        }
        Ok(())
    }
    pub fn start_workflow(
        &mut self,
        room: &str,
        message: &str,
        content: &str,
    ) -> Result<(Workflow, bool)> {
        let project = self
            .conversation_project(room)?
            .ok_or("请先为会话绑定项目")?;
        let existing = self
            .connection
            .query_row(
                "SELECT id FROM workflows WHERE user_message_id=?1",
                [message],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if let Some(id) = existing {
            let existing = self.workflow(&id)?;
            if existing.conversation_id != room
                || existing.request != content.trim()
                || existing.project_id != project.id
            {
                return Err("协作请求 ID 已用于其他内容或项目".into());
            }
            return Ok((existing, false));
        }
        for agent in EXECUTOR_AGENTS {
            self.guard_service_maintenance(agent)?;
        }
        self.guard_workflow(room)?;
        self.guard_discussion(room)?;
        let saved = self.save_message(room, message, content)?;
        if saved.sender_id != "user"
            || self.run_for_message(message)?.is_some()
            || self.discussion_for_message(message)?.is_some()
        {
            return Err("这条消息已用于其他运行".into());
        }
        let id = Uuid::new_v4().to_string();
        let tx = self.connection.transaction().map_err(|e| e.to_string())?;
        tx.execute("INSERT INTO workflows(id,project_id,conversation_id,user_message_id,request,status,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,'queued',?6,?6)",params![id,project.id,room,message,saved.content,now()]).map_err(|e|e.to_string())?;
        tx.execute(
            "UPDATE messages SET status='pending' WHERE id=?1",
            [message],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok((self.workflow(&id)?, true))
    }
    pub fn workflow(&self, id: &str) -> Result<Workflow> {
        let mut workflow=self.connection.query_row("SELECT id,project_id,conversation_id,user_message_id,request,status,plan,summary,error,created_at,updated_at,roles FROM workflows WHERE id=?1",[id],|r| {
            let plan:Option<String>=r.get(6)?;
            let plan=plan.map(|text|serde_json::from_str(&text)).transpose().map_err(|e|rusqlite::Error::FromSqlConversionFailure(6,rusqlite::types::Type::Text,Box::new(e)))?;
            Ok(Workflow{id:r.get(0)?,project_id:r.get(1)?,conversation_id:r.get(2)?,user_message_id:r.get(3)?,request:r.get(4)?,status:r.get(5)?,plan,roles:r.get(11)?,coordination:None,summary:r.get(7)?,error:r.get(8)?,created_at:r.get(9)?,updated_at:r.get(10)?,tasks:vec![],attempts:vec![],changes:vec![],agent_messages:vec![],team_messages:vec![]})
        }).optional().map_err(|e|e.to_string())?.ok_or("项目协作不存在")?;
        let mut stmt = self
            .connection
            .prepare(&format!(
                "SELECT {TASK_COLUMNS} FROM project_tasks WHERE workflow_id=?1 ORDER BY position"
            ))
            .map_err(|e| e.to_string())?;
        workflow.tasks = stmt
            .query_map([id], task_from_row)
            .map_err(|e| e.to_string())?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        workflow.attempts = self.attempts(id)?;
        let mut changes=self.connection.prepare("SELECT c.attempt_id,c.path,c.operation,c.before_hash,c.after_hash FROM project_changes c JOIN project_attempts a ON a.id=c.attempt_id WHERE a.workflow_id=?1 ORDER BY a.rowid,c.path").map_err(|e|e.to_string())?;
        workflow.changes = changes
            .query_map([id], |r| {
                Ok(Change {
                    attempt_id: r.get(0)?,
                    path: r.get(1)?,
                    operation: r.get(2)?,
                    before_hash: r.get(3)?,
                    after_hash: r.get(4)?,
                })
            })
            .map_err(|e| e.to_string())?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        workflow.agent_messages = self.agent_messages(id)?;
        workflow.team_messages = self.workflow_team_messages(id)?;
        workflow.coordination = self.coordination(id)?;
        Ok(workflow)
    }

    pub fn workflow_team_messages(&self, workflow_id: &str) -> Result<Vec<TeamMessage>> {
        let mut statement = self.connection.prepare(
            "SELECT id,project_id,conversation_id,workflow_id,attempt_id,source_message_id,sender_agent,message_kind,content,created_at FROM project_team_messages WHERE workflow_id=?1 AND recipient_agent IS NULL ORDER BY created_at,id",
        ).map_err(|_| "团队消息读取失败")?;
        let rows = statement
            .query_map([workflow_id], |row| {
                Ok(TeamMessage {
                    id: row.get(0)?,
                    project_id: row.get(1)?,
                    conversation_id: row.get(2)?,
                    workflow_id: row.get(3)?,
                    attempt_id: row.get(4)?,
                    source_message_id: row.get(5)?,
                    sender_agent: row.get(6)?,
                    kind: row.get(7)?,
                    content: row.get(8)?,
                    created_at: row.get(9)?,
                })
            })
            .map_err(|_| "团队消息读取失败")?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    pub fn team_messages(&self, conversation_id: &str) -> Result<Vec<TeamMessage>> {
        let mut statement = self.connection.prepare(
            "SELECT id,project_id,conversation_id,workflow_id,attempt_id,source_message_id,sender_agent,message_kind,content,created_at FROM project_team_messages WHERE conversation_id=?1 AND recipient_agent IS NULL ORDER BY created_at,id",
        ).map_err(|_| "团队消息读取失败")?;
        let rows = statement
            .query_map([conversation_id], |row| {
                Ok(TeamMessage {
                    id: row.get(0)?,
                    project_id: row.get(1)?,
                    conversation_id: row.get(2)?,
                    workflow_id: row.get(3)?,
                    attempt_id: row.get(4)?,
                    source_message_id: row.get(5)?,
                    sender_agent: row.get(6)?,
                    kind: row.get(7)?,
                    content: row.get(8)?,
                    created_at: row.get(9)?,
                })
            })
            .map_err(|_| "团队消息读取失败")?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| "团队消息读取失败".into())
    }

    pub fn add_system_team_message(
        &mut self,
        workflow_id: &str,
        content: &str,
    ) -> Result<TeamMessage> {
        let workflow = self.workflow(workflow_id)?;
        let content = content.trim();
        if content.is_empty() || content.chars().count() > 4000 {
            return Err("团队状态消息无效".into());
        }
        let id = Uuid::new_v4().to_string();
        let created_at = now();
        let tx = self.connection.transaction().map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO project_team_messages(id,project_id,conversation_id,workflow_id,message_kind,content,created_at) VALUES(?1,?2,?3,?4,'system',?5,?6)",
            params![id,workflow.project_id,workflow.conversation_id,workflow.id,content,created_at],
        ).map_err(|_| "团队状态消息保存失败")?;
        tx.execute(
            "UPDATE workflows SET updated_at=?1 WHERE id=?2",
            params![created_at, workflow_id],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        self.team_messages(&workflow.conversation_id)?
            .into_iter()
            .find(|message| message.id == id)
            .ok_or("团队状态消息不可用".into())
    }

    pub fn post_user_team_message(
        &mut self,
        conversation_id: &str,
        content: &str,
    ) -> Result<TeamMessage> {
        let group = self.conversation(conversation_id)?;
        if group.kind != "group" || group.archived {
            return Err("请选择未归档的项目群聊".into());
        }
        let project = self
            .conversation_project(conversation_id)?
            .ok_or("群聊未绑定项目")?;
        let workflow_id: String = self.connection.query_row(
            "SELECT id FROM workflows WHERE conversation_id=?1 AND status IN ('queued','planning','running','verifying','reviewing','cancelling') ORDER BY created_at DESC,rowid DESC LIMIT 1",
            [conversation_id], |row| row.get(0),
        ).optional().map_err(|_| "当前协作状态读取失败")?.ok_or("当前没有进行中的团队协作")?;
        let content = content.trim();
        if content.is_empty()
            || content.chars().count() > 16000
            || content
                .chars()
                .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
        {
            return Err("团队消息需为 1–16000 个有效字符".into());
        }
        let roles = crate::project_directives::encoded_roles(self, &workflow_id, content)?;
        let id = Uuid::new_v4().to_string();
        let created_at = now();
        let tx = self.connection.transaction().map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO project_team_messages(id,project_id,conversation_id,workflow_id,message_kind,content,created_at) VALUES(?1,?2,?3,?4,'user',?5,?6)",
            params![id,project.id,conversation_id,workflow_id,content,created_at],
        ).map_err(|_| "团队消息保存失败")?;
        crate::project_directives::save_roles(&tx, &workflow_id, &roles, created_at)?;
        tx.commit().map_err(|e| e.to_string())?;
        self.team_messages(conversation_id)?
            .into_iter()
            .find(|message| message.id == id)
            .ok_or("团队消息不可用".into())
    }

    pub fn share_project_agent_message(
        &mut self,
        group_id: &str,
        agent_chat_id: &str,
        source_message_id: &str,
    ) -> Result<TeamMessage> {
        let group = self.conversation(group_id)?;
        if group.kind != "group" || group.archived {
            return Err("请选择未归档的项目群聊".into());
        }
        let project = self
            .conversation_project(group_id)?
            .ok_or("群聊未绑定项目")?;
        let (agent, content): (String, String) = self.connection.query_row(
            "SELECT pac.agent_id,m.content FROM project_agent_chats pac JOIN messages m ON m.conversation_id=pac.conversation_id WHERE pac.project_id=?1 AND pac.group_conversation_id=?2 AND pac.conversation_id=?3 AND m.id=?4 AND m.sender_id=pac.agent_id AND m.status='completed'",
            params![project.id,group_id,agent_chat_id,source_message_id],
            |row| Ok((row.get(0)?,row.get(1)?)),
        ).optional().map_err(|_| "Agent 消息读取失败")?.ok_or("只能分享同一项目 Agent 已完成的回复")?;
        if !group.members.contains(&agent) {
            return Err("该 Agent 不属于当前项目群".into());
        }
        if let Some(existing_id) = self
            .connection
            .query_row(
                "SELECT id FROM project_team_messages WHERE source_message_id=?1",
                [source_message_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|_| "团队消息状态读取失败")?
        {
            return self
                .team_messages(group_id)?
                .into_iter()
                .find(|message| message.id == existing_id)
                .ok_or("团队消息已分享至其他会话".into());
        }
        let id = Uuid::new_v4().to_string();
        self.connection.execute(
            "INSERT INTO project_team_messages(id,project_id,conversation_id,source_message_id,sender_agent,content,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![id,project.id,group_id,source_message_id,agent,content,now()],
        ).map_err(|_| "分享至团队会话失败")?;
        self.team_messages(group_id)?
            .into_iter()
            .find(|message| message.id == id)
            .ok_or("分享后的团队消息不可用".into())
    }

    pub fn project_agent_chats(&self, group_id: &str) -> Result<Vec<ProjectAgentChat>> {
        let Some(project) = self.conversation_project(group_id)? else {
            return Ok(Vec::new());
        };
        let mut statement = self.connection.prepare(
            "SELECT group_conversation_id,agent_id,conversation_id FROM project_agent_chats WHERE project_id=?1 AND group_conversation_id=?2 ORDER BY CASE agent_id WHEN 'codex-win' THEN 0 WHEN 'hermes-win' THEN 1 ELSE 2 END",
        ).map_err(|_| "Agent 会话读取失败")?;
        let rows = statement
            .query_map(params![project.id, group_id], |row| {
                Ok(ProjectAgentChat {
                    group_conversation_id: row.get(0)?,
                    agent_id: row.get(1)?,
                    conversation_id: row.get(2)?,
                })
            })
            .map_err(|_| "Agent 会话读取失败")?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| "Agent 会话读取失败".into())
    }

    pub fn project_chat_messages(&self, conversation_id: &str) -> Result<Vec<RoomAgentMessage>> {
        let Some((project, group, agent)) = self.project_agent_chat_context(conversation_id)?
        else {
            return Ok(Vec::new());
        };
        let mut query = self.connection.prepare("SELECT id,sender_agent,recipient_agent,content,created_at FROM project_team_messages WHERE project_id=?1 AND conversation_id=?2 AND recipient_agent IS NOT NULL AND (sender_agent=?3 OR recipient_agent=?3) ORDER BY created_at,id").map_err(|_| "成员直发消息不可用")?;
        let rows = query
            .query_map(params![project, group, agent], |row| {
                Ok(RoomAgentMessage {
                    id: row.get(0)?,
                    sender_agent: row.get(1)?,
                    recipient_agent: row.get(2)?,
                    content: row.get(3)?,
                    created_at: row.get(4)?,
                })
            })
            .map_err(|_| "成员直发消息读取失败")?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| "成员直发消息读取失败".into());
        rows
    }

    pub fn project_agent_chat_context(
        &self,
        chat_conversation_id: &str,
    ) -> Result<Option<(String, String, String)>> {
        self.connection.query_row(
            "SELECT project_id,group_conversation_id,agent_id FROM project_agent_chats WHERE conversation_id=?1",
            [chat_conversation_id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
        ).optional().map_err(|_| "项目 Agent 会话归属读取失败".into())
    }

    /// Each project gets a hidden direct conversation per connected Windows member.
    /// It uses the normal harness/run storage but has its own CLI session and project cwd.
    pub fn ensure_project_agent_chats(&mut self, group_id: &str) -> Result<Vec<ProjectAgentChat>> {
        let group = self.conversation(group_id)?;
        if group.kind != "group" {
            return Ok(Vec::new());
        }
        let Some(project) = self.conversation_project(group_id)? else {
            return Ok(Vec::new());
        };
        for agent in EXECUTOR_AGENTS {
            if !group.members.iter().any(|member| member == agent) {
                continue;
            }
            let exists: bool = self.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM project_agent_chats WHERE project_id=?1 AND group_conversation_id=?2 AND agent_id=?3)",
                params![project.id,group_id,agent], |row| row.get(0),
            ).map_err(|_| "Agent 会话状态不可用")?;
            if exists {
                continue;
            }
            let title = format!("{} · {}", project.name, crate::store::agent_name(agent));
            let chat = self.create(&title, "direct", &[agent.to_owned()])?;
            let saved_settings: (Option<String>, Option<String>) = self.connection.query_row(
                "SELECT s.model,s.reasoning_effort FROM sessions s JOIN conversations c ON c.id=s.conversation_id WHERE s.agent_id=?1 AND c.kind='direct' AND c.id<>?2 ORDER BY c.updated_at DESC LIMIT 1",
                params![agent, chat.id], |row| Ok((row.get(0)?,row.get(1)?)),
            ).optional().map_err(|_| "Agent 会话设置读取失败")?.unwrap_or((None,None));
            let tx = self
                .connection
                .transaction()
                .map_err(|_| "Agent 会话创建失败")?;
            tx.execute(
                "UPDATE sessions SET native_cwd=?1,model=?2,reasoning_effort=?3 WHERE conversation_id=?4 AND agent_id=?5",
                params![project.root,saved_settings.0,saved_settings.1,chat.id,agent],
            ).map_err(|_| "Agent 项目目录绑定失败")?;
            tx.execute(
                "INSERT INTO project_agent_chats(project_id,group_conversation_id,agent_id,conversation_id,created_at) VALUES(?1,?2,?3,?4,?5)",
                params![project.id,group_id,agent,chat.id,now()],
            ).map_err(|_| "Agent 项目会话绑定失败")?;
            tx.commit().map_err(|_| "Agent 项目会话保存失败")?;
        }
        self.project_agent_chats(group_id)
    }

    pub fn agent_messages(&self, workflow_id: &str) -> Result<Vec<AgentMessage>> {
        let mut statement = self
            .connection
            .prepare("SELECT id,workflow_id,attempt_id,sender_agent,recipient_agent,content,created_at FROM project_agent_messages WHERE workflow_id=?1 ORDER BY created_at,id")
            .map_err(|_| "项目成员消息读取失败")?;
        let rows = statement
            .query_map([workflow_id], |row| {
                Ok(AgentMessage {
                    id: row.get(0)?,
                    workflow_id: row.get(1)?,
                    attempt_id: row.get(2)?,
                    sender_agent: row.get(3)?,
                    recipient_agent: row.get(4)?,
                    content: row.get(5)?,
                    created_at: row.get(6)?,
                })
            })
            .map_err(|_| "项目成员消息读取失败")?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| "项目成员消息读取失败".into())
    }

    /// 单独读一个任务：任务级 model/effort/worktree/branch 都在这里带出。
    pub fn task(&self, id: &str) -> Result<Task> {
        self.connection
            .query_row(
                &format!("SELECT {TASK_COLUMNS} FROM project_tasks WHERE id=?1"),
                [id],
                task_from_row,
            )
            .optional()
            .map_err(|e| e.to_string())?
            .ok_or("实现任务不存在".into())
    }

    /// 写任务级覆盖：传 None（或空白字符串）就是清除，写回 NULL。
    pub fn set_task_config(
        &mut self,
        task_id: &str,
        model: Option<String>,
        effort: Option<String>,
        worktree: Option<String>,
        branch: Option<String>,
    ) -> Result<Task> {
        let model = optional_text(model, 200, "任务模型")?;
        let effort = optional_text(effort, 40, "任务思考强度")?;
        let worktree = optional_text(worktree, 1000, "任务工作树路径")?;
        let branch = optional_text(branch, 300, "任务分支名")?;
        let changed = self
            .connection
            .execute(
                "UPDATE project_tasks SET model=?1,effort=?2,worktree=?3,branch=?4 WHERE id=?5",
                params![model, effort, worktree, branch, task_id],
            )
            .map_err(|e| e.to_string())?;
        if changed == 0 {
            return Err("实现任务不存在".into());
        }
        self.task(task_id)
    }

    /// Change a queued task's executor while its workflow is still waiting for user confirmation.
    pub fn set_task_agent(&mut self, task_id: &str, agent: &str) -> Result<Task> {
        if !EXECUTOR_AGENTS.contains(&agent) {
            return Err("实现成员只能选择 Codex、Hermes、DSH 或 Claude".into());
        }
        let task = self.task(task_id)?;
        let workflow = self.workflow(&task.workflow_id)?;
        if workflow.status != "planning" || task.status != "queued" {
            return Err("只能在确认阶段修改待执行任务的成员".into());
        }
        let update = if agent == "hermes-win" {
            self.connection.execute(
                "UPDATE project_tasks SET assigned_agent='hermes-win' WHERE id=?1",
                [task_id],
            )
        } else {
            self.connection.execute(
                "UPDATE project_tasks SET agent_id=?1,assigned_agent=NULL WHERE id=?2",
                params![agent, task_id],
            )
        };
        update.map_err(|e| e.to_string())?;
        self.task(task_id)
    }

    /// 写这次工作流的角色配置：必须是一个 JSON 对象（规划/执行/验收的成员+模型+强度）。
    pub fn set_workflow_roles(&mut self, id: &str, roles: Option<&str>) -> Result<Workflow> {
        self.workflow(id)?;
        let roles = match roles {
            None => None,
            Some(text) => {
                if text.len() > 4000 {
                    return Err("角色配置过长".into());
                }
                let value: serde_json::Value =
                    serde_json::from_str(text).map_err(|_| "角色配置必须是 JSON 对象")?;
                if !value.is_object() {
                    return Err("角色配置必须是 JSON 对象".into());
                }
                Some(text.to_owned())
            }
        };
        self.connection
            .execute(
                "UPDATE workflows SET roles=?1,updated_at=?2 WHERE id=?3",
                params![roles, now(), id],
            )
            .map_err(|e| e.to_string())?;
        self.workflow(id)
    }

    pub fn update_paused_workflow_roles(&mut self, id: &str, roles: &str) -> Result<Workflow> {
        let workflow = self.workflow(id)?;
        if !matches!(workflow.status.as_str(), "failed" | "interrupted") || workflow.plan.is_none()
        {
            return Err("只能调整已暂停且保留原方案的协作".into());
        }
        let active: bool = self
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM project_attempts WHERE workflow_id=?1 AND status IN ('starting','running','cancelling'))",
                [id],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        if active {
            return Err("项目成员仍在运行，暂时不能调整阶段设置".into());
        }
        let roles = Roles::parse(Some(roles))?;
        if !EXECUTOR_AGENTS.contains(&roles.implement.agent.as_str()) {
            return Err("执行角色只能选 Codex、Hermes、DSH 或 Claude".into());
        }
        let encoded_roles = serde_json::to_string(&roles).map_err(|_| "角色配置序列化失败")?;
        let tx = self.connection.transaction().map_err(|e| e.to_string())?;
        tx.execute(
            "UPDATE workflows SET roles=?1,updated_at=?2 WHERE id=?3",
            params![encoded_roles, now(), id],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        self.workflow(id)
    }

    pub fn workflows(&self, room: &str) -> Result<Vec<Workflow>> {
        let mut stmt=self.connection.prepare("SELECT id FROM workflows WHERE conversation_id=?1 ORDER BY created_at DESC,rowid DESC").map_err(|e|e.to_string())?;
        let ids = stmt
            .query_map([room], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        ids.iter().map(|id| self.workflow(id)).collect()
    }
    pub fn active_workflows(&self) -> Result<Vec<Workflow>> {
        let mut stmt=self.connection.prepare("SELECT id FROM workflows WHERE status IN ('queued','planning','running','verifying','reviewing','cancelling') ORDER BY created_at,rowid").map_err(|e|e.to_string())?;
        let ids = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        ids.iter().map(|id| self.workflow(id)).collect()
    }
    pub fn set_project_summary(&mut self, id: &str, enabled: bool) -> Result<Project> {
        self.project(id)?;
        self.connection
            .execute(
                "UPDATE projects SET summary_enabled=?1 WHERE id=?2",
                params![enabled, id],
            )
            .map_err(|e| e.to_string())?;
        self.project(id)
    }
    pub fn shared_workflows(&self) -> Result<Vec<Workflow>> {
        let mut active = self
            .connection
            .prepare("SELECT w.id FROM workflows w JOIN projects p ON p.id=w.project_id WHERE p.summary_enabled=1 AND w.status IN ('queued','planning','running','verifying','reviewing','cancelling') ORDER BY w.updated_at DESC,w.id DESC LIMIT 2")
            .map_err(|e| e.to_string())?;
        let mut ids = active
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        let mut finished = self
            .connection
            .prepare("SELECT w.id FROM workflows w JOIN projects p ON p.id=w.project_id WHERE p.summary_enabled=1 AND w.status IN ('completed','failed','interrupted') ORDER BY w.updated_at DESC,w.id DESC LIMIT 3")
            .map_err(|e| e.to_string())?;
        let done = finished
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        ids.extend(done);
        ids.iter().map(|id| self.workflow(id)).collect()
    }
    pub fn acquire_project(&mut self, id: &str) -> Result<bool> {
        let workflow = self.workflow(id)?;
        if workflow.status != "queued" {
            return Err("项目协作不在排队状态".into());
        }
        let project = self.project(&workflow.project_id)?;
        let root = Path::new(&project.root)
            .canonicalize()
            .map_err(|_| "项目目录不可用")?;
        let root_key = key(&root);
        let mut stmt = self
            .connection
            .prepare("SELECT root_key FROM project_leases")
            .map_err(|e| e.to_string())?;
        let keys = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        drop(stmt);
        if keys.iter().any(|other| overlaps(&root_key, other)) {
            return Ok(false);
        }
        let tx = self.connection.transaction().map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO project_leases VALUES(?1,?2,?3,?4)",
            params![id, project.id, root_key, now()],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "UPDATE workflows SET status='planning',updated_at=?1 WHERE id=?2",
            params![now(), id],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(true)
    }
    pub fn save_plan(&mut self, id: &str, plan: &Plan) -> Result<Workflow> {
        validate_plan(plan)?;
        let workflow = self.workflow(id)?;
        if workflow.status != "planning" || !workflow.tasks.is_empty() {
            return Err("当前协作不能保存新分发结果".into());
        }
        let attempts = self.attempts(id)?;
        if attempts.iter().any(|attempt| {
            matches!(
                attempt.status.as_str(),
                "starting" | "running" | "cancelling"
            )
        }) {
            return Err("项目成员尚未停止，不能保存分发结果".into());
        }
        let plans = attempts
            .iter()
            .filter(|attempt| {
                attempt.stage == "plan" && attempt.activity.as_deref() != Some("discussion")
            })
            .collect::<Vec<_>>();
        if plans.len() != 1
            || plans[0].status != "completed"
            || parse_plan(&plans[0].output).is_err()
        {
            return Err("保存方案前需要已完成的规划结果".into());
        }
        let planned_output = parse_plan(&plans[0].output)?;
        if planned_output != *plan {
            return Err("保存的方案不能改变规划给出的任务或实现要求".into());
        }
        let plan_output = serde_json::to_string(plan).map_err(|e| e.to_string())?;
        let plan_attempt_id = plans[0].id.clone();
        let project = self.project(&workflow.project_id)?;
        for task in &plan.tasks {
            for file in &task.files {
                task_path(Path::new(&project.root), file)?;
            }
        }
        let tx = self.connection.transaction().map_err(|e| e.to_string())?;
        let current: (String, i64) = tx
            .query_row(
                "SELECT status,(SELECT count(*) FROM project_tasks WHERE workflow_id=?1) FROM workflows WHERE id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|e| e.to_string())?;
        let active: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM project_attempts WHERE workflow_id=?1 AND status IN ('starting','running','cancelling'))",
                [id],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        if current.0 != "planning" || current.1 != 0 || active {
            return Err("协作状态已变化或项目成员仍在运行，不能保存方案".into());
        }
        tx.execute(
            "UPDATE project_attempts SET output=?1 WHERE id=?2",
            params![plan_output, plan_attempt_id],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "UPDATE workflows SET plan=?1,updated_at=?2 WHERE id=?3",
            params![
                serde_json::to_string(&plan).map_err(|e| e.to_string())?,
                now(),
                id
            ],
        )
        .map_err(|e| e.to_string())?;
        let fallback = roles_of(&workflow)?.implement.agent;
        for (index, task) in plan.tasks.iter().enumerate() {
            let (stored_agent, assigned_agent) = if task.agent_id == "hermes-win" {
                let stored = if TASK_ROW_BASE_AGENTS.contains(&fallback.as_str()) {
                    fallback.as_str()
                } else {
                    "dsh-win"
                };
                (stored, Some("hermes-win"))
            } else {
                (task.agent_id.as_str(), None)
            };
            tx.execute("INSERT INTO project_tasks(id,workflow_id,position,title,agent_id,instructions,files,depends_on,status,assigned_agent) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'queued',?9)",params![Uuid::new_v4().to_string(),id,index as u32,task.title,stored_agent,task.instructions,serde_json::to_string(&task.files).map_err(|e|e.to_string())?,serde_json::to_string(&task.depends_on).map_err(|e|e.to_string())?,assigned_agent]).map_err(|e|e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        self.workflow(id)
    }

    /// Reopen a failed workflow from its stored plan output without calling a planner again.
    pub fn resume_workflow(&mut self, id: &str, plan: &Plan) -> Result<Workflow> {
        validate_plan(plan)?;
        let workflow = self.workflow(id)?;
        if !matches!(workflow.status.as_str(), "failed" | "interrupted") {
            return Err("只能继续失败或中断的协作".into());
        }
        if workflow.plan.as_ref().is_some_and(|saved| saved != plan) {
            return Err("继续协作不能更改原方案".into());
        }
        let attempts = self.attempts(id)?;
        if attempts.iter().any(|attempt| {
            matches!(
                attempt.status.as_str(),
                "starting" | "running" | "cancelling"
            )
        }) {
            return Err("项目成员尚未停止，不能继续协作".into());
        }
        let planning = attempts
            .iter()
            .filter(|attempt| {
                attempt.stage == "plan" && attempt.activity.as_deref() != Some("discussion")
            })
            .collect::<Vec<_>>();
        if planning.len() != 1
            || planning[0].status != "completed"
            || parse_plan(&planning[0].output)? != *plan
        {
            return Err("找不到可复用的已完成方案，请重新规划".into());
        }

        let project = self.project(&workflow.project_id)?;
        for task in &plan.tasks {
            for file in &task.files {
                task_path(Path::new(&project.root), file)?;
            }
        }
        let root = Path::new(&project.root)
            .canonicalize()
            .map_err(|_| "项目目录不可用")?;
        let root_key = key(&root);
        let mut stmt = self
            .connection
            .prepare("SELECT root_key FROM project_leases")
            .map_err(|e| e.to_string())?;
        let leases = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        drop(stmt);
        if leases.iter().any(|other| overlaps(&root_key, other)) {
            return Err("项目正被另一项协作占用，请稍后继续".into());
        }
        if !workflow.tasks.is_empty()
            && (workflow.tasks.len() != plan.tasks.len()
                || workflow.tasks.iter().zip(&plan.tasks).enumerate().any(
                    |(position, (saved, planned))| {
                        saved.position as usize != position
                            || saved.title != planned.title
                            || saved.instructions != planned.instructions
                            || saved.files != planned.files
                            || saved.depends_on != planned.depends_on
                    },
                ))
        {
            return Err("已保存任务与原方案不一致，不能安全继续".into());
        }

        let tx = self.connection.transaction().map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO project_leases VALUES(?1,?2,?3,?4)",
            params![id, project.id, root_key, now()],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "UPDATE workflows SET status='planning',plan=?1,summary='',error=NULL,updated_at=?2 WHERE id=?3",
            params![serde_json::to_string(plan).map_err(|e| e.to_string())?, now(), id],
        )
        .map_err(|e| e.to_string())?;
        let task_count: i64 = tx
            .query_row(
                "SELECT count(*) FROM project_tasks WHERE workflow_id=?1",
                [id],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        if task_count == 0 {
            tx.execute(
                "UPDATE messages SET status='pending' WHERE id=?1",
                [&workflow.user_message_id],
            )
            .map_err(|e| e.to_string())?;
            tx.commit().map_err(|e| e.to_string())?;
            let saved = self.save_plan(id, plan)?;
            // 纯答复方案（tasks 为空）没有任何可确认/可执行的任务，继续协作必须和首次规划
            // （projects::plan_work 的同名分支）落到同一个终态；否则工作流会停在 planning，
            // 界面上唯一的按钮「确认并开始执行」必然报错，用户只能再停一次，永远收不了尾。
            if plan.tasks.is_empty() {
                return self.finish_workflow(id, "completed", &plan.summary, None);
            }
            return Ok(saved);
        }
        // 失败的任务也保留工作树和分支：常见的失败是成员已经做完、只是合并没成功，
        // 继续协作时在原分支上接着做，不丢已经提交的改动、也不从头再来。
        tx.execute(
            "UPDATE project_tasks SET status=CASE WHEN status='completed' THEN 'completed' ELSE 'queued' END,output=CASE WHEN status='completed' THEN output ELSE '' END,error=CASE WHEN status='completed' THEN error ELSE NULL END,worktree=CASE WHEN status IN ('completed','interrupted','failed') THEN worktree ELSE NULL END,branch=CASE WHEN status IN ('completed','interrupted','failed') THEN branch ELSE NULL END WHERE workflow_id=?1",
            [id],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "UPDATE messages SET status='pending' WHERE id=?1",
            [&workflow.user_message_id],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        self.workflow(id)
    }

    /// 用户确认任务参数后把工作流从待确认（planning）提到执行中（running）。
    /// 保存方案本身不改状态：状态停在 planning 就是「等你确认」。
    pub fn begin_execution(&mut self, id: &str) -> Result<Workflow> {
        let workflow = self.workflow(id)?;
        if workflow.status != "planning" {
            return Err("当前协作不在待确认阶段，不能开始执行".into());
        }
        if workflow.tasks.is_empty() {
            return Err("方案还没有可执行的任务".into());
        }
        let active: bool = self
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM project_attempts WHERE workflow_id=?1 AND status IN ('starting','running','cancelling'))",
                [id],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        if active {
            return Err("项目成员尚未停止，不能开始执行".into());
        }
        self.connection
            .execute(
                "UPDATE workflows SET status='running',updated_at=?1 WHERE id=?2 AND status='planning'",
                params![now(), id],
            )
            .map_err(|e| e.to_string())?;
        self.workflow(id)
    }
    pub fn cancel_workflow(&mut self, id: &str) -> Result<Workflow> {
        let workflow = self.workflow(id)?;
        if live(&workflow.status) {
            self.connection
                .execute(
                    "UPDATE workflows SET status='cancelling',updated_at=?1 WHERE id=?2",
                    params![now(), id],
                )
                .map_err(|e| e.to_string())?;
            let active: bool = self
                .connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM project_attempts WHERE workflow_id=?1 AND status IN ('starting','running','cancelling'))",
                    [id],
                    |row| row.get(0),
                )
                .map_err(|e| e.to_string())?;
            if !active {
                return self.finish_workflow(id, "interrupted", "", Some("用户停止了协作"));
            }
        }
        self.workflow(id)
    }
    pub fn finish_workflow(
        &mut self,
        id: &str,
        status: &str,
        summary: &str,
        error: Option<&str>,
    ) -> Result<Workflow> {
        if !matches!(status, "completed" | "failed" | "interrupted") {
            return Err("协作结束状态无效".into());
        }
        let workflow = self.workflow(id)?;
        if !live(&workflow.status) {
            return Ok(workflow);
        }
        let active=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM project_attempts WHERE workflow_id=?1 AND status IN ('starting','running','cancelling'))",[id],|r|r.get::<_,bool>(0)).map_err(|e|e.to_string())?;
        if active {
            return Err("项目成员尚未停止，不能释放写入租约".into());
        }
        if status == "completed" {
            let reply_only = workflow.tasks.is_empty()
                && workflow
                    .plan
                    .as_ref()
                    .is_some_and(|plan| plan.tasks.is_empty())
                && workflow
                    .attempts
                    .iter()
                    .rev()
                    .find(|attempt| {
                        attempt.stage == "plan" && attempt.activity.as_deref() != Some("discussion")
                    })
                    .is_some_and(|attempt| {
                        attempt.status == "completed"
                            && parse_plan(&attempt.output).is_ok_and(|plan| plan.tasks.is_empty())
                    });
            let implementation_complete = workflow.status == "reviewing"
                && !workflow.tasks.is_empty()
                && workflow.tasks.iter().all(|t| t.status == "completed")
                && workflow
                    .attempts
                    .iter()
                    .rev()
                    .find(|attempt| attempt.stage == "review")
                    .is_some_and(|attempt| {
                        attempt.status == "completed"
                            && parse_review(&attempt.output).is_ok_and(|review| review.approved)
                    });
            let coordinated_complete = workflow.coordination.is_some()
                && !workflow.tasks.is_empty()
                && workflow
                    .tasks
                    .iter()
                    .all(|task| matches!(task.status.as_str(), "completed" | "skipped"));
            if !(reply_only || implementation_complete || coordinated_complete) {
                return Err("实现与验收尚未完成".into());
            }
        }
        let tx = self.connection.transaction().map_err(|e| e.to_string())?;
        tx.execute(
            "UPDATE workflows SET status=?1,summary=?2,error=?3,updated_at=?4 WHERE id=?5",
            params![status, summary, error, now(), id],
        )
        .map_err(|e| e.to_string())?;
        if status == "completed" && !summary.trim().is_empty() && workflow.coordination.is_none() {
            let final_attempt = workflow.attempts.iter().rev().find(|attempt| {
                attempt.status == "completed" && matches!(attempt.stage.as_str(), "review" | "plan")
            });
            if let Some(attempt) = final_attempt {
                tx.execute("INSERT OR IGNORE INTO project_team_messages(id,project_id,conversation_id,workflow_id,attempt_id,sender_agent,message_kind,content,created_at) VALUES(?1,?2,?3,?4,?5,?6,'agent',?7,?8)",params![format!("result:{id}"),workflow.project_id,workflow.conversation_id,id,attempt.id,attempt.agent_id,summary,now()]).map_err(|_|"团队结果发布失败")?;
            }
        }
        if status != "interrupted" && workflow.coordination.is_none() {
            tx.execute(
                "UPDATE project_tasks SET status='skipped' WHERE workflow_id=?1 AND status='queued'",
                [id],
            )
            .map_err(|e| e.to_string())?;
        }
        tx.execute("DELETE FROM project_leases WHERE workflow_id=?1", [id])
            .map_err(|e| e.to_string())?;
        tx.execute(
            "UPDATE messages SET status=?1 WHERE id=?2 AND status='pending'",
            params![
                if status == "completed" {
                    "delivered"
                } else {
                    "failed"
                },
                workflow.user_message_id
            ],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        self.workflow(id)
    }
    pub fn recover_projects(&mut self) -> Result<()> {
        self.connection.execute("UPDATE project_questions SET status='cancelled',answered_at=?1 WHERE status='pending'", [now()]).map_err(|e| e.to_string())?;
        self.connection.execute_batch("UPDATE project_attempts SET status='interrupted',error='上次项目运行因退出中断，不会自动重放' WHERE status IN ('starting','running','cancelling');
          UPDATE project_tasks SET status='interrupted',error='上次任务因退出中断' WHERE status='running';
          UPDATE messages SET status='failed' WHERE status='pending' AND id IN (SELECT user_message_id FROM workflows WHERE status IN ('queued','planning','running','verifying','reviewing','cancelling'));
          UPDATE workflows SET status='interrupted',error='上次协作因软件退出中断，不会自动重放' WHERE status IN ('queued','planning','running','verifying','reviewing','cancelling');
          UPDATE coordination_runs SET status='paused' WHERE workflow_id IN (SELECT id FROM workflows WHERE status='interrupted');
          DELETE FROM project_leases;")
          .map_err(|e|e.to_string())
    }

    pub fn attempts(&self, workflow: &str) -> Result<Vec<Attempt>> {
        let mut stmt=self.connection.prepare("SELECT id,workflow_id,task_id,agent_id,stage,status,native_thread_id,native_turn_id,model,reasoning_effort,thought,output,error,created_at,activity FROM project_attempts WHERE workflow_id=?1 ORDER BY rowid").map_err(|e|e.to_string())?;
        let result = stmt
            .query_map([workflow], |r| {
                Ok(Attempt {
                    id: r.get(0)?,
                    created_at: r.get(13)?,
                    activity: r.get(14)?,
                    workflow_id: r.get(1)?,
                    task_id: r.get(2)?,
                    agent_id: r.get(3)?,
                    stage: r.get(4)?,
                    status: r.get(5)?,
                    native_thread_id: r.get(6)?,
                    native_turn_id: r.get(7)?,
                    model: r.get(8)?,
                    reasoning_effort: r.get(9)?,
                    thought: r.get(10)?,
                    tool_events: Vec::new(),
                    output: r.get(11)?,
                    error: r.get(12)?,
                })
            })
            .map_err(|e| e.to_string())?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| e.to_string());
        let mut attempts = result?;
        for attempt in &mut attempts {
            attempt.tool_events = self.attempt_tool_events(&attempt.id)?;
        }
        Ok(attempts)
    }

    fn attempt_tool_events(&self, attempt_id: &str) -> Result<Vec<crate::store::RunToolEvent>> {
        let mut statement = self.connection.prepare(
            "SELECT event_id,kind,title,input,output,status,created_at FROM project_attempt_events WHERE attempt_id=?1 ORDER BY created_at,event_id",
        ).map_err(|e| e.to_string())?;
        let rows = statement
            .query_map([attempt_id], |row| {
                Ok(crate::store::RunToolEvent {
                    event_id: row.get(0)?,
                    kind: row.get(1)?,
                    title: row.get(2)?,
                    input: row.get(3)?,
                    output: row.get(4)?,
                    status: row.get(5)?,
                    created_at: row.get(6)?,
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    pub fn attempt(&self, id: &str) -> Result<Attempt> {
        let workflow = self
            .connection
            .query_row(
                "SELECT workflow_id FROM project_attempts WHERE id=?1",
                [id],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| e.to_string())?
            .ok_or("项目运行不存在")?;
        self.attempts(&workflow)?
            .into_iter()
            .find(|a| a.id == id)
            .ok_or("项目运行不存在".into())
    }

    pub fn begin_attempt(
        &mut self,
        workflow_id: &str,
        task_id: Option<&str>,
        agent: &str,
        stage: &str,
        model: Option<String>,
        effort: Option<String>,
    ) -> Result<Attempt> {
        let workflow = self.workflow(workflow_id)?;
        let lease = self
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM project_leases WHERE workflow_id=?1)",
                [workflow_id],
                |r| r.get::<_, bool>(0),
            )
            .map_err(|e| e.to_string())?;
        if !lease || workflow.status == "cancelling" || !live(&workflow.status) {
            return Err("项目写入租约不可用或任务已停止".into());
        }
        let attempts = self.attempts(workflow_id)?;
        // 独占按任务：同一条任务才互斥，别的任务（并行实现）互不影响；
        // 阶段级步骤 task_id 为空时仍与任何活动 attempt 互斥。
        let blocked = attempts
            .iter()
            .filter(|a| matches!(a.status.as_str(), "starting" | "running" | "cancelling"))
            .any(|a| match (a.task_id.as_deref(), task_id) {
                (Some(running), Some(now)) => running == now,
                _ => true,
            });
        if blocked {
            return Err("上一项项目运行尚未完成".into());
        }
        self.guard_service_maintenance(agent)?;
        let chatting=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM runs WHERE agent_id=?1 AND status IN ('starting','running','cancelling'))",[agent],|r|r.get::<_,bool>(0)).map_err(|e|e.to_string())?;
        if chatting {
            return Err("该成员正在会话中回复，请等待完成".into());
        }
        // 阶段与成员的对应关系由这次工作流的角色配置决定，不再写死。
        // 同一成员并发跑多个任务由 attempt 级独占（上面的「上一项项目运行尚未完成」）
        // 在下一批放开；每个 attempt 本来就有自己的原生子进程。
        let roles = roles_of(&workflow)?;
        let mut next = workflow.status.as_str();
        match stage {
            "discussion"
                if EXECUTOR_AGENTS.contains(&agent)
                    && task_id.is_none()
                    && workflow.status == "planning"
                    && self
                        .conversation(&workflow.conversation_id)?
                        .members
                        .iter()
                        .any(|member| member == agent) => {}
            // 规划：规划角色一个人做完（不再有 Hermes 分发这一步）。
            "plan"
                if agent == roles.plan.agent
                    && task_id.is_none()
                    && workflow.status == "planning"
                    && !attempts.iter().any(|a| {
                        a.stage == "plan" && a.activity.as_deref() != Some("discussion")
                    }) => {}
            // 实现：成员由用户逐任务确认；仍只允许 Codex/DSH，且必须匹配该任务的已确认成员。
            "implement" if EXECUTOR_AGENTS.contains(&agent) && workflow.status == "running" => {
                let task = workflow
                    .tasks
                    .iter()
                    .find(|t| Some(t.id.as_str()) == task_id)
                    .ok_or("实现任务不存在")?;
                if task.agent_id != agent
                    || task.status != "queued"
                    || task.depends_on.iter().any(|i| {
                        workflow
                            .tasks
                            .get(*i as usize)
                            .is_none_or(|t| t.status != "completed")
                    })
                {
                    return Err("任务成员、状态或前置依赖尚不满足".into());
                }
            }
            // 功能验收由选定 agent 直接对照需求和授权源码完成。
            "review"
                if agent == roles.review.agent
                    && task_id.is_none()
                    && matches!(workflow.status.as_str(), "running" | "reviewing")
                    && !workflow.tasks.is_empty()
                    && workflow.tasks.iter().all(|t| t.status == "completed") =>
            {
                next = "reviewing";
            }
            // 验收给出具体问题后，由执行角色修复一次再复核。
            "repair"
                if agent == roles.implement.agent
                    && task_id.is_none()
                    && workflow.status == "reviewing"
                    // Failed/interrupted transports did not perform the bounded repair and may be
                    // retried after changing the stage member/model; only a completed repair uses it.
                    && !attempts
                        .iter()
                        .any(|a| a.stage == "repair" && a.status == "completed")
                    && attempts
                        .iter()
                        .rev()
                        .find(|a| a.stage == "review")
                        .is_some_and(|a| {
                            a.status == "completed"
                                && parse_review(&a.output).is_ok_and(|review| !review.approved)
                        }) =>
            {
                next = "running";
            }
            _ => return Err("项目运行阶段或成员无效".into()),
        }
        let id = Uuid::new_v4().to_string();
        let tx = self.connection.transaction().map_err(|e| e.to_string())?;
        let activity = (stage == "discussion").then_some("discussion");
        let stage = if activity.is_some() { "plan" } else { stage };
        tx.execute("INSERT INTO project_attempts(id,workflow_id,task_id,agent_id,stage,status,model,reasoning_effort,created_at,activity) VALUES(?1,?2,?3,?4,?5,'starting',?6,?7,?8,?9)",params![id,workflow_id,task_id,agent,stage,model,effort,now(),activity]).map_err(|e|e.to_string())?;
        tx.execute(
            "UPDATE workflows SET status=?1,updated_at=?2 WHERE id=?3",
            params![next, now(), workflow_id],
        )
        .map_err(|e| e.to_string())?;
        if let Some(task) = task_id {
            tx.execute(
                "UPDATE project_tasks SET status='running' WHERE id=?1",
                [task],
            )
            .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        self.attempt(&id)
    }

    pub fn cancel_agent_attempts(
        &mut self,
        workflow_id: &str,
        agent_id: &str,
    ) -> Result<Vec<String>> {
        if !EXECUTOR_AGENTS.contains(&agent_id) {
            return Err("未知的项目 Agent".into());
        }
        let workflow = self.workflow(workflow_id)?;
        if !matches!(workflow.status.as_str(), "running" | "reviewing") {
            return Err("当前协作没有可单独中断的 Agent".into());
        }
        let attempts = self.attempts(workflow_id)?;
        let ids = attempts
            .into_iter()
            .filter(|attempt| {
                attempt.agent_id == agent_id
                    && attempt.activity.as_deref()
                        != Some(crate::coordinator_policy::MANAGER_ACTIVITY)
                    && matches!(
                        attempt.status.as_str(),
                        "starting" | "running" | "cancelling"
                    )
            })
            .map(|attempt| attempt.id)
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return Err("该 Agent 当前没有运行中的项目任务".into());
        }
        let tx = self.connection.transaction().map_err(|e| e.to_string())?;
        for id in &ids {
            tx.execute("UPDATE project_attempts SET status='cancelling',error='用户正在单独中断此 Agent' WHERE id=?1 AND status IN ('starting','running','cancelling')",[id]).map_err(|e|e.to_string())?;
        }
        tx.execute(
            "UPDATE workflows SET updated_at=?1 WHERE id=?2",
            params![now(), workflow_id],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(ids)
    }

    pub fn checkpoint_attempt(&mut self, attempt: &Attempt) -> Result<()> {
        if !matches!(
            attempt.status.as_str(),
            "starting" | "running" | "cancelling" | "completed" | "failed" | "interrupted"
        ) {
            return Err("项目运行状态无效".into());
        }
        let old = self.attempt(&attempt.id)?;
        if old.workflow_id != attempt.workflow_id
            || old.task_id != attempt.task_id
            || old.agent_id != attempt.agent_id
            || old.stage != attempt.stage
            || old.model != attempt.model
            || old.reasoning_effort != attempt.reasoning_effort
        {
            return Err("项目运行身份不匹配".into());
        }
        if !matches!(old.status.as_str(), "starting" | "running" | "cancelling") {
            return Err("项目运行已经结束".into());
        }
        let tx = self.connection.transaction().map_err(|e| e.to_string())?;
        tx.execute("UPDATE project_attempts SET status=?1,native_thread_id=?2,native_turn_id=?3,model=?4,reasoning_effort=?5,thought=?6,output=?7,error=?8 WHERE id=?9",params![attempt.status,attempt.native_thread_id,attempt.native_turn_id,attempt.model,attempt.reasoning_effort,attempt.thought,attempt.output,attempt.error,attempt.id]).map_err(|e|e.to_string())?;
        if attempt.native_turn_id.is_some() {
            tx.execute("UPDATE messages SET status='delivered' WHERE id=(SELECT user_message_id FROM workflows WHERE id=?1) AND status='pending'", [&attempt.workflow_id]).map_err(|e|e.to_string())?;
        }
        if let Some(task) = attempt.task_id.as_ref() {
            let state = if matches!(
                attempt.status.as_str(),
                "starting" | "running" | "cancelling"
            ) || (attempt.status == "completed"
                && attempt
                    .activity
                    .as_deref()
                    .is_some_and(|value| value.starts_with("task:")))
            {
                // Coordinated output is complete only after its commit/merge succeeds.
                "running"
            } else {
                &attempt.status
            };
            tx.execute(
                "UPDATE project_tasks SET status=?1,output=?2,error=?3 WHERE id=?4",
                params![state, attempt.output, attempt.error, task],
            )
            .map_err(|e| e.to_string())?;
        }
        for event in &attempt.tool_events {
            tx.execute(
                "INSERT INTO project_attempt_events(attempt_id,event_id,kind,title,input,output,status,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(attempt_id,event_id) DO UPDATE SET kind=excluded.kind,title=excluded.title,input=excluded.input,output=excluded.output,status=excluded.status",
                params![attempt.id,event.event_id,event.kind,event.title,event.input,event.output,event.status,event.created_at],
            ).map_err(|e| e.to_string())?;
        }
        tx.execute(
            "UPDATE workflows SET updated_at=?1 WHERE id=?2",
            params![now(), attempt.workflow_id],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn memory() -> Store {
        Store::initialize(
            Connection::open_in_memory().unwrap(),
            PathBuf::from(":memory:"),
        )
        .unwrap()
    }

    fn columns(store: &Store, table: &str) -> Vec<String> {
        let mut statement = store
            .connection
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap();
        statement
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(std::result::Result::ok)
            .collect()
    }

    fn version(store: &Store) -> i64 {
        store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }

    /// 造一条 v9 时代的工作流与任务：不跑流水线，只保证老数据齐备。
    fn legacy_workflow(store: &mut Store) -> (String, String) {
        let project_root = std::env::temp_dir().join(format!("hub-v10-proj-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&project_root).unwrap();
        let project = store
            .register_project("项目", project_root.to_str().unwrap())
            .unwrap();
        let room = store
            .create("协作", "direct", &["codex-win".into()])
            .unwrap();
        let message = Uuid::new_v4().to_string();
        store.save_message(&room.id, &message, "旧需求").unwrap();
        let workflow = Uuid::new_v4().to_string();
        let timestamp = now();
        store.connection.execute("INSERT INTO workflows(id,project_id,conversation_id,user_message_id,request,status,plan,summary,error,created_at,updated_at) VALUES(?1,?2,?3,?4,'旧需求','completed',NULL,'',NULL,?5,?5)", params![workflow, project.id, room.id, message, timestamp]).unwrap();
        let task = Uuid::new_v4().to_string();
        store.connection.execute("INSERT INTO project_tasks(id,workflow_id,position,title,agent_id,instructions,files,depends_on,status,output,error) VALUES(?1,?2,0,'旧任务','dsh-win','按方案实现','[\"src/lib.rs\"]','[]','queued','',NULL)", params![task, workflow]).unwrap();
        (workflow, task)
    }

    #[test]
    fn v11_upgrade_adds_project_thought_and_keeps_legacy_rows() {
        let mut store = memory();
        let (workflow, task) = legacy_workflow(&mut store);
        // 把库退回 v9 的样子：新列全去掉，版本号也退回。
        store
            .connection
            .execute_batch(
                "ALTER TABLE workflows DROP COLUMN roles;
                 ALTER TABLE project_tasks DROP COLUMN model;
                 ALTER TABLE project_tasks DROP COLUMN effort;
                 ALTER TABLE project_tasks DROP COLUMN worktree;
                 ALTER TABLE project_tasks DROP COLUMN branch;
                 ALTER TABLE project_tasks DROP COLUMN assigned_agent;
                 ALTER TABLE project_attempts DROP COLUMN thought;
                 PRAGMA user_version=9;",
            )
            .unwrap();
        assert!(!columns(&store, "workflows").iter().any(|c| c == "roles"));
        let store = Store::initialize(store.connection, PathBuf::from(":memory:")).unwrap();
        assert_eq!(version(&store), crate::store::SCHEMA_VERSION);
        assert!(columns(&store, "workflows").iter().any(|c| c == "roles"));
        assert!(columns(&store, "project_attempts")
            .iter()
            .any(|c| c == "thought"));
        for column in ["model", "effort", "worktree", "branch", "assigned_agent"] {
            assert!(
                columns(&store, "project_tasks")
                    .iter()
                    .any(|name| name == column),
                "缺少新列 {column}"
            );
        }
        // 旧数据完好，新列是 NULL。
        let stored = store.workflow(&workflow).unwrap();
        assert_eq!(stored.request, "旧需求");
        assert_eq!(stored.status, "completed");
        assert!(stored.roles.is_none());
        assert_eq!(stored.tasks.len(), 1);
        assert_eq!(stored.tasks[0].id, task);
        assert_eq!(stored.tasks[0].title, "旧任务");
        assert_eq!(stored.tasks[0].files, vec!["src/lib.rs".to_string()]);
        assert_eq!(stored.tasks[0].instructions, "按方案实现");
        let stored_task = store.task(&task).unwrap();
        assert!(stored_task.model.is_none());
        assert!(stored_task.effort.is_none());
        assert!(stored_task.worktree.is_none());
        assert!(stored_task.branch.is_none());
    }

    #[test]
    fn migration_is_idempotent_for_fresh_and_upgraded_databases() {
        let mut store = memory();
        let (workflow, _) = legacy_workflow(&mut store);
        // 全新库里列已经建好：再跑一遍迁移不能把列加第二遍。
        let tx = store.connection.transaction().unwrap();
        migrate(&tx).unwrap();
        tx.commit().unwrap();
        for column in ["model", "effort", "worktree", "branch", "assigned_agent"] {
            assert_eq!(
                columns(&store, "project_tasks")
                    .iter()
                    .filter(|name| *name == column)
                    .count(),
                1
            );
        }
        assert_eq!(
            columns(&store, "workflows")
                .iter()
                .filter(|name| *name == "roles")
                .count(),
            1
        );
        assert_eq!(
            columns(&store, "project_attempts")
                .iter()
                .filter(|name| *name == "thought")
                .count(),
            1
        );
        // v9 老库升一次，再降回 v9 重升一次：两次都不报错、数据也还在。
        store
            .connection
            .execute_batch("PRAGMA user_version=9;")
            .unwrap();
        let store = Store::initialize(store.connection, PathBuf::from(":memory:")).unwrap();
        store
            .connection
            .execute_batch("PRAGMA user_version=9;")
            .unwrap();
        let store = Store::initialize(store.connection, PathBuf::from(":memory:")).unwrap();
        assert_eq!(version(&store), crate::store::SCHEMA_VERSION);
        assert_eq!(store.workflow(&workflow).unwrap().tasks.len(), 1);
    }

    #[test]
    fn workflow_roles_round_trip_and_clear() {
        let mut store = memory();
        let (workflow, _) = legacy_workflow(&mut store);
        let roles = "{\"plan\":{\"agent\":\"codex-win\",\"model\":null,\"effort\":null},\"implement\":{\"agent\":\"dsh-win\",\"model\":null,\"effort\":null},\"review\":{\"agent\":\"hermes-win\",\"model\":null,\"effort\":null}}";
        let saved = store.set_workflow_roles(&workflow, Some(roles)).unwrap();
        assert_eq!(saved.roles.as_deref(), Some(roles));
        assert_eq!(
            store.workflow(&workflow).unwrap().roles.as_deref(),
            Some(roles)
        );
        // 非法配置不落库，原值保持原样。
        assert!(store
            .set_workflow_roles(&workflow, Some("不是 JSON"))
            .is_err());
        assert!(store.set_workflow_roles(&workflow, Some("[1,2]")).is_err());
        assert_eq!(
            store.workflow(&workflow).unwrap().roles.as_deref(),
            Some(roles)
        );
        assert!(store
            .set_workflow_roles(&workflow, None)
            .unwrap()
            .roles
            .is_none());
        assert!(store.workflow(&workflow).unwrap().roles.is_none());
        assert!(store.set_workflow_roles("missing", Some(roles)).is_err());
    }

    #[test]
    fn conversation_project_roles_round_trip_and_reset_after_member_changes() {
        let mut store = memory();
        let root = std::env::temp_dir().join(format!("hub-project-roles-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let project = store
            .register_project("角色配置项目", root.to_str().unwrap())
            .unwrap();
        let room = store
            .create(
                "项目群",
                "group",
                &["codex-win".into(), "hermes-win".into(), "dsh-win".into()],
            )
            .unwrap();
        store.bind_project(&room.id, Some(&project.id)).unwrap();
        let roles = Roles {
            plan: RoleChoice {
                agent: "hermes-win".into(),
                model: Some("custom:daily-one:deepseek/flash".into()),
                effort: Some("high".into()),
            },
            implement: RoleChoice {
                agent: "dsh-win".into(),
                model: Some("[\"command-code-daily\",\"deepseek/deepseek-v4.1-flash\"]".into()),
                effort: Some("off".into()),
            },
            review: RoleChoice {
                agent: "codex-win".into(),
                model: Some("gpt-6-luna".into()),
                effort: Some("low".into()),
            },
        };
        store
            .set_conversation_project_roles(&room.id, &roles)
            .unwrap();
        assert_eq!(
            store.conversation_project_roles(&room.id).unwrap(),
            Some(roles.clone())
        );
        assert_eq!(
            store.detail(&room.id).unwrap().project_roles,
            Some(roles.clone())
        );
        store
            .replace_members(&room.id, &["codex-win".into(), "dsh-win".into()])
            .unwrap();
        assert!(store
            .conversation_project_roles(&room.id)
            .unwrap()
            .is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn task_overrides_round_trip_and_allow_null() {
        let mut store = memory();
        let (workflow, task) = legacy_workflow(&mut store);
        let saved = store
            .set_task_config(
                &task,
                Some("provider:model".into()),
                Some("high".into()),
                Some("C:/worktrees/task-1".into()),
                Some("hub/task-1".into()),
            )
            .unwrap();
        assert_eq!(saved.model.as_deref(), Some("provider:model"));
        assert_eq!(saved.effort.as_deref(), Some("high"));
        assert_eq!(saved.worktree.as_deref(), Some("C:/worktrees/task-1"));
        assert_eq!(saved.branch.as_deref(), Some("hub/task-1"));
        // 读工作流时任务级覆盖也要带出来。
        let listed = store.workflow(&workflow).unwrap().tasks.remove(0);
        assert_eq!(listed.model.as_deref(), Some("provider:model"));
        assert_eq!(listed.effort.as_deref(), Some("high"));
        assert_eq!(listed.worktree.as_deref(), Some("C:/worktrees/task-1"));
        assert_eq!(listed.branch.as_deref(), Some("hub/task-1"));
        // 清除：写回 NULL 也要能表示。
        let cleared = store
            .set_task_config(&task, None, None, None, None)
            .unwrap();
        assert!(cleared.model.is_none());
        assert!(cleared.effort.is_none());
        assert!(cleared.worktree.is_none());
        assert!(cleared.branch.is_none());
        // 空白等价于未设置，仍然是 NULL。
        let blank = store
            .set_task_config(&task, Some("  ".into()), None, Some(String::new()), None)
            .unwrap();
        assert!(blank.model.is_none());
        assert!(blank.worktree.is_none());
        assert!(store
            .set_task_config("missing", None, None, None, None)
            .is_err());
        assert!(store
            .set_task_config(&task, Some("a\nb".into()), None, None, None)
            .is_err());
    }

    #[test]
    fn task_executor_can_change_only_while_queued_for_confirmation() {
        let mut store = memory();
        let (workflow, task) = legacy_workflow(&mut store);
        store
            .connection
            .execute(
                "UPDATE workflows SET status='planning' WHERE id=?1",
                [&workflow],
            )
            .unwrap();

        let saved = store.set_task_agent(&task, "codex-win").unwrap();
        assert_eq!(saved.agent_id, "codex-win");
        assert_eq!(
            store.workflow(&workflow).unwrap().tasks[0].agent_id,
            "codex-win"
        );
        let saved = store.set_task_agent(&task, "hermes-win").unwrap();
        assert_eq!(saved.agent_id, "hermes-win");
        assert_eq!(
            store.workflow(&workflow).unwrap().tasks[0].agent_id,
            "hermes-win"
        );

        store
            .connection
            .execute(
                "UPDATE workflows SET status='running' WHERE id=?1",
                [&workflow],
            )
            .unwrap();
        assert!(store.set_task_agent(&task, "dsh-win").is_err());
    }

    #[test]
    fn all_three_windows_agents_can_fill_each_project_role() {
        for agent in EXECUTOR_AGENTS {
            let roles = Roles {
                plan: RoleChoice {
                    agent: agent.into(),
                    model: None,
                    effort: None,
                },
                implement: RoleChoice {
                    agent: agent.into(),
                    model: None,
                    effort: None,
                },
                review: RoleChoice {
                    agent: agent.into(),
                    model: None,
                    effort: None,
                },
            };
            assert!(roles.validate().is_ok(), "{agent}");
        }
        let mut albion = Roles::defaults();
        albion.plan.agent = "albion-wsl".into();
        assert!(albion.validate().is_err());
    }

    #[test]
    fn project_agent_chats_are_private_hidden_from_sidebar_and_bound_to_project_cwd() {
        let mut store = memory();
        let room = store
            .create(
                "项目群",
                "group",
                &["codex-win".into(), "hermes-win".into(), "dsh-win".into()],
            )
            .unwrap();
        let root = std::env::temp_dir().join(format!("agent-hub-chat-root-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let project = store
            .register_project("项目", root.to_str().unwrap())
            .unwrap();
        store.bind_project(&room.id, Some(&project.id)).unwrap();

        let chats = store.ensure_project_agent_chats(&room.id).unwrap();
        assert_eq!(chats.len(), 3);
        assert_eq!(store.ensure_project_agent_chats(&room.id).unwrap().len(), 3);
        let listed = store.list("", false).unwrap();
        assert_eq!(
            listed.iter().filter(|item| item.kind == "direct").count(),
            5
        );
        for chat in chats {
            let detail = store.detail(&chat.conversation_id).unwrap();
            assert_eq!(detail.conversation.kind, "direct");
            assert_eq!(detail.conversation.members, vec![chat.agent_id.clone()]);
            assert_eq!(
                detail.sessions[0].native_cwd.as_deref(),
                Some(project.root.as_str())
            );
            assert!(detail.messages.is_empty());
        }
        assert_eq!(store.detail(&room.id).unwrap().project_agent_chats.len(), 3);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_private_agent_reply_can_be_shared_to_the_bound_team_once() {
        let mut store = memory();
        let room = store
            .create(
                "项目群",
                "group",
                &["codex-win".into(), "hermes-win".into(), "dsh-win".into()],
            )
            .unwrap();
        let root = std::env::temp_dir().join(format!("agent-hub-share-root-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let project = store
            .register_project("项目", root.to_str().unwrap())
            .unwrap();
        store.bind_project(&room.id, Some(&project.id)).unwrap();
        let chats = store.ensure_project_agent_chats(&room.id).unwrap();
        let hermes = chats
            .iter()
            .find(|chat| chat.agent_id == "hermes-win")
            .unwrap();
        let source = Uuid::new_v4().to_string();
        store.connection.execute(
            "INSERT INTO messages(id,conversation_id,sender_id,content,status,created_at) VALUES(?1,?2,'hermes-win','结论已复核，可以分享。','completed',?3)",
            params![source,hermes.conversation_id,now()],
        ).unwrap();

        let shared = store
            .share_project_agent_message(&room.id, &hermes.conversation_id, &source)
            .unwrap();
        assert_eq!(shared.sender_agent.as_deref(), Some("hermes-win"));
        assert!(shared.content.contains("可以分享"));
        assert_eq!(
            store
                .share_project_agent_message(&room.id, &hermes.conversation_id, &source)
                .unwrap()
                .id,
            shared.id
        );
        assert_eq!(store.team_messages(&room.id).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn user_can_post_updates_to_an_active_team_thread_without_starting_a_second_workflow() {
        let mut store = memory();
        let room = store
            .create(
                "项目群",
                "group",
                &["codex-win".into(), "hermes-win".into(), "dsh-win".into()],
            )
            .unwrap();
        let root = std::env::temp_dir().join(format!("agent-hub-team-post-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let project = store
            .register_project("项目", root.to_str().unwrap())
            .unwrap();
        store.bind_project(&room.id, Some(&project.id)).unwrap();
        let workflow = store
            .start_workflow(&room.id, &Uuid::new_v4().to_string(), "推进项目")
            .unwrap()
            .0;
        let posted = store
            .post_user_team_message(&room.id, "请先把兼容性问题发到团队讨论里")
            .unwrap();
        assert_eq!(posted.kind, "user");
        assert_eq!(posted.workflow_id.as_deref(), Some(workflow.id.as_str()));
        assert!(posted.content.contains("团队讨论"));
        assert_eq!(store.team_messages(&room.id).unwrap().len(), 1);
        assert_eq!(store.workflows(&room.id).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(root);
    }
}
