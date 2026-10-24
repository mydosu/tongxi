//! 项目登记、项目群绑定、成员专属会话和群消息。旧协作的表和迁移保留，数据不删。
use crate::store::{now, Store};
use rusqlite::{params, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path};
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


/// 可参与项目的 Windows 成员；阿尔比恩（WSL Pi）不加入项目工作流。
pub const EXECUTOR_AGENTS: [&str; 5] = ["codex-win", "hermes-win", "dsh-win", "claude-win", "opencode-win"];
/// project_tasks.agent_id 的约束只允许 Codex/DSH/Claude（v18 起含 Claude），Hermes 用 assigned_agent 列存放。
/// 同一成员在项目里的额外席位（`codex-win-2` 这类 id）：各有自己的专属会话与模型，
/// 调度把它们当独立成员派活，执行时落回底层成员的原生连接。
pub const SEAT_SLOTS: [u8; 3] = [2, 3, 4];
/// 主脑也是一个席位（`claude-win-brain` 这类）：项目里唯一一条主脑会话，跑在底层成员的原生连接上。
pub const BRAIN_SLOT: &str = "brain";
pub fn seat_ids() -> Vec<String> {
    EXECUTOR_AGENTS
        .iter()
        .flat_map(|agent| {
            SEAT_SLOTS
                .iter()
                .map(move |slot| format!("{agent}-{slot}"))
                .chain(std::iter::once(format!("{agent}-{BRAIN_SLOT}")))
        })
        .collect()
}
/// 席位 → 底层成员；不是席位就原样返回。
pub fn base_agent(id: &str) -> &str {
    if let Some((base, slot)) = id.rsplit_once('-') {
        if EXECUTOR_AGENTS.contains(&base)
            && (slot == BRAIN_SLOT || SEAT_SLOTS.iter().any(|n| n.to_string() == slot))
        {
            return base;
        }
    }
    id
}
pub fn is_brain(id: &str) -> bool {
    base_agent(id) != id && id.ends_with(BRAIN_SLOT)
}
/// 项目成员（含额外席位）。
pub fn is_executor(id: &str) -> bool {
    EXECUTOR_AGENTS.contains(&base_agent(id))
}












#[derive(Clone, Debug, Serialize)]
pub struct ProjectAgentChat {
    pub group_conversation_id: String,
    /// 成员 id；额外席位是 `codex-win-2` 这类，专属会话里的原生成员是它的底层成员。
    pub agent_id: String,
    pub conversation_id: String,
    /// 用户在这个项目里给这个会话起的名字；没起为空。
    pub label: Option<String>,
    /// 显示名：起过名就是它，否则按成员名与编号算好（前端直接用，不再自己拼）。
    pub name: String,
    /// 调度新开的会话（用户建的为 false）；和用户建的一样留在项目里。
    pub opened_by_coordinator: bool,
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
        ("project_attempts", "usage", "TEXT"),
        ("project_attempts", "reported_usage", "TEXT"),
        ("project_questions", "attempt_id", "TEXT"),
        (
            "project_questions",
            "delivery",
            "TEXT NOT NULL DEFAULT 'deferred'",
        ),
        ("project_questions", "answered_at", "INTEGER"),
        ("runs", "usage", "TEXT"),
    ] {
        add_column(tx, table, column, definition)?;
    }
    migrate_coordination(tx)?;
    crate::project_sessions::migrate(tx)?;
    // 旧版本把席位会话里的回复记在成员名下（claude-win），这里改记成席位（claude-win-2、claude-win-brain）。
    // 只动发送者正好是该席位底层成员的回复，重复跑不会再改；成员交回主脑的消息也记在成员名下，不是回复，不能动。
    tx.execute_batch("UPDATE messages SET sender_id=(SELECT p.agent_id FROM project_agent_chats p WHERE p.conversation_id=messages.conversation_id)
        WHERE sender_id<>'user' AND status NOT IN ('local_only','pending','delivered')
          AND id NOT IN (SELECT user_message_id FROM runs)
          AND EXISTS(SELECT 1 FROM project_agent_chats p WHERE p.conversation_id=messages.conversation_id
          AND substr(p.agent_id,1,length(messages.sender_id)+1)=messages.sender_id||'-');")
        .map_err(|e| e.to_string())?;
    // 旧版本把主脑派活、成员交回、同席通知都记成用户发的，按开头改记到真正的发送者名下；改过就不再是 user，重复跑不会再改。
    tx.execute_batch(&format!(
        "UPDATE messages SET sender_id=COALESCE((SELECT b.agent_id FROM project_agent_chats p JOIN project_agent_chats b ON b.group_conversation_id=p.group_conversation_id
            WHERE p.conversation_id=messages.conversation_id AND b.agent_id LIKE '%-{BRAIN_SLOT}' LIMIT 1),'hub')
          WHERE sender_id='user' AND (content LIKE '【主脑派活】%' OR content LIKE '【主脑插话】%')
            AND conversation_id IN (SELECT conversation_id FROM project_agent_chats WHERE agent_id NOT LIKE '%-{BRAIN_SLOT}');
         UPDATE messages SET sender_id=CASE
            WHEN length(content)-length(replace(content,'【成员交回】',''))=6 AND content NOT LIKE '%【同席】%' AND content NOT LIKE '%【用户回答】%'
            THEN COALESCE((SELECT agent_id FROM project_agent_chats WHERE agent_id=substr(content,instr(content,'（')+1,instr(content,'）')-instr(content,'（')-1) LIMIT 1),'hub')
            ELSE 'hub' END
          WHERE sender_id='user' AND (content LIKE '【成员交回】%' OR content LIKE '【同席】%' OR content LIKE '【用户回答】%')
            AND NOT (content LIKE '【用户回答】%' AND content NOT LIKE '%【成员交回】%' AND content NOT LIKE '%【同席】%')
            AND conversation_id IN (SELECT conversation_id FROM project_agent_chats WHERE agent_id LIKE '%-{BRAIN_SLOT}');"
    ))
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// 旧调度留下的表：协作数据保留，表结构照旧迁移。
pub(crate) fn migrate_coordination(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch("CREATE TABLE IF NOT EXISTS project_coordinator_settings (
      conversation_id TEXT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,settings TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS coordination_runs (
      workflow_id TEXT PRIMARY KEY REFERENCES workflows(id) ON DELETE CASCADE,coordinator TEXT NOT NULL,status TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS coordination_routes (
      task_id TEXT PRIMARY KEY REFERENCES project_tasks(id) ON DELETE CASCADE,workflow_id TEXT NOT NULL REFERENCES workflows(id) ON DELETE CASCADE,
      purpose TEXT NOT NULL,source_id TEXT NOT NULL,quote TEXT NOT NULL,context_ids TEXT NOT NULL,subagent INTEGER NOT NULL DEFAULT 0);
      CREATE INDEX IF NOT EXISTS coordination_route_key ON coordination_routes(workflow_id,purpose,source_id,quote);
      CREATE TABLE IF NOT EXISTS coordinator_sessions (
      workflow_id TEXT PRIMARY KEY REFERENCES workflows(id) ON DELETE CASCADE,agent_id TEXT NOT NULL,native_session_id TEXT,cwd TEXT NOT NULL);")
      .map_err(|_|"调度会话结构迁移失败")?;
    add_column(tx, "coordination_routes", "subagent", "INTEGER NOT NULL DEFAULT 0")?;
    add_column(tx, "coordination_routes", "instructions", "TEXT")?;
    add_column(tx, "coordination_routes", "reason", "TEXT")?;
    add_column(tx, "coordination_routes", "allow_subagents", "INTEGER NOT NULL DEFAULT 0")?;
    add_column(tx, "coordination_routes", "clear_context", "INTEGER NOT NULL DEFAULT 0")?;
    add_column(tx, "coordination_routes", "category", "TEXT")?;
    add_column(tx, "coordination_routes", "complexity", "TEXT")?;
    add_column(tx, "coordination_routes", "merge_state", "TEXT")?;
    // 旧调度的请教、软预算、账本已删，老库里留下的表和列一并清掉。
    tx.execute_batch("DROP TABLE IF EXISTS advisor_requests;")
        .map_err(|e| e.to_string())?;
    for (table, column) in [
        ("coordination_routes", "hold_merge"),
        ("coordination_routes", "budget_tokens"),
        ("coordination_routes", "advisor_calls"),
        ("coordination_runs", "ledger"),
    ] {
        if has_column(tx, table, column)? {
            tx.execute_batch(&format!("ALTER TABLE {table} DROP COLUMN {column};"))
                .map_err(|e| e.to_string())?;
        }
    }
    crate::project_inquiries::migrate(tx)
}

/// 幂等加列：列已存在就跳过，老库补列、新库不重复加。
pub(crate) fn add_column(tx: &Transaction<'_>, table: &str, column: &str, definition: &str) -> Result<()> {
    if !has_column(tx, table, column)? {
        tx.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {definition};"
        ))
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn has_column(tx: &Transaction<'_>, table: &str, column: &str) -> Result<bool> {
    let mut statement = tx
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|e| e.to_string())?;
    let exists = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| e.to_string())?
        .filter_map(std::result::Result::ok)
        .any(|name| name == column);
    Ok(exists)
}

fn overlaps(left: &str, right: &str) -> bool {
    left == right
        || left.starts_with(&format!("{right}/"))
        || right.starts_with(&format!("{left}/"))
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




/// 任务查询的列顺序与 `task_from_row` 一一对应。



impl Store {
    pub fn projects(&self) -> Result<Vec<Project>> {
        let mut stmt = self
            .connection
            .prepare("SELECT id FROM projects WHERE root_key NOT LIKE 'released:%' ORDER BY created_at,id")
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
    /// 解除登记：把项目从目录上解开、从「已登记项目」里拿掉。还有没归档的群在用、或协作正在运行时拒绝。
    /// 归档群对它的绑定一并解开（聊天和协作历史保留）；有协作历史的项目留着记录，只让出目录。
    pub fn release_project(&mut self, id: &str) -> Result<()> {
        let project = self.project(id)?;
        let active: Option<String> = self.connection.query_row(
            "SELECT c.title FROM conversation_projects cp JOIN conversations c ON c.id=cp.conversation_id WHERE cp.project_id=?1 AND c.archived=0 ORDER BY c.updated_at DESC LIMIT 1",
            [id], |r| r.get(0),
        ).optional().map_err(|e| e.to_string())?;
        if let Some(title) = active {
            return Err(format!("「{}」还被「{title}」使用，先在那个群里解除绑定或把它归档", project.name));
        }
        let running: bool = self
            .connection
            .query_row("SELECT EXISTS(SELECT 1 FROM project_leases WHERE project_id=?1)", [id], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if running {
            return Err("这个项目还有协作在运行，停止后再解除登记".into());
        }
        let tx = self.connection.transaction().map_err(|e| e.to_string())?;
        tx.execute("DELETE FROM conversation_projects WHERE project_id=?1", [id]).map_err(|e| e.to_string())?;
        tx.execute("DELETE FROM project_sessions WHERE project_id=?1", [id]).map_err(|e| e.to_string())?;
        let history: bool = tx
            .query_row("SELECT EXISTS(SELECT 1 FROM workflows WHERE project_id=?1)", [id], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if history {
            tx.execute("UPDATE projects SET root_key='released:'||id WHERE id=?1", [id]).map_err(|e| e.to_string())?;
        } else {
            tx.execute("DELETE FROM projects WHERE id=?1", [id]).map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())
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
                // 只有已归档的群用过这个目录时，不再占着它：沿用原登记（历史与项目记忆接得上），改成新名字。
                let active: Option<String> = self.connection.query_row(
                    "SELECT c.title FROM conversation_projects cp JOIN conversations c ON c.id=cp.conversation_id WHERE cp.project_id=?1 AND c.archived=0 ORDER BY c.updated_at DESC LIMIT 1",
                    [&id], |r| r.get(0),
                ).optional().map_err(|e| e.to_string())?;
                if let Some(title) = active {
                    return Err(format!("这个目录正被「{title}」使用（项目名「{}」）；可以在「已登记项目」里直接选它，或先归档那个群", project.name));
                }
                self.connection.execute("UPDATE projects SET name=?1 WHERE id=?2", params![name, id]).map_err(|e| e.to_string())?;
                return self.project(&id);
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
    pub fn bind_project(&mut self, room: &str, project: Option<&str>) -> Result<()> {
        let conversation = self.conversation(room)?;
        if conversation.kind != "group" {
            return Err("只有群聊才能绑定项目".into());
        }
        if conversation.archived {
            return Err("先恢复归档会话再绑定项目".into());
        }
        self.guard_discussion(room)?;
        self.guard_brain_jobs(room)?;
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
        if !group.members.iter().any(|member| member == base_agent(&agent)) {
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
            "SELECT c.group_conversation_id,c.agent_id,c.conversation_id,COALESCE(NULLIF(s.label,''),n.label),s.temporary_for IS NOT NULL FROM project_agent_chats c LEFT JOIN project_seats s ON s.conversation_id=c.group_conversation_id AND s.seat_id=c.agent_id LEFT JOIN project_member_names n ON n.conversation_id=c.group_conversation_id AND n.agent_id=c.agent_id WHERE c.project_id=?1 AND c.group_conversation_id=?2 ORDER BY CASE WHEN c.agent_id LIKE 'codex-win%' THEN 0 WHEN c.agent_id LIKE 'hermes-win%' THEN 1 WHEN c.agent_id LIKE 'dsh-win%' THEN 2 ELSE 3 END,c.agent_id",
        ).map_err(|_| "Agent 会话读取失败")?;
        let rows = statement
            .query_map(params![project.id, group_id], |row| {
                Ok(ProjectAgentChat {
                    group_conversation_id: row.get(0)?,
                    agent_id: row.get(1)?,
                    conversation_id: row.get(2)?,
                    label: row.get(3)?,
                    name: String::new(),
                    opened_by_coordinator: row.get::<_, Option<bool>>(4)?.unwrap_or(false),
                })
            })
            .map_err(|_| "Agent 会话读取失败")?;
        let mut chats = rows
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| "Agent 会话读取失败")?;
        drop(statement);
        // 只列出仍在群里的成员的会话；移出的成员（及其多开的会话）数据保留，加回来就恢复。
        // 主脑只列当前那一条（换过主脑成员的，旧主脑会话留着不显示），排在最前。
        let members = self.conversation(group_id)?.members;
        let brain = crate::project_brain::brain_seat(self, group_id).ok();
        chats.retain(|chat| {
            members.iter().any(|member| member == base_agent(&chat.agent_id))
                && (!is_brain(&chat.agent_id) || brain.as_deref() == Some(chat.agent_id.as_str()))
        });
        chats.sort_by_key(|chat| !is_brain(&chat.agent_id));
        for chat in &mut chats {
            chat.name = self.seat_name(group_id, &chat.agent_id);
        }
        Ok(chats)
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
        // 主席位按群成员来；额外席位来自 project_seats，底层成员也必须仍在群里。
        let mut seats: Vec<String> = EXECUTOR_AGENTS
            .iter()
            .filter(|agent| group.members.iter().any(|member| member == **agent))
            .map(|agent| agent.to_string())
            .collect();
        for (seat, _) in self.project_seats(group_id)? {
            if group.members.iter().any(|member| member == base_agent(&seat)) {
                seats.push(seat);
            }
        }
        if let Ok(brain) = crate::project_brain::brain_seat(self, group_id) {
            seats.push(brain);
        }
        for seat in seats {
            let label = self.seat_name(group_id, &seat);
            let agent = base_agent(&seat).to_owned();
            let exists: bool = self.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM project_agent_chats WHERE project_id=?1 AND group_conversation_id=?2 AND agent_id=?3)",
                params![project.id,group_id,seat], |row| row.get(0),
            ).map_err(|_| "Agent 会话状态不可用")?;
            if exists {
                continue;
            }
            let title = format!("{} · {}", project.name, label);
            let chat = self.create(&title, "direct", &[agent.clone()])?;
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
                params![project.id,group_id,seat,chat.id,now()],
            ).map_err(|_| "Agent 项目会话绑定失败")?;
            tx.commit().map_err(|_| "Agent 项目会话保存失败")?;
        }
        crate::project_brain::sync_brain_settings(self, group_id)?;
        self.project_agent_chats(group_id)
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








}
