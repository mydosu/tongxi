use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Serialize)]
pub struct Agent {
    pub id: String,
    pub name: String,
    pub subtitle: String,
    pub role: String,
    pub location: String,
    pub accent: String,
    pub status: String,
    /// 陪伴的头像（用户上传的 data URL）；其他成员没有。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
}

/// 当前数据库 schema 版本；项目工具子进程据此拒绝不匹配的数据库。
pub(crate) const SCHEMA_VERSION: i64 = 19;

/// 把成员白名单 CHECK 约束里的 `existing` 后补上 `added`。
///
/// SQLite 不能 ALTER 约束；这些表又互相 `ON DELETE CASCADE`，重建表会在 DROP 旧表时
/// 连带删掉子表数据。改用 SQLite 官方文档允许的做法（lang_altertable「其他表结构修改」）：
/// 同一事务内只改 sqlite_schema 里的建表文本、递增 schema_version，再做完整性检查。
/// 放宽约束不改变任何已存数据的含义，也不触碰磁盘上的行格式。
pub(crate) fn allow_agent_in_checks(
    tx: &rusqlite::Transaction<'_>,
    existing: &str,
    added: &str,
) -> Result<()> {
    let from = format!("'{existing}'");
    let to = format!("'{existing}','{added}'");
    let tables = {
        let mut statement = tx
            .prepare("SELECT name,sql FROM sqlite_schema WHERE type='table' AND sql IS NOT NULL")
            .map_err(|e| e.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| e.to_string())?
            .filter_map(std::result::Result::ok)
            .filter(|(_, sql)| sql.contains(&from) && !sql.contains(&format!("'{added}'")))
            .collect::<Vec<_>>();
        rows
    };
    if tables.is_empty() {
        return Ok(());
    }
    let version: i64 = tx
        .query_row("PRAGMA schema_version", [], |row| row.get(0))
        .map_err(|e| e.to_string())?;
    tx.execute_batch("PRAGMA writable_schema=ON;")
        .map_err(|e| e.to_string())?;
    let patched = (|| -> Result<()> {
        for (name, sql) in &tables {
            tx.execute(
                "UPDATE sqlite_schema SET sql=?1 WHERE type='table' AND name=?2",
                rusqlite::params![sql.replace(&from, &to), name],
            )
            .map_err(|e| e.to_string())?;
        }
        tx.execute_batch(&format!("PRAGMA schema_version={};", version + 1))
            .map_err(|e| e.to_string())
    })();
    tx.execute_batch("PRAGMA writable_schema=OFF;")
        .map_err(|e| e.to_string())?;
    patched?;
    let check: String = tx
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(|e| e.to_string())?;
    if check != "ok" {
        return Err("成员约束升级后完整性检查失败，未提交迁移".into());
    }
    Ok(())
}

/// 成员显示名；未知 id 原样返回，不猜。
pub(crate) fn agent_name(id: &str) -> &str {
    match id {
        "codex-win" => "Codex",
        "hermes-win" => "Hermes",
        "dsh-win" => "DSH",
        "claude-win" => "Claude",
        "albion-wsl" => "陪伴",
        other => other,
    }
}

pub fn agents() -> Vec<Agent> {
    [
        (
            "hermes-win",
            "Hermes",
            "Hermes Agent",
            "本机 Hermes，使用它自己配置的模型、工具、MCP 与插件",
            "Windows",
            "amber",
        ),
        (
            "codex-win",
            "Codex",
            "Codex CLI",
            "本机 Codex，使用它自己配置的模型、工具、MCP 与插件",
            "Windows",
            "green",
        ),
        (
            "dsh-win",
            "DSH",
            "DeepSeek Harness",
            "本机 DSH，使用它自己的原生工具，以及它自己配置的模型与 Key",
            "Windows",
            "blue",
        ),
        (
            "claude-win",
            "Claude",
            "Claude Code",
            "本机 Claude Code，使用它自己配置的模型、工具、MCP 与插件",
            "Windows",
            "orange",
        ),
        (
            "albion-wsl",
            "陪伴",
            "情感陪伴",
            "通过它自己的服务对话，人格、记忆与工具都在那边",
            "陪伴",
            "rose",
        ),
    ]
    .into_iter()
    .map(|(id, name, subtitle, role, location, accent)| Agent {
        id: id.into(),
        name: name.into(),
        subtitle: subtitle.into(),
        role: role.into(),
        location: location.into(),
        accent: accent.into(),
        status: "not_connected".into(),
        avatar: None,
    })
    .collect()
}

#[derive(Clone, Serialize, Debug)]
pub struct Conversation {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub archived: bool,
    pub created_at: i64,
    pub updated_at: i64,
    pub members: Vec<String>,
    pub message_count: i64,
    pub preview: String,
}

#[derive(Clone, Serialize, Debug, PartialEq)]
pub struct Message {
    pub id: String,
    pub conversation_id: String,
    pub sender_id: String,
    pub content: String,
    pub status: String,
    pub created_at: i64,
    /// 后端推理块（模型的思考），可能为空；不显示在气泡正文里。
    pub thought: String,
    pub tool_events: Vec<RunToolEvent>,
}

#[derive(Clone, Serialize, Debug, PartialEq)]
pub struct RunToolEvent {
    pub event_id: String,
    pub kind: String,
    pub title: String,
    pub input: Option<String>,
    pub output: Option<String>,
    pub status: String,
    pub created_at: i64,
}

#[derive(Serialize)]
pub struct Session {
    pub agent_id: String,
    pub session_key: String,
    pub native_session_id: Option<String>,
    /// 原生会话自己的工作目录（接入时记下，续聊时必须原样传回去）。
    pub native_cwd: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
}

#[derive(Serialize)]
pub struct ConversationDetail {
    pub conversation: Conversation,
    pub messages: Vec<Message>,
    pub sessions: Vec<Session>,
    pub discussions: Vec<crate::group_store::Discussion>,
    pub project: Option<crate::project_store::Project>,
    pub project_roles: Option<crate::project_store::Roles>,
    pub project_coordinator: Option<crate::project_store::RoleChoice>,
    /// 调度倾向：save / balanced / quality。
    pub project_preference: String,
    /// 项目长期授权：用户写的一段话，每次协作都算用户原话授权。
    pub project_grants: String,
    pub workflows: Vec<crate::project_store::Workflow>,
    pub project_agent_chats: Vec<crate::project_store::ProjectAgentChat>,
    pub project_team_messages: Vec<crate::project_store::TeamMessage>,
    pub project_agent_inbox: Vec<crate::project_store::RoomAgentMessage>,
    pub project_questions: Vec<crate::project_questions::Question>,
    pub chat_questions: Vec<crate::chat_questions::Question>,
}

pub struct Store {
    pub(crate) connection: Connection,
    pub path: PathBuf,
    pub(crate) service_maintenance: HashSet<String>,
    /// 交给 CLI 使用中的（会话, 成员）：同席暂停对这条原生会话的使用，CLI 关闭后自动收回。
    pub(crate) cli_handoffs: HashSet<(String, String)>,
}

#[derive(Clone, Serialize, Debug)]
pub struct RunRecord {
    pub id: String,
    pub agent_id: String,
    pub conversation_id: String,
    pub user_message_id: String,
    pub assistant_message_id: String,
    pub status: String,
    pub native_thread_id: Option<String>,
    pub native_turn_id: Option<String>,
    pub error: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub discussion_id: Option<String>,
    pub round: Option<u32>,
}

/// 思考与正文交替出现时，在思考文本里插入 `\u{1e}<正文 UTF-16 长度>\u{1f}`，
/// 记下“下面这段思考出现在正文第几个字符之后”，界面据此把两者穿插显示（像各 CLI 那样）。
pub(crate) fn push_thought(thought: &mut String, text: &str, delta: &str) {
    if delta.is_empty() {
        return;
    }
    let at = text.encode_utf16().count();
    let last = thought
        .rfind('\u{1e}')
        .and_then(|index| thought[index + 1..].split('\u{1f}').next()?.parse().ok())
        .unwrap_or(0);
    if at > last {
        thought.push_str(&format!("\u{1e}{at}\u{1f}"));
    }
    thought.push_str(delta);
}

/// 正文另起一段（新的回复条目，或工具调用之后再开口）：记 `\u{1d}<正文 UTF-16 长度>\u{1f}`，
/// 界面在这里另起一个气泡，像 CLI 那样一条一条地出。
pub(crate) fn push_break(thought: &mut String, text: &str) {
    let at = text.encode_utf16().count();
    let mark = format!("\u{1d}{at}\u{1f}");
    if at > 0 && !thought.ends_with(&mark) {
        thought.push_str(&mark);
    }
}

/// 工具调用第一次出现：记 `\u{1b}<正文 UTF-16 长度>:<工具事件 id>\u{1f}`，
/// 界面把这次调用插在思考与正文之间显示（像 Hermes 那样一行一个），之后的正文另起一个气泡。
pub(crate) fn push_tool(thought: &mut String, text: &str, event_id: &str) {
    let tail = format!(":{event_id}\u{1f}");
    if thought.contains(&tail) {
        return;
    }
    let at = text.encode_utf16().count();
    thought.push_str(&format!("\u{1b}{at}{tail}"));
}

/// 回复中途用户插话：记 `\u{1c}<正文 UTF-16 长度>:<插话消息 id>\u{1f}`，
/// 界面把这条插话放在这里，之后的回复接在插话下面另起一条。
pub(crate) fn push_steer(thought: &mut String, text: &str, message_id: &str) {
    let at = text.encode_utf16().count();
    thought.push_str(&format!("\u{1c}{at}:{message_id}\u{1f}"));
}

pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn valid_title(title: &str) -> Result<String> {
    let title = title.trim();
    if title.is_empty() || title.chars().count() > 80 || title.chars().any(char::is_control) {
        return Err("会话名称需为 1–80 个字符，且不包含控制字符".into());
    }
    Ok(title.into())
}

fn valid_members(kind: &str, members: &[String]) -> Result<()> {
    if kind != "direct" && kind != "group" {
        return Err("不支持的会话类型".into());
    }
    if (kind == "direct" && members.len() != 1)
        || (kind == "group" && !(1..=5).contains(&members.len()))
    {
        // 群聊可以只有一位成员：同一成员在项目里多开的会话也算成员（像多 agent 协作那样）。
        return Err("私聊需选择一位成员，群聊需选择一至五位成员".into());
    }
    let roster = agents();
    for (index, member) in members.iter().enumerate() {
        if !roster.iter().any(|agent| &agent.id == member) {
            return Err("成员不存在".into());
        }
        if members[..index].contains(member) {
            return Err("不能重复添加成员".into());
        }
    }
    Ok(())
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let connection = Connection::open(path).map_err(|error| error.to_string())?;
        Self::initialize(connection, path.to_path_buf())
    }

    pub(crate) fn initialize(mut connection: Connection, path: PathBuf) -> Result<Self> {
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| e.to_string())?;
        connection
            .execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;")
            .map_err(|e| e.to_string())?;
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|e| e.to_string())?;
        if version > SCHEMA_VERSION {
            return Err("数据由更新的软件版本创建，请使用对应版本打开".into());
        }
        if version > 0 && version < SCHEMA_VERSION && path.is_absolute() {
            let backups = path.parent().ok_or("数据库目录无效")?.join("backups");
            std::fs::create_dir_all(&backups).map_err(|_| "迁移前数据库备份目录创建失败")?;
            let snapshot = backups.join(format!("hub-schema-v{version}-{}.db", Uuid::new_v4()));
            // SQLite snapshots the full committed state, including WAL pages.
            connection
                .execute("VACUUM INTO ?1", [snapshot.to_string_lossy().as_ref()])
                .map_err(|_| "迁移前数据库备份失败，未执行迁移")?;
        }
        let tx = connection.transaction().map_err(|e| e.to_string())?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS conversations (
                id TEXT PRIMARY KEY, title TEXT NOT NULL, kind TEXT NOT NULL CHECK(kind IN ('direct','group')),
                archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN (0,1)),
                created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS members (
                conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                agent_id TEXT NOT NULL, position INTEGER NOT NULL,
                PRIMARY KEY(conversation_id,agent_id)
            );
            CREATE TABLE IF NOT EXISTS sessions (
                conversation_id TEXT NOT NULL, agent_id TEXT NOT NULL,
                session_key TEXT NOT NULL UNIQUE, native_session_id TEXT,
                PRIMARY KEY(conversation_id,agent_id),
                FOREIGN KEY(conversation_id,agent_id) REFERENCES members(conversation_id,agent_id) ON DELETE CASCADE
            );
            CREATE TABLE IF NOT EXISTS messages (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE,
                conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                sender_id TEXT NOT NULL, content TEXT NOT NULL,
                status TEXT NOT NULL CHECK(status='local_only'), created_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS message_conversation ON messages(conversation_id,sequence);
            CREATE TABLE IF NOT EXISTS metadata (key TEXT PRIMARY KEY,value TEXT NOT NULL);
            "
        ).map_err(|e| e.to_string())?;
        if version < 2 {
            tx.execute_batch(
                "ALTER TABLE messages RENAME TO messages_v1;
                 DROP INDEX message_conversation;
                 CREATE TABLE messages (
                   sequence INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE,
                   conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                   sender_id TEXT NOT NULL, content TEXT NOT NULL,
                   status TEXT NOT NULL CHECK(status IN ('local_only','pending','delivered','streaming','completed','interrupted','failed')),
                   created_at INTEGER NOT NULL);
                 INSERT INTO messages SELECT * FROM messages_v1;
                 DROP TABLE messages_v1;
                 CREATE INDEX message_conversation ON messages(conversation_id,sequence);"
            ).map_err(|e| e.to_string())?;
        }
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS runs (
               id TEXT PRIMARY KEY,
               conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
               agent_id TEXT NOT NULL CHECK(agent_id='codex-win'),
               user_message_id TEXT NOT NULL UNIQUE REFERENCES messages(id) ON DELETE CASCADE,
               assistant_message_id TEXT NOT NULL UNIQUE REFERENCES messages(id) ON DELETE CASCADE,
               status TEXT NOT NULL CHECK(status IN ('starting','running','cancelling','completed','interrupted','failed')),
               native_thread_id TEXT, native_turn_id TEXT, error TEXT);
"
        ).map_err(|e| e.to_string())?;
        if version < 3 {
            tx.execute_batch(
                "ALTER TABLE sessions ADD COLUMN model TEXT;
                 ALTER TABLE sessions ADD COLUMN reasoning_effort TEXT;
                 ALTER TABLE runs RENAME TO runs_v2;
                 DROP INDEX IF EXISTS one_active_codex;
                 CREATE TABLE runs (
                   id TEXT PRIMARY KEY,
                   conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                   agent_id TEXT NOT NULL CHECK(agent_id IN ('codex-win','hermes-win')),
                   user_message_id TEXT NOT NULL UNIQUE REFERENCES messages(id) ON DELETE CASCADE,
                   assistant_message_id TEXT NOT NULL UNIQUE REFERENCES messages(id) ON DELETE CASCADE,
                   status TEXT NOT NULL CHECK(status IN ('starting','running','cancelling','completed','interrupted','failed')),
                   native_thread_id TEXT, native_turn_id TEXT, error TEXT, model TEXT, reasoning_effort TEXT);
                 INSERT INTO runs(id,conversation_id,agent_id,user_message_id,assistant_message_id,status,native_thread_id,native_turn_id,error) SELECT id,conversation_id,agent_id,user_message_id,assistant_message_id,status,native_thread_id,native_turn_id,error FROM runs_v2;
                 DROP TABLE runs_v2;
                 CREATE UNIQUE INDEX one_active_agent ON runs(agent_id)
                   WHERE status IN ('starting','running','cancelling');"
            ).map_err(|e| e.to_string())?;
        }
        if version < 4 {
            tx.execute_batch("DROP INDEX IF EXISTS one_active_agent;
                ALTER TABLE runs RENAME TO runs_v3;
                CREATE TABLE runs (
                   id TEXT PRIMARY KEY,
                   conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                   agent_id TEXT NOT NULL CHECK(agent_id IN ('codex-win','hermes-win','dsh-win')),
                   user_message_id TEXT NOT NULL UNIQUE REFERENCES messages(id) ON DELETE CASCADE,
                   assistant_message_id TEXT NOT NULL UNIQUE REFERENCES messages(id) ON DELETE CASCADE,
                   status TEXT NOT NULL CHECK(status IN ('starting','running','cancelling','completed','interrupted','failed')),
                   native_thread_id TEXT, native_turn_id TEXT, error TEXT, model TEXT, reasoning_effort TEXT);
                INSERT INTO runs SELECT id,conversation_id,agent_id,user_message_id,assistant_message_id,status,native_thread_id,native_turn_id,error,model,reasoning_effort FROM runs_v3;
                DROP TABLE runs_v3;
                CREATE UNIQUE INDEX one_active_agent ON runs(agent_id) WHERE status IN ('starting','running','cancelling');").map_err(|e| e.to_string())?;
        }
        if version < 5 {
            tx.execute_batch("DROP INDEX IF EXISTS one_active_agent;
                ALTER TABLE runs RENAME TO runs_v4;
                CREATE TABLE runs (
                   id TEXT PRIMARY KEY,
                   conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                   agent_id TEXT NOT NULL CHECK(agent_id IN ('codex-win','hermes-win','dsh-win','albion-wsl')),
                   user_message_id TEXT NOT NULL UNIQUE REFERENCES messages(id) ON DELETE CASCADE,
                   assistant_message_id TEXT NOT NULL UNIQUE REFERENCES messages(id) ON DELETE CASCADE,
                   status TEXT NOT NULL CHECK(status IN ('starting','running','cancelling','completed','interrupted','failed')),
                   native_thread_id TEXT, native_turn_id TEXT, error TEXT, model TEXT, reasoning_effort TEXT);
                INSERT INTO runs SELECT id,conversation_id,agent_id,user_message_id,assistant_message_id,status,native_thread_id,native_turn_id,error,model,reasoning_effort FROM runs_v4;
                DROP TABLE runs_v4;
                CREATE UNIQUE INDEX one_active_agent ON runs(agent_id) WHERE status IN ('starting','running','cancelling');").map_err(|e| e.to_string())?;
        }
        crate::group_store::migrate(&tx, version)?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS run_tool_events (
            run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
            event_id TEXT NOT NULL,kind TEXT NOT NULL,title TEXT NOT NULL,input TEXT,output TEXT,
            status TEXT NOT NULL,created_at INTEGER NOT NULL,PRIMARY KEY(run_id,event_id));
            CREATE INDEX IF NOT EXISTS run_tool_event_order ON run_tool_events(run_id,created_at,event_id);")
            .map_err(|e| e.to_string())?;
        crate::project_store::migrate(&tx)?;
        if version < 8 {
            // 推理块（后端思考）随回复一起存，历史会话里也能回看。
            // 先查列：老库降级重跑（测试里会有）时这条 ALTER 不能再打一次。
            let mut statement = tx
                .prepare("PRAGMA table_info(messages)")
                .map_err(|e| e.to_string())?;
            let has_thought = statement
                .query_map([], |row| row.get::<_, String>(1))
                .map_err(|e| e.to_string())?
                .filter_map(std::result::Result::ok)
                .any(|name| name == "thought");
            drop(statement);
            if !has_thought {
                tx.execute_batch(
                    "ALTER TABLE messages ADD COLUMN thought TEXT NOT NULL DEFAULT '';",
                )
                .map_err(|e| e.to_string())?;
            }
        }
        if version < 9 {
            // 接入原生会话时要记住它自己的工作目录：DSH 的会话按 cwd 分桶存，
            // 换了目录再 session/load 会被拒（-32602）。同样先查列，降级重跑不再 ALTER。
            let mut statement = tx
                .prepare("PRAGMA table_info(sessions)")
                .map_err(|e| e.to_string())?;
            let has_cwd = statement
                .query_map([], |row| row.get::<_, String>(1))
                .map_err(|e| e.to_string())?
                .filter_map(std::result::Result::ok)
                .any(|name| name == "native_cwd");
            drop(statement);
            if !has_cwd {
                tx.execute_batch("ALTER TABLE sessions ADD COLUMN native_cwd TEXT;")
                    .map_err(|e| e.to_string())?;
            }
        }
        if version < 18 {
            allow_agent_in_checks(&tx, "dsh-win", "claude-win")?;
        }
        // 项目额外席位（codex-win-2 …）：放宽成员白名单。幂等，已放宽的表会被跳过。
        for seat in crate::project_store::seat_ids() {
            allow_agent_in_checks(&tx, "dsh-win", &seat)?;
        }
        if version < 19 {
            // v19：同一句原文可以有返修/复核轮次，去掉“一句原文只能一个任务”的唯一约束，
            // 去重改在 enqueue_routes 里判断。coordination_routes 是叶子表（没有表引用它），重建安全。
            let unique: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='coordination_routes' AND sql LIKE '%UNIQUE(workflow_id,purpose,source_id,quote)%')",
                    [],
                    |row| row.get(0),
                )
                .map_err(|e| e.to_string())?;
            if unique {
                tx.execute_batch(
                    "CREATE TABLE coordination_routes_v19 (
                       task_id TEXT PRIMARY KEY REFERENCES project_tasks(id) ON DELETE CASCADE,workflow_id TEXT NOT NULL REFERENCES workflows(id) ON DELETE CASCADE,
                       purpose TEXT NOT NULL,source_id TEXT NOT NULL,quote TEXT NOT NULL,context_ids TEXT NOT NULL,subagent INTEGER NOT NULL DEFAULT 0,
                       instructions TEXT,reason TEXT,allow_subagents INTEGER NOT NULL DEFAULT 0);
                     INSERT INTO coordination_routes_v19(rowid,task_id,workflow_id,purpose,source_id,quote,context_ids)
                       SELECT rowid,task_id,workflow_id,purpose,source_id,quote,context_ids FROM coordination_routes
                       WHERE task_id IN (SELECT id FROM project_tasks) AND workflow_id IN (SELECT id FROM workflows);
                     DROP TABLE coordination_routes;
                     ALTER TABLE coordination_routes_v19 RENAME TO coordination_routes;
                     CREATE INDEX IF NOT EXISTS coordination_route_key ON coordination_routes(workflow_id,purpose,source_id,quote);",
                )
                .map_err(|e| e.to_string())?;
            }
        }
        // 一个成员可以同时在多个会话里回复：进行中的运行改为“每个成员 × 会话”唯一。
        // cli_handoffs：交给 CLI 使用中的会话，重启同席后仍能认出并在 CLI 关闭时收回。
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS cli_handoffs (
               conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
               agent_id TEXT NOT NULL, native_session_id TEXT NOT NULL, pid INTEGER NOT NULL DEFAULT 0,
               since INTEGER NOT NULL, PRIMARY KEY(conversation_id, agent_id));
             DROP INDEX IF EXISTS one_active_agent;
             CREATE UNIQUE INDEX IF NOT EXISTS one_active_agent_conversation ON runs(agent_id,conversation_id)
               WHERE status IN ('starting','running','cancelling');",
        )
        .map_err(|e| e.to_string())?;
        tx.execute_batch(&format!("PRAGMA user_version={SCHEMA_VERSION};"))
            .map_err(|e| e.to_string())?;
        let seeded = tx
            .query_row("SELECT value FROM metadata WHERE key='seeded'", [], |row| {
                row.get::<_, String>(0)
            })
            .optional()
            .map_err(|e| e.to_string())?;
        if seeded.is_some() && version > 0 && version < 18 {
            // 已有数据库：第五位成员 Claude 加入时补一个私聊；全新库由下面的初始化统一创建。
            Self::insert_conversation(&tx, "与 Claude 私聊", "direct", &["claude-win".into()])?;
        }
        if seeded.is_none() {
            for agent in agents() {
                Self::insert_conversation(
                    &tx,
                    &format!("与 {} 私聊", agent.name),
                    "direct",
                    &[agent.id],
                )?;
            }
            Self::insert_conversation(
                &tx,
                "开发协作",
                "group",
                &["hermes-win".into(), "codex-win".into(), "dsh-win".into()],
            )?;
            tx.execute("INSERT INTO metadata(key,value) VALUES('seeded','1')", [])
                .map_err(|e| e.to_string())?;
        }
        crate::chat_questions::migrate(&tx)?;
        crate::backgrounds::migrate(&tx)?;
        tx.commit().map_err(|e| e.to_string())?;
        let mut store = Self {
            connection,
            path,
            service_maintenance: HashSet::new(),
            cli_handoffs: HashSet::new(),
        };
        store.cli_handoffs = store.saved_cli_handoffs()?.into_iter().map(|row| (row.0, row.1)).collect();
        store.recover_runs()?;
        store.recover_projects()?;
        Ok(store)
    }

    fn insert_conversation(
        tx: &Transaction<'_>,
        title: &str,
        kind: &str,
        members: &[String],
    ) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        let timestamp = now();
        tx.execute(
            "INSERT INTO conversations(id,title,kind,created_at,updated_at) VALUES(?1,?2,?3,?4,?4)",
            params![id, title, kind, timestamp],
        )
        .map_err(|e| e.to_string())?;
        for (position, agent_id) in members.iter().enumerate() {
            tx.execute(
                "INSERT INTO members(conversation_id,agent_id,position) VALUES(?1,?2,?3)",
                params![id, agent_id, position as i64],
            )
            .map_err(|e| e.to_string())?;
            tx.execute(
                "INSERT INTO sessions(conversation_id,agent_id,session_key) VALUES(?1,?2,?3)",
                params![id, agent_id, Uuid::new_v4().to_string()],
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(id)
    }

    pub fn create(&mut self, title: &str, kind: &str, members: &[String]) -> Result<Conversation> {
        let title = valid_title(title)?;
        valid_members(kind, members)?;
        let tx = self.connection.transaction().map_err(|e| e.to_string())?;
        let id = Self::insert_conversation(&tx, &title, kind, members)?;
        tx.commit().map_err(|e| e.to_string())?;
        self.conversation(&id)
    }

    pub fn conversation(&self, id: &str) -> Result<Conversation> {
        let mut conversation = self.connection.query_row(
            "SELECT id,title,kind,archived,created_at,updated_at,
                (SELECT count(*) FROM messages WHERE conversation_id=conversations.id),
                COALESCE((SELECT substr(content,1,100) FROM messages WHERE conversation_id=conversations.id ORDER BY sequence DESC LIMIT 1),'')
             FROM conversations WHERE id=?1", [id],
            |row| Ok(Conversation {
                id: row.get(0)?, title: row.get(1)?, kind: row.get(2)?, archived: row.get(3)?,
                created_at: row.get(4)?, updated_at: row.get(5)?, members: Vec::new(),
                message_count: row.get(6)?, preview: row.get(7)?,
            }),
        ).optional().map_err(|e| e.to_string())?.ok_or("会话不存在")?;
        let mut statement = self
            .connection
            .prepare("SELECT agent_id FROM members WHERE conversation_id=?1 ORDER BY position")
            .map_err(|e| e.to_string())?;
        conversation.members = statement
            .query_map([id], |row| row.get(0))
            .map_err(|e| e.to_string())?
            .collect::<std::result::Result<Vec<String>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(conversation)
    }

    pub fn list(&self, search: &str, archived: bool) -> Result<Vec<Conversation>> {
        if search.chars().count() > 120 {
            return Err("搜索内容过长".into());
        }
        let mut statement = self.connection.prepare(
            "SELECT id FROM conversations c WHERE archived=?1 AND NOT EXISTS (SELECT 1 FROM project_agent_chats pchat WHERE pchat.conversation_id=c.id) AND (
                ?2='' OR instr(lower(title),lower(?2))>0 OR EXISTS (
                    SELECT 1 FROM messages m WHERE m.conversation_id=c.id AND instr(lower(m.content),lower(?2))>0
                )) ORDER BY updated_at DESC,rowid DESC"
        ).map_err(|e| e.to_string())?;
        let ids = statement
            .query_map(params![archived, search.trim()], |row| row.get(0))
            .map_err(|e| e.to_string())?
            .collect::<std::result::Result<Vec<String>, _>>()
            .map_err(|e| e.to_string())?;
        self.arrange(ids)?
            .into_iter()
            .map(|id| self.conversation(&id))
            .collect()
    }

    /// 用户拖动保存的侧栏顺序（metadata.conversation_order）。
    fn conversation_order(&self) -> Result<Vec<String>> {
        let stored = self
            .connection
            .query_row(
                "SELECT value FROM metadata WHERE key='conversation_order'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        Ok(stored
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default())
    }

    /// 按最近活动排好的 id 再套用手动顺序：还没被安排过的（新会话）留在最前，
    /// 其余按用户拖出的先后排列。排序稳定，新会话之间仍按最近活动。
    fn arrange(&self, mut ids: Vec<String>) -> Result<Vec<String>> {
        let order = self.conversation_order()?;
        if order.is_empty() {
            return Ok(ids);
        }
        let rank: HashMap<&str, usize> = order
            .iter()
            .enumerate()
            .map(|(index, id)| (id.as_str(), index + 1))
            .collect();
        ids.sort_by_key(|id| rank.get(id.as_str()).copied().unwrap_or(0));
        Ok(ids)
    }

    /// 保存拖动后的顺序。`ids` 是当前可见的那部分会话的新先后：只重排它们原本占据的位置，
    /// 筛选、搜索或归档视图之外的会话位置不变。
    pub fn set_conversation_order(&mut self, ids: &[String]) -> Result<()> {
        let mut statement = self
            .connection
            .prepare("SELECT id FROM conversations ORDER BY updated_at DESC,rowid DESC")
            .map_err(|e| e.to_string())?;
        let all = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        drop(statement);
        let moved: HashSet<&str> = ids.iter().map(String::as_str).collect();
        if moved.len() != ids.len() || ids.iter().any(|id| !all.contains(id)) {
            return Err("会话顺序包含无效或重复的会话".into());
        }
        let mut next = ids.iter();
        let full = self
            .arrange(all)?
            .into_iter()
            .map(|id| {
                if moved.contains(id.as_str()) {
                    next.next().cloned().unwrap_or(id)
                } else {
                    id
                }
            })
            .collect::<Vec<_>>();
        self.connection
            .execute(
                "INSERT INTO metadata(key,value) VALUES('conversation_order',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [serde_json::to_string(&full).map_err(|e| e.to_string())?],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn detail(&self, id: &str) -> Result<ConversationDetail> {
        let conversation = self.conversation(id)?;
        let mut statement = self.connection.prepare("SELECT id,conversation_id,sender_id,content,status,created_at,thought FROM messages WHERE conversation_id=?1 ORDER BY sequence").map_err(|e| e.to_string())?;
        let mut messages = statement
            .query_map([id], message_from_row)
            .map_err(|e| e.to_string())?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        for message in &mut messages {
            if let Some(run_id) = self
                .connection
                .query_row(
                    "SELECT id FROM runs WHERE assistant_message_id=?1",
                    [&message.id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|e| e.to_string())?
            {
                message.tool_events = self.tool_events(&run_id)?;
            }
        }
        let mut statement = self.connection.prepare("SELECT agent_id,session_key,native_session_id,native_cwd,model,reasoning_effort FROM sessions WHERE conversation_id=?1 ORDER BY agent_id").map_err(|e| e.to_string())?;
        let sessions = statement
            .query_map([id], |row| {
                Ok(Session {
                    agent_id: row.get(0)?,
                    session_key: row.get(1)?,
                    native_session_id: row.get(2)?,
                    native_cwd: row.get(3)?,
                    model: row.get(4)?,
                    reasoning_effort: row.get(5)?,
                })
            })
            .map_err(|e| e.to_string())?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(ConversationDetail {
            conversation,
            messages,
            sessions,
            discussions: self.discussions(id)?,
            project: self.conversation_project(id)?,
            project_roles: self.conversation_project_roles(id)?,
            project_coordinator: self.project_coordinator(id)?,
            project_preference: self.project_preference(id),
            project_grants: self.project_grants(id),
            workflows: self.workflows(id)?,
            project_agent_chats: self.project_agent_chats(id)?,
            project_team_messages: self.team_messages(id)?,
            project_agent_inbox: self.project_chat_messages(id)?,
            project_questions: self.project_questions(id)?,
            chat_questions: self.chat_questions(id)?,
        })
    }

    /// 已被同席会话占用的原生会话：native_session_id → 占用它的会话标题。
    pub fn native_session_owners(&self, agent_id: &str) -> Result<HashMap<String, String>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT sessions.native_session_id, conversations.title \
                 FROM sessions JOIN conversations ON conversations.id = sessions.conversation_id \
                 WHERE sessions.agent_id=?1 AND sessions.native_session_id IS NOT NULL",
            )
            .map_err(|e| e.to_string())?;
        let owners = statement
            .query_map([agent_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| e.to_string())?
            .collect::<std::result::Result<std::collections::HashMap<_, _>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(owners)
    }

    /// 把某个 agent 的原生会话接进这个会话（接管）：只改绑定，不清空已有聊天记录。
    ///
    /// 占用规则（用户定）：同一个原生会话同一时刻只允许挂在一个同席会话上，
    /// 想换绑就先把原来那个换走，避免两处同时续同一个会话。
    pub fn attach_native_session(
        &mut self,
        id: &str,
        agent_id: &str,
        native_session_id: &str,
        native_cwd: Option<&str>,
    ) -> Result<()> {
        let native_session_id = native_session_id.trim();
        if native_session_id.is_empty() {
            return Err("原生会话 ID 不能为空".into());
        }
        let conversation = self.conversation(id)?;
        if conversation.archived {
            return Err("先恢复归档会话，再接入原生会话".into());
        }
        if !conversation.members.iter().any(|member| member == agent_id) {
            return Err("成员不在这个会话中".into());
        }
        let occupied = self
            .connection
            .query_row(
                "SELECT conversations.title FROM sessions \
                 JOIN conversations ON conversations.id = sessions.conversation_id \
                 WHERE sessions.agent_id=?1 AND sessions.native_session_id=?2 AND sessions.conversation_id<>?3",
                params![agent_id, native_session_id, id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if let Some(owner) = occupied.or(self.coordinator_occupies(agent_id, native_session_id, None)?) {
            return Err(format!("这个原生会话已被「{owner}」占用，先从那边换走"));
        }
        let changed = self
            .connection
            .execute(
                "UPDATE sessions SET native_session_id=?1, native_cwd=?2 WHERE conversation_id=?3 AND agent_id=?4",
                params![
                    native_session_id,
                    native_cwd.map(str::trim).filter(|value| !value.is_empty()),
                    id,
                    agent_id
                ],
            )
            .map_err(|e| e.to_string())?;
        if changed == 0 {
            return Err("这个会话里没有该成员".into());
        }
        self.connection
            .execute(
                "UPDATE conversations SET updated_at=?1 WHERE id=?2",
                params![now(), id],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn rename(&mut self, id: &str, title: &str) -> Result<Conversation> {
        let title = valid_title(title)?;
        self.conversation(id)?;
        self.connection
            .execute(
                "UPDATE conversations SET title=?1,updated_at=?2 WHERE id=?3",
                params![title, now(), id],
            )
            .map_err(|e| e.to_string())?;
        self.conversation(id)
    }

    pub fn set_session_settings(
        &mut self,
        id: &str,
        agent_id: &str,
        model: Option<String>,
        reasoning_effort: Option<String>,
    ) -> Result<ConversationDetail> {
        let conversation = self.conversation(id)?;
        if conversation.archived {
            return Err("先恢复归档会话，再修改模型设置".into());
        }
        if !conversation.members.iter().any(|member| member == agent_id) {
            return Err("成员不在这个会话中".into());
        }
        let model = model
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        if model
            .as_ref()
            .is_some_and(|value| value.chars().count() > 200 || value.chars().any(char::is_control))
        {
            return Err("模型名称过长或包含控制字符".into());
        }
        let reasoning_effort = reasoning_effort.filter(|value| !value.is_empty());
        if reasoning_effort.as_ref().is_some_and(|value| {
            ![
                "none", "off", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
            ]
            .contains(&value.as_str())
        }) {
            return Err("不支持的思考强度".into());
        }
        self.connection.execute("UPDATE sessions SET model=?1,reasoning_effort=?2 WHERE conversation_id=?3 AND agent_id=?4", params![model,reasoning_effort,id,agent_id]).map_err(|e| e.to_string())?;
        self.detail(id)
    }

    pub fn archive(&mut self, id: &str, archived: bool) -> Result<Conversation> {
        self.conversation(id)?;
        self.guard_discussion(id)?;
        self.guard_workflow(id)?;
        self.connection
            .execute(
                "UPDATE conversations SET archived=?1,updated_at=?2 WHERE id=?3",
                params![archived, now(), id],
            )
            .map_err(|e| e.to_string())?;
        self.conversation(id)
    }

    pub fn delete(&mut self, id: &str) -> Result<()> {
        let conversation = self.conversation(id)?;
        self.guard_discussion(id)?;
        self.guard_workflow(id)?;
        let project_chats = if conversation.kind == "group" {
            self.project_agent_chats(id)?
        } else {
            Vec::new()
        };
        for chat in &project_chats {
            let active: bool = self.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM runs WHERE conversation_id=?1 AND status IN ('starting','running','cancelling'))",
                [&chat.conversation_id], |row| row.get(0),
            ).map_err(|_| "项目私聊状态不可用")?;
            if active {
                return Err("项目内 Agent 私聊仍在运行，请先停止后再删除群聊".into());
            }
        }
        let tx = self.connection.transaction().map_err(|e| e.to_string())?;
        for chat in project_chats {
            tx.execute(
                "DELETE FROM conversations WHERE id=?1",
                [chat.conversation_id],
            )
            .map_err(|e| e.to_string())?;
        }
        tx.execute("DELETE FROM conversations WHERE id=?1", [id])
            .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn replace_members(&mut self, id: &str, members: &[String]) -> Result<Conversation> {
        let conversation = self.conversation(id)?;
        self.guard_discussion(id)?;
        self.guard_workflow(id)?;
        if conversation.kind != "group" {
            return Err("私聊成员固定，请另建会话".into());
        }
        valid_members("group", members)?;
        // 只有被移出的成员牵涉到时才重置：调度管家被移出就回到默认，角色里有人被移出就清掉角色。
        let coordinator_gone = self.project_coordinator(id)?.is_some_and(|choice| !members.contains(&choice.agent));
        let roles_gone = self.conversation_project_roles(id).ok().flatten().is_some_and(|roles| {
            [&roles.plan.agent, &roles.implement.agent, &roles.review.agent].iter().any(|agent| !members.contains(agent))
        });
        let tx = self.connection.transaction().map_err(|e| e.to_string())?;
        if coordinator_gone {
            tx.execute(
                "DELETE FROM project_coordinator_settings WHERE conversation_id=?1",
                [id],
            )
            .map_err(|_| "管家配置重置失败")?;
        }
        if roles_gone {
            tx.execute(
                "UPDATE conversation_projects SET roles=NULL WHERE conversation_id=?1",
                [id],
            )
            .map_err(|e| e.to_string())?;
        }
        for previous in &conversation.members {
            if !members.contains(previous) {
                tx.execute(
                    "DELETE FROM members WHERE conversation_id=?1 AND agent_id=?2",
                    params![id, previous],
                )
                .map_err(|e| e.to_string())?;
            }
        }
        for (position, agent_id) in members.iter().enumerate() {
            tx.execute("INSERT INTO members(conversation_id,agent_id,position) VALUES(?1,?2,?3) ON CONFLICT(conversation_id,agent_id) DO UPDATE SET position=excluded.position", params![id,agent_id,position as i64]).map_err(|e| e.to_string())?;
            tx.execute("INSERT OR IGNORE INTO sessions(conversation_id,agent_id,session_key) VALUES(?1,?2,?3)", params![id,agent_id,Uuid::new_v4().to_string()]).map_err(|e| e.to_string())?;
        }
        tx.execute(
            "UPDATE conversations SET updated_at=?1 WHERE id=?2",
            params![now(), id],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        self.conversation(id)
    }

    pub fn run_for_message(&self, message_id: &str) -> Result<Option<RunRecord>> {
        self.connection.query_row(
            "SELECT id,conversation_id,user_message_id,assistant_message_id,status,native_thread_id,native_turn_id,error,agent_id,model,reasoning_effort,discussion_id,round FROM runs WHERE user_message_id=?1 AND discussion_id IS NULL", [message_id],
            crate::group_store::run_from_row
        ).optional().map_err(|e| e.to_string())
    }

    /// 因额度或频率限制失败、之后会话里再没有新消息和新运行的私聊运行：
    /// (会话 ID, 错误摘要, 失败那条回复的创建时间毫秒)。运行中插话（steer）记在回复的思考标记里，不算接手。
    pub fn quota_stalled_runs(&self, agent_id: &str) -> Result<Vec<(String, String, i64)>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT r.conversation_id,r.error,a.created_at FROM runs r
                 JOIN conversations c ON c.id=r.conversation_id AND c.archived=0
                 JOIN messages a ON a.id=r.assistant_message_id
                 WHERE r.agent_id=?1 AND r.status='failed' AND r.discussion_id IS NULL
                   AND (r.error LIKE '%额度限制%' OR r.error LIKE '%请求频率限制%')
                   AND NOT EXISTS(SELECT 1 FROM runs n WHERE n.conversation_id=r.conversation_id AND n.rowid>r.rowid)
                   AND NOT EXISTS(SELECT 1 FROM messages m WHERE m.conversation_id=r.conversation_id AND m.sequence>a.sequence AND instr(coalesce(a.thought,''),m.id)=0)",
            )
            .map_err(|e| e.to_string())?;
        let rows = statement
            .query_map([agent_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(|e| e.to_string())
    }

    pub fn begin_run(
        &mut self,
        conversation_id: &str,
        message_id: &str,
        content: &str,
    ) -> Result<RunRecord> {
        self.begin_agent_run(conversation_id, message_id, content, "codex-win")
    }

    pub fn begin_agent_run(
        &mut self,
        conversation_id: &str,
        message_id: &str,
        content: &str,
        agent_id: &str,
    ) -> Result<RunRecord> {
        self.validate_private_target(conversation_id, agent_id)?;
        // 交给 CLI 期间连消息也不存，免得拦下的话混进聊天记录。
        self.guard_cli_handoff(conversation_id, agent_id)?;
        self.guard_project_task(conversation_id)?;
        self.save_message(conversation_id, message_id, content)?;
        if let Some(existing) = self.run_for_message(message_id)? {
            return Ok(existing);
        }
        self.insert_run(conversation_id, message_id, agent_id, None, None)
    }

    pub(crate) fn insert_run(
        &mut self,
        conversation_id: &str,
        message_id: &str,
        agent_id: &str,
        discussion_id: Option<&str>,
        round: Option<u32>,
    ) -> Result<RunRecord> {
        self.guard_service_maintenance(agent_id)?;
        self.guard_cli_handoff(conversation_id, agent_id)?;
        // 项目任务与私聊走各自的原生会话，可以同时进行，不再互相等待。
        let settings = self
            .detail(conversation_id)?
            .sessions
            .into_iter()
            .find(|session| session.agent_id == agent_id)
            .ok_or("会话设置不存在")?;
        let record = RunRecord {
            id: Uuid::new_v4().to_string(),
            agent_id: agent_id.into(),
            conversation_id: conversation_id.into(),
            user_message_id: message_id.into(),
            assistant_message_id: Uuid::new_v4().to_string(),
            status: "starting".into(),
            native_thread_id: None,
            native_turn_id: None,
            error: None,
            model: settings.model,
            reasoning_effort: settings.reasoning_effort,
            discussion_id: discussion_id.map(String::from),
            round,
        };
        let tx = self.connection.transaction().map_err(|e| e.to_string())?;
        tx.execute(
            "UPDATE messages SET status='pending' WHERE id=?1 AND status='local_only'",
            [message_id],
        )
        .map_err(|e| e.to_string())?;
        tx.execute("INSERT INTO messages(id,conversation_id,sender_id,content,status,created_at) VALUES(?1,?2,?3,'','streaming',?4)", params![record.assistant_message_id,conversation_id,agent_id,now()]).map_err(|e| e.to_string())?;
        tx.execute("INSERT INTO runs(id,conversation_id,agent_id,user_message_id,assistant_message_id,status,model,reasoning_effort,discussion_id,round) VALUES(?1,?2,?3,?4,?5,'starting',?6,?7,?8,?9)", params![record.id,conversation_id,agent_id,message_id,record.assistant_message_id,record.model,record.reasoning_effort,discussion_id,round]).map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(record)
    }

    pub fn append_tool_event(&mut self, record: &RunRecord, event: &RunToolEvent) -> Result<()> {
        if event.event_id.is_empty()
            || event.event_id.chars().count() > 160
            || event.kind.is_empty()
            || event.kind.chars().count() > 80
            || event.title.chars().count() > 500
            || event
                .input
                .as_ref()
                .is_some_and(|value| value.len() > 16_000)
            || event
                .output
                .as_ref()
                .is_some_and(|value| value.len() > 16_000)
            || !matches!(event.status.as_str(), "running" | "completed" | "failed")
        {
            return Err("工具活动记录无效".into());
        }
        let belongs: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM runs WHERE id=?1 AND assistant_message_id=?2 AND conversation_id=?3)",
            params![record.id,record.assistant_message_id,record.conversation_id], |row| row.get(0),
        ).map_err(|e| e.to_string())?;
        if !belongs {
            return Err("工具活动不属于当前会话运行".into());
        }
        self.connection.execute(
            "INSERT INTO run_tool_events(run_id,event_id,kind,title,input,output,status,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(run_id,event_id) DO UPDATE SET kind=excluded.kind,title=excluded.title,input=excluded.input,output=excluded.output,status=excluded.status",
            params![record.id,event.event_id,event.kind,event.title,event.input,event.output,event.status,event.created_at],
        ).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn tool_events(&self, run_id: &str) -> Result<Vec<RunToolEvent>> {
        let mut statement = self.connection.prepare(
            "SELECT event_id,kind,title,input,output,status,created_at FROM run_tool_events WHERE run_id=?1 ORDER BY created_at,event_id",
        ).map_err(|e| e.to_string())?;
        let rows = statement
            .query_map([run_id], |row| {
                Ok(RunToolEvent {
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

    pub fn bind_thread(&mut self, record: &mut RunRecord, thread_id: &str) -> Result<()> {
        let tx = self.connection.transaction().map_err(|e| e.to_string())?;
        let changed = tx
            .execute(
                "UPDATE sessions SET native_session_id=?1 WHERE conversation_id=?2 AND agent_id=?3",
                params![thread_id, record.conversation_id, record.agent_id],
            )
            .map_err(|e| e.to_string())?;
        if changed != 1 {
            return Err("原生会话映射不存在".into());
        }
        tx.execute(
            "UPDATE runs SET native_thread_id=?1 WHERE id=?2",
            params![thread_id, record.id],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        record.native_thread_id = Some(thread_id.into());
        Ok(())
    }

    pub fn mark_turn(&mut self, record: &mut RunRecord, turn_id: &str) -> Result<()> {
        record.native_turn_id = Some(turn_id.into());
        if record.status != "cancelling" {
            record.status = "running".into();
        }
        let tx = self.connection.transaction().map_err(|e| e.to_string())?;
        tx.execute(
            "UPDATE runs SET native_turn_id=?1,status=?2 WHERE id=?3",
            params![turn_id, record.status, record.id],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "UPDATE messages SET status='delivered' WHERE id=?1",
            [&record.user_message_id],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())
    }

    pub fn record_model(
        &mut self,
        record: &mut RunRecord,
        model: Option<String>,
        effort: Option<String>,
    ) -> Result<()> {
        self.connection
            .execute(
                "UPDATE runs SET model=?1,reasoning_effort=?2 WHERE id=?3",
                params![model, effort, record.id],
            )
            .map_err(|e| e.to_string())?;
        record.model = model;
        record.reasoning_effort = effort;
        Ok(())
    }

    pub fn checkpoint(&mut self, record: &RunRecord, content: &str, thought: &str) -> Result<()> {
        let tx = self.connection.transaction().map_err(|e| e.to_string())?;
        tx.execute(
            "UPDATE runs SET status=?1,error=?2 WHERE id=?3",
            params![record.status, record.error, record.id],
        )
        .map_err(|e| e.to_string())?;
        let status = if matches!(
            record.status.as_str(),
            "starting" | "running" | "cancelling"
        ) {
            "streaming"
        } else {
            &record.status
        };
        tx.execute(
            "UPDATE messages SET content=?1,status=?2,thought=?3 WHERE id=?4",
            params![content, status, thought, record.assistant_message_id],
        )
        .map_err(|e| e.to_string())?;
        if matches!(
            record.status.as_str(),
            "completed" | "interrupted" | "failed"
        ) {
            tx.execute(
                "UPDATE messages SET status=?1 WHERE id=?2 AND status='pending'",
                params![
                    if record.status == "completed" {
                        "delivered"
                    } else {
                        "failed"
                    },
                    record.user_message_id
                ],
            )
            .map_err(|e| e.to_string())?;
        }
        tx.execute(
            "UPDATE conversations SET updated_at=?1 WHERE id=?2",
            params![now(), record.conversation_id],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        self.capture_chat_question(record, content)
    }

    fn recover_runs(&mut self) -> Result<()> {
        self.connection.execute_batch("UPDATE chat_questions SET status='cancelled',delivery='cancelled' WHERE status='pending' AND (mode='native' OR run_id IN (SELECT id FROM runs WHERE status IN ('starting','running','cancelling','failed','interrupted'))); UPDATE chat_questions SET delivery='sent' WHERE delivery='queued' AND answer_message_id IN (SELECT user_message_id FROM runs); UPDATE chat_questions SET status='pending',delivery='awaiting' WHERE delivery='queued';") .map_err(|e|e.to_string())?;
        self.connection.execute_batch(
            "UPDATE messages SET status='interrupted' WHERE id IN (SELECT assistant_message_id FROM runs WHERE status IN ('starting','running','cancelling'));
             UPDATE messages SET status='failed' WHERE status='pending' AND id IN (SELECT user_message_id FROM runs WHERE status IN ('starting','running','cancelling'));
             UPDATE runs SET status='interrupted',error='上次运行因软件退出而中断，请重新发送新消息' WHERE status IN ('starting','running','cancelling');
             UPDATE messages SET status='failed' WHERE status='pending' AND id IN (SELECT user_message_id FROM discussions WHERE status IN ('running','cancelling'));
             UPDATE discussions SET status='interrupted',error='上次讨论因软件退出而中断，不会自动重放',updated_at=CAST(strftime('%s','now') AS INTEGER)*1000 WHERE status IN ('running','cancelling');"
        ).map_err(|e| e.to_string())
    }

    pub fn save_message(
        &mut self,
        conversation_id: &str,
        message_id: &str,
        content: &str,
    ) -> Result<Message> {
        if Uuid::parse_str(message_id).is_err() {
            return Err("消息 ID 无效".into());
        }
        let content = content.trim();
        if content.is_empty() || content.chars().count() > 16_000 {
            return Err("消息需为 1–16000 个字符".into());
        }
        let conversation = self.conversation(conversation_id)?;
        if conversation.archived {
            return Err("先恢复归档会话，再保存消息".into());
        }
        self.guard_discussion(conversation_id)?;
        self.guard_workflow(conversation_id)?;
        let existing = self.connection.query_row("SELECT id,conversation_id,sender_id,content,status,created_at,thought FROM messages WHERE id=?1", [message_id], message_from_row).optional().map_err(|e| e.to_string())?;
        if let Some(existing) = existing {
            if existing.conversation_id == conversation_id && existing.content == content {
                return Ok(existing);
            }
            return Err("消息 ID 已用于其他内容，不能重复保存".into());
        }
        let message = Message {
            id: message_id.into(),
            conversation_id: conversation_id.into(),
            sender_id: "user".into(),
            content: content.into(),
            thought: String::new(),
            tool_events: Vec::new(),
            status: "local_only".into(),
            created_at: now(),
        };
        let tx = self.connection.transaction().map_err(|e| e.to_string())?;
        tx.execute("INSERT INTO messages(id,conversation_id,sender_id,content,status,created_at) VALUES(?1,?2,?3,?4,?5,?6)", params![message.id,message.conversation_id,message.sender_id,message.content,message.status,message.created_at]).map_err(|e| e.to_string())?;
        tx.execute(
            "UPDATE conversations SET updated_at=?1 WHERE id=?2",
            params![message.created_at, conversation_id],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(message)
    }

    /// 已经交给原生会话的用户消息（如回复中途的插话）：保存并直接标为已提交。
    pub fn save_delivered_message(
        &mut self,
        conversation_id: &str,
        message_id: &str,
        content: &str,
    ) -> Result<Message> {
        let mut message = self.save_message(conversation_id, message_id, content)?;
        self.connection
            .execute(
                "UPDATE messages SET status='delivered' WHERE id=?1",
                [message_id],
            )
            .map_err(|e| e.to_string())?;
        message.status = "delivered".into();
        Ok(message)
    }

    /// 移交记录：(会话, 成员, 原生会话, CLI 窗口进程号, 移交时刻毫秒)。
    pub(crate) fn saved_cli_handoffs(&self) -> Result<Vec<(String, String, String, u32, i64)>> {
        let mut statement = self
            .connection
            .prepare("SELECT conversation_id,agent_id,native_session_id,pid,since FROM cli_handoffs")
            .map_err(|e| e.to_string())?;
        let rows = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)))
            .map_err(|e| e.to_string())?;
        rows.collect::<std::result::Result<_, _>>().map_err(|e| e.to_string())
    }

    pub(crate) fn save_cli_handoff(&mut self, conversation: &str, agent: &str, native: &str, pid: u32, since: i64) -> Result<()> {
        self.connection
            .execute(
                "INSERT INTO cli_handoffs(conversation_id,agent_id,native_session_id,pid,since) VALUES(?1,?2,?3,?4,?5)
                 ON CONFLICT(conversation_id,agent_id) DO UPDATE SET native_session_id=?3,pid=?4,since=?5",
                params![conversation, agent, native, pid, since],
            )
            .map_err(|e| e.to_string())?;
        self.cli_handoffs.insert((conversation.to_owned(), agent.to_owned()));
        Ok(())
    }

    pub(crate) fn set_cli_handoff_pid(&mut self, conversation: &str, agent: &str, pid: u32) -> Result<()> {
        self.connection
            .execute("UPDATE cli_handoffs SET pid=?3 WHERE conversation_id=?1 AND agent_id=?2", params![conversation, agent, pid])
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub(crate) fn clear_cli_handoff(&mut self, conversation: &str, agent: &str) -> Result<()> {
        self.cli_handoffs.remove(&(conversation.to_owned(), agent.to_owned()));
        self.connection
            .execute("DELETE FROM cli_handoffs WHERE conversation_id=?1 AND agent_id=?2", params![conversation, agent])
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// 把 CLI 里新增的对话补进同席的聊天记录（人说的话记为已送达，成员回复记为已完成）。
    pub(crate) fn import_cli_turns(&mut self, conversation: &str, agent: &str, turns: &[crate::cli_transcript::Turn]) -> Result<usize> {
        let tx = self.connection.transaction().map_err(|e| e.to_string())?;
        let mut latest = 0;
        for turn in turns {
            let (sender, status) = if turn.user { ("user", "delivered") } else { (agent, "completed") };
            tx.execute(
                "INSERT INTO messages(id,conversation_id,sender_id,content,status,created_at,thought) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![Uuid::new_v4().to_string(), conversation, sender, turn.text, status, turn.at, turn.thought],
            )
            .map_err(|e| e.to_string())?;
            latest = latest.max(turn.at);
        }
        if latest > 0 {
            tx.execute("UPDATE conversations SET updated_at=MAX(updated_at,?1) WHERE id=?2", params![latest, conversation])
                .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(turns.len())
    }

    /// 同席给某个成员定的默认模型（会话没单独选模型时用它，而不是 harness 自己的默认）。
    pub(crate) fn agent_default_model(&self, agent: &str) -> Option<String> {
        self.connection
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [format!("default_model:{agent}")],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten()
    }

    pub(crate) fn set_agent_default_model(&mut self, agent: &str, model: Option<&str>) -> Result<()> {
        let key = format!("default_model:{agent}");
        match model {
            Some(model) => self.connection.execute(
                "INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=?2",
                params![key, model],
            ),
            None => self.connection.execute("DELETE FROM metadata WHERE key=?1", [key]),
        }
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub(crate) fn guard_cli_handoff(&self, conversation: &str, agent: &str) -> Result<()> {
        if self
            .cli_handoffs
            .contains(&(conversation.to_owned(), agent.to_owned()))
        {
            return Err("这条会话正在 CLI 中使用，同席已暂停；关闭 CLI 窗口或点“收回到同席”后再发".into());
        }
        Ok(())
    }

    pub(crate) fn guard_service_maintenance(&self, agent: &str) -> Result<()> {
        if self.service_maintenance.contains(agent) {
            return Err("该代理正在维护，请稍后再试".into());
        }
        Ok(())
    }

    pub(crate) fn begin_service_maintenance(&mut self, agent: &str) -> Result<()> {
        self.guard_service_maintenance(agent)?;
        if self.agent_busy(agent)? {
            return Err("该代理仍有进行中的会话或任务，暂不能进入维护".into());
        }
        self.service_maintenance.insert(agent.to_owned());
        Ok(())
    }

    /// 显式断开：仍允许停止本软件自己的 private run，
    /// 但不允许停止正在群聊或项目任务中工作的成员。
    pub(crate) fn begin_service_shutdown(&mut self, agent: &str) -> Result<()> {
        self.guard_service_maintenance(agent)?;
        if self.agent_work_busy(agent)? {
            return Err("该代理仍参与进行中的群聊或项目任务，暂不能断开".into());
        }
        self.service_maintenance.insert(agent.to_owned());
        Ok(())
    }

    pub(crate) fn end_service_maintenance(&mut self, agent: &str) -> Result<()> {
        self.service_maintenance.remove(agent);
        Ok(())
    }

    fn agent_busy(&self, agent: &str) -> Result<bool> {
        let active_run = self
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM runs WHERE agent_id=?1 AND status IN ('starting','running','cancelling'))",
                [agent],
                |row| row.get::<_, bool>(0),
            )
            .map_err(|e| e.to_string())?;
        if active_run {
            return Ok(true);
        }
        self.agent_work_busy(agent)
    }

    /// 群聊/项目工作是否占用该代理。任何 SQL 或 JSON 解析失败都以 Err 失败关闭。
    fn agent_work_busy(&self, agent: &str) -> Result<bool> {
        // 固定 schema：workflows.conversation_id 与 members.conversation_id 直接关联。
        let workflow_busy = self
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM workflows w JOIN members m ON m.conversation_id=w.conversation_id WHERE w.status IN ('queued','planning','running','verifying','reviewing','cancelling') AND m.agent_id=?1)",
                [agent],
                |row| row.get::<_, bool>(0),
            )
            .map_err(|e| e.to_string())?;
        if workflow_busy {
            return Ok(true);
        }
        self.connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM discussions d WHERE d.status IN ('running','cancelling') AND EXISTS(SELECT 1 FROM json_each(d.participants) WHERE json_each.value=?1))",
                [agent],
                |row| row.get::<_, bool>(0),
            )
            .map_err(|e| e.to_string())
    }
}

fn message_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    Ok(Message {
        id: row.get(0)?,
        conversation_id: row.get(1)?,
        sender_id: row.get(2)?,
        content: row.get(3)?,
        status: row.get(4)?,
        created_at: row.get(5)?,
        thought: row.get(6)?,
        tool_events: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_settings_are_scoped_to_member_and_conversation() {
        let mut store = memory();
        let private = direct(&mut store, "私聊");
        let group = store
            .create("群聊", "group", &["codex-win".into(), "hermes-win".into()])
            .unwrap();
        let saved = store
            .set_session_settings(
                &group.id,
                "codex-win",
                Some("example-model".into()),
                Some("high".into()),
            )
            .unwrap();
        assert_eq!(
            saved
                .sessions
                .iter()
                .find(|session| session.agent_id == "codex-win")
                .unwrap()
                .model
                .as_deref(),
            Some("example-model")
        );
        assert!(saved
            .sessions
            .iter()
            .find(|session| session.agent_id == "hermes-win")
            .unwrap()
            .model
            .is_none());
        assert!(store.detail(&private.id).unwrap().sessions[0]
            .model
            .is_none());
        assert!(store
            .set_session_settings(&group.id, "dsh-win", None, None)
            .is_err());
        assert!(store
            .set_session_settings(&group.id, "codex-win", None, Some("typo".into()))
            .is_err());
        assert_eq!(
            store
                .detail(&group.id)
                .unwrap()
                .sessions
                .iter()
                .find(|session| session.agent_id == "codex-win")
                .unwrap()
                .reasoning_effort
                .as_deref(),
            Some("high")
        );
    }

    #[test]
    fn live_settings_preserve_running_snapshot_and_block_archived_conversation() {
        let mut store = memory();
        let room = direct(&mut store, "运行设置");
        store
            .set_session_settings(
                &room.id,
                "codex-win",
                Some("example".into()),
                Some("low".into()),
            )
            .unwrap();
        let mut run = store
            .begin_run(&room.id, &Uuid::new_v4().to_string(), "请求")
            .unwrap();
        store
            .set_session_settings(
                &room.id,
                "codex-win",
                Some("next-model".into()),
                Some("high".into()),
            )
            .unwrap();
        let stored_run = store
            .run_for_message(&run.user_message_id)
            .unwrap()
            .unwrap();
        assert_eq!(stored_run.model.as_deref(), Some("example"));
        assert_eq!(stored_run.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(
            store.detail(&room.id).unwrap().sessions[0].model.as_deref(),
            Some("next-model")
        );
        run.status = "completed".into();
        store.checkpoint(&run, "完成", "").unwrap();
        let reset = store
            .set_session_settings(&room.id, "codex-win", None, None)
            .unwrap();
        assert!(reset.sessions[0].model.is_none() && reset.sessions[0].reasoning_effort.is_none());
        store.archive(&room.id, true).unwrap();
        assert!(store
            .set_session_settings(&room.id, "codex-win", None, None)
            .is_err());
    }

    #[test]
    fn v2_migration_preserves_native_runs_and_session_identity() {
        let mut store = memory();
        let room = direct(&mut store, "旧会话");
        let mut run = store
            .begin_run(&room.id, &Uuid::new_v4().to_string(), "旧消息")
            .unwrap();
        store.bind_thread(&mut run, "old-native-thread").unwrap();
        run.status = "completed".into();
        store.checkpoint(&run, "旧回复", "").unwrap();
        let old = store.detail(&room.id).unwrap();
        store.connection.execute_batch("ALTER TABLE sessions DROP COLUMN model; ALTER TABLE sessions DROP COLUMN reasoning_effort; DROP INDEX IF EXISTS one_active_agent; DROP INDEX IF EXISTS one_active_agent_conversation; CREATE UNIQUE INDEX one_active_codex ON runs(agent_id) WHERE status IN ('starting','running','cancelling'); PRAGMA user_version=2;").unwrap();
        let store = Store::initialize(store.connection, PathBuf::from(":memory:")).unwrap();
        let migrated = store.detail(&room.id).unwrap();
        assert_eq!(
            migrated.sessions[0].session_key,
            old.sessions[0].session_key
        );
        assert_eq!(
            migrated.sessions[0].native_session_id.as_deref(),
            Some("old-native-thread")
        );
        assert_eq!(migrated.messages[1].content, "旧回复");
        assert_eq!(
            store
                .run_for_message(&run.user_message_id)
                .unwrap()
                .unwrap()
                .status,
            "completed"
        );
        assert!(migrated.sessions[0].model.is_none());
    }

    #[test]
    fn hermes_runs_have_separate_identity_and_restore_settings() {
        let path = std::env::temp_dir().join(format!("hub-test-{}.db", Uuid::new_v4()));
        let id;
        {
            let mut store = Store::open(&path).unwrap();
            let room = store
                .create("Hermes", "direct", &["hermes-win".into()])
                .unwrap();
            id = room.id;
            store
                .set_session_settings(
                    &id,
                    "hermes-win",
                    Some("provider:model".into()),
                    Some("medium".into()),
                )
                .unwrap();
            let mut run = store
                .begin_agent_run(&id, &Uuid::new_v4().to_string(), "请求", "hermes-win")
                .unwrap();
            store.bind_thread(&mut run, "hermes-thread").unwrap();
            run.status = "completed".into();
            store.checkpoint(&run, "回复", "").unwrap();
            assert_eq!(
                store.detail(&id).unwrap().messages[1].sender_id,
                "hermes-win"
            );
        }
        let store = Store::open(&path).unwrap();
        let detail = store.detail(&id).unwrap();
        let session = &detail.sessions[0];
        assert_eq!(session.model.as_deref(), Some("provider:model"));
        assert_eq!(session.reasoning_effort.as_deref(), Some("medium"));
        assert_eq!(session.native_session_id.as_deref(), Some("hermes-thread"));
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn v3_upgrade_preserves_profiles_and_runs_and_admits_dsh() {
        let mut store = memory();
        let room = direct(&mut store, "旧配置");
        store
            .set_session_settings(
                &room.id,
                "codex-win",
                Some("original-model".into()),
                Some("low".into()),
            )
            .unwrap();
        let mut run = store
            .begin_run(&room.id, &Uuid::new_v4().to_string(), "旧请求")
            .unwrap();
        store.bind_thread(&mut run, "original-thread").unwrap();
        run.status = "completed".into();
        store.checkpoint(&run, "保留内容", "").unwrap();
        store
            .connection
            .execute_batch("PRAGMA user_version=3;")
            .unwrap();
        let mut upgraded = Store::initialize(store.connection, PathBuf::from(":memory:")).unwrap();
        let detail = upgraded.detail(&room.id).unwrap();
        assert_eq!(detail.sessions[0].model.as_deref(), Some("original-model"));
        assert_eq!(
            detail.sessions[0].native_session_id.as_deref(),
            Some("original-thread")
        );
        assert_eq!(detail.messages[1].content, "保留内容");
        let dsh = upgraded
            .create("DSH", "direct", &["dsh-win".into()])
            .unwrap();
        let dsh_run = upgraded
            .begin_agent_run(&dsh.id, &Uuid::new_v4().to_string(), "新请求", "dsh-win")
            .unwrap();
        assert_eq!(dsh_run.agent_id, "dsh-win");
        assert!(upgraded
            .begin_agent_run(&dsh.id, &Uuid::new_v4().to_string(), "重复运行", "dsh-win")
            .is_err());
        assert!(upgraded
            .connection
            .query_row("PRAGMA foreign_key_check", [], |_| Ok(()))
            .optional()
            .unwrap()
            .is_none());
    }
    #[test]
    fn service_maintenance_begin_is_exclusive_until_end_and_busy_chat_blocks_it() {
        let mut store = memory();
        store.guard_service_maintenance("dsh-win").unwrap();
        store.begin_service_maintenance("dsh-win").unwrap();
        assert!(store.guard_service_maintenance("dsh-win").is_err());
        assert!(store.begin_service_maintenance("dsh-win").is_err());
        store.end_service_maintenance("dsh-win").unwrap();
        store.guard_service_maintenance("dsh-win").unwrap();
        store.begin_service_maintenance("dsh-win").unwrap();
        store.end_service_maintenance("dsh-win").unwrap();
        let room = store
            .list("", false)
            .unwrap()
            .into_iter()
            .find(|room| {
                room.kind == "direct" && room.members.iter().any(|member| member == "dsh-win")
            })
            .unwrap();
        store
            .begin_agent_run(
                &room.id,
                &Uuid::new_v4().to_string(),
                "维护期间的私聊请求",
                "dsh-win",
            )
            .unwrap();
        assert!(store.begin_service_maintenance("dsh-win").is_err());
        store.guard_service_maintenance("dsh-win").unwrap();
        assert!(store.begin_service_maintenance("hermes-win").is_ok());
    }

    #[test]
    fn disconnect_shutdown_allows_own_private_run_and_end_clears_flag() {
        let mut store = memory();
        let room = store
            .list("", false)
            .unwrap()
            .into_iter()
            .find(|room| {
                room.kind == "direct" && room.members.iter().any(|member| member == "dsh-win")
            })
            .unwrap();
        let run = store
            .begin_agent_run(
                &room.id,
                &Uuid::new_v4().to_string(),
                "断开前的私聊",
                "dsh-win",
            )
            .unwrap();
        assert_eq!(run.agent_id, "dsh-win");
        // 严格维护仍拒绝进行中的私聊运行。
        assert!(store.begin_service_maintenance("dsh-win").is_err());
        // 显式断开允许停止本软件自己的私聊运行。
        store.begin_service_shutdown("dsh-win").unwrap();
        assert!(store.begin_service_shutdown("dsh-win").is_err());
        assert!(store.guard_service_maintenance("dsh-win").is_err());
        store.end_service_maintenance("dsh-win").unwrap();
        store.guard_service_maintenance("dsh-win").unwrap();
        // 私聊运行仍在进行，严格维护路径依旧被拒绝。
        assert!(store.begin_service_maintenance("dsh-win").is_err());
    }

    fn memory() -> Store {
        Store::initialize(
            Connection::open_in_memory().unwrap(),
            PathBuf::from(":memory:"),
        )
        .unwrap()
    }
    fn direct(store: &mut Store, name: &str) -> Conversation {
        store.create(name, "direct", &["codex-win".into()]).unwrap()
    }
    fn message(store: &mut Store, id: &str, text: &str) -> Message {
        store
            .save_message(id, &Uuid::new_v4().to_string(), text)
            .unwrap()
    }

    /// 把当前库退回 v17 的样子：约束里去掉 Claude、删掉 Claude 私聊。
    fn downgrade_to_v17(store: &Store) {
        let tables: Vec<(String, String)> = store
            .connection
            .prepare(
                "SELECT name,sql FROM sqlite_schema WHERE type='table' AND sql LIKE '%claude-win%'",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert!(!tables.is_empty());
        let version: i64 = store
            .connection
            .query_row("PRAGMA schema_version", [], |row| row.get(0))
            .unwrap();
        store
            .connection
            .execute("DELETE FROM conversations WHERE title='与 Claude 私聊'", [])
            .unwrap();
        store
            .connection
            .execute_batch("PRAGMA writable_schema=ON;")
            .unwrap();
        for (name, sql) in tables {
            store
                .connection
                .execute(
                    "UPDATE sqlite_schema SET sql=?1 WHERE type='table' AND name=?2",
                    params![sql.replace(",'claude-win'", ""), name],
                )
                .unwrap();
        }
        store
            .connection
            .execute_batch(&format!(
                "PRAGMA schema_version={}; PRAGMA writable_schema=OFF; PRAGMA user_version=17;",
                version + 1
            ))
            .unwrap();
    }

    #[test]
    fn dragged_order_persists_and_only_permutes_the_visible_subset() {
        let mut store = memory();
        let ids = |store: &Store| -> Vec<String> {
            store
                .list("", false)
                .unwrap()
                .into_iter()
                .map(|room| room.id)
                .collect()
        };
        let before = ids(&store);
        // 把最后一个拖到最前。
        let mut moved = before.clone();
        let last = moved.pop().unwrap();
        moved.insert(0, last.clone());
        store.set_conversation_order(&moved).unwrap();
        assert_eq!(ids(&store), moved);
        // 新消息不再把会话顶上去：手动顺序优先于最近活动。
        message(&mut store, &moved[3], "有新消息");
        assert_eq!(ids(&store), moved);
        // 只重排一个子集（例如筛选后的两项互换），其他位置不动。
        let subset = vec![moved[4].clone(), moved[1].clone()];
        store.set_conversation_order(&subset).unwrap();
        let mut expected = moved.clone();
        expected.swap(1, 4);
        assert_eq!(ids(&store), expected);
        // 新建的会话出现在最前。
        let fresh = direct(&mut store, "新会话");
        assert_eq!(ids(&store)[0], fresh.id);
        assert!(store
            .set_conversation_order(&[fresh.id.clone(), fresh.id.clone()])
            .is_err());
        assert!(store.set_conversation_order(&["missing".into()]).is_err());
    }

    #[test]
    fn v18_upgrade_admits_claude_without_touching_existing_rows() {
        let root = std::env::temp_dir().join(format!("agent-hub-v18-{}", Uuid::new_v4()));
        let path = root.join("hub.db");
        let mut store = Store::open(&path).unwrap();
        let room = direct(&mut store, "旧私聊");
        let run = store
            .begin_agent_run(&room.id, &Uuid::new_v4().to_string(), "旧问题", "codex-win")
            .unwrap();
        store
            .append_tool_event(
                &run,
                &RunToolEvent {
                    event_id: "tool-1".into(),
                    kind: "read".into(),
                    title: "读取".into(),
                    input: None,
                    output: Some("内容".into()),
                    status: "completed".into(),
                    created_at: now(),
                },
            )
            .unwrap();
        downgrade_to_v17(&store);
        let rejected = store.connection.execute(
            "UPDATE runs SET agent_id='claude-win' WHERE id=?1",
            [&run.id],
        );
        assert!(rejected.is_err(), "v17 约束应拒绝 Claude");
        let before = store.list("", false).unwrap().len();
        drop(store);

        let mut store = Store::open(&path).unwrap();
        let count = |sql: &str| -> i64 {
            store
                .connection
                .query_row(sql, [], |row| row.get(0))
                .unwrap()
        };
        assert_eq!(count("PRAGMA user_version"), SCHEMA_VERSION);
        assert_eq!(count("SELECT COUNT(*) FROM runs"), 1);
        assert_eq!(count("SELECT COUNT(*) FROM run_tool_events"), 1);
        assert_eq!(
            count("SELECT COUNT(*) FROM sqlite_schema WHERE sql LIKE '%''dsh-win''%' AND sql NOT LIKE '%''claude-win''%'"),
            0
        );
        let rooms = store.list("", false).unwrap();
        assert_eq!(rooms.len(), before + 1);
        let claude = rooms
            .iter()
            .find(|room| room.members == ["claude-win"])
            .expect("升级后应有与 Claude 的私聊");
        // 约束已放宽：Claude 的运行记录可以写入。
        store
            .begin_agent_run(
                &claude.id,
                &Uuid::new_v4().to_string(),
                "你好",
                "claude-win",
            )
            .unwrap();
        let five = [
            "codex-win",
            "hermes-win",
            "dsh-win",
            "claude-win",
            "albion-wsl",
        ]
        .map(String::from);
        store.create("一家五口", "group", &five).unwrap();
        assert!(std::fs::read_dir(root.join("backups"))
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("hub-schema-v17-")));
        drop(store);

        // 再次打开不会重复补私聊。
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store
                .list("", false)
                .unwrap()
                .iter()
                .filter(|room| room.members == ["claude-win"])
                .count(),
            1
        );
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn native_run_promotion_is_idempotent_and_rejects_payload_drift() {
        let mut store = memory();
        let room = direct(&mut store, "真实私聊");
        let id = Uuid::new_v4().to_string();
        let mut run = store.begin_run(&room.id, &id, "hello").unwrap();
        assert_eq!(
            store.detail(&room.id).unwrap().messages[0].status,
            "pending"
        );
        assert_eq!(store.begin_run(&room.id, &id, "hello").unwrap().id, run.id);
        assert!(store.begin_run(&room.id, &id, "different").is_err());
        store.bind_thread(&mut run, "native-thread").unwrap();
        store.mark_turn(&mut run, "native-turn").unwrap();
        run.status = "completed".into();
        store.checkpoint(&run, "真实回复", "").unwrap();
        let detail = store.detail(&room.id).unwrap();
        assert_eq!(detail.messages.len(), 2);
        assert_eq!(detail.messages[0].status, "delivered");
        assert_eq!(detail.messages[1].status, "completed");
        assert_eq!(detail.messages[1].sender_id, "codex-win");
        assert_eq!(
            detail.sessions[0].native_session_id.as_deref(),
            Some("native-thread")
        );
    }

    #[test]
    fn native_delivery_rejects_group_and_other_agent() {
        let mut store = memory();
        let group = store
            .create("群聊", "group", &["codex-win".into(), "dsh-win".into()])
            .unwrap();
        let other = store
            .create("阿尔比恩", "direct", &["albion-wsl".into()])
            .unwrap();
        for room in [group, other] {
            assert!(store
                .begin_run(&room.id, &Uuid::new_v4().to_string(), "不能投递")
                .is_err());
            assert!(store.detail(&room.id).unwrap().messages.is_empty());
        }
    }

    #[test]
    fn v4_upgrade_keeps_native_runs_and_separates_albion_identity() {
        let root = std::env::temp_dir().join(format!("hub-v4-{}", Uuid::new_v4()));
        let path = root.join("hub.db");
        let mut store = Store::open(&path).unwrap();
        let codex = direct(&mut store, "既有项目");
        let mut old = store
            .begin_run(&codex.id, &Uuid::new_v4().to_string(), "旧请求")
            .unwrap();
        store
            .bind_thread(&mut old, "retained-codex-thread")
            .unwrap();
        old.status = "completed".into();
        store.checkpoint(&old, "已保存回复", "").unwrap();
        store
            .connection
            .execute_batch("PRAGMA user_version=4;")
            .unwrap();
        drop(store);
        let mut store = Store::open(&path).unwrap();
        assert_eq!(
            store
                .run_for_message(&old.user_message_id)
                .unwrap()
                .unwrap()
                .native_thread_id
                .as_deref(),
            Some("retained-codex-thread")
        );
        let albion = store
            .create("独立阿尔比恩", "direct", &["albion-wsl".into()])
            .unwrap();
        store
            .set_session_settings(&albion.id, "albion-wsl", None, Some("low".into()))
            .unwrap();
        let mut run = store
            .begin_agent_run(
                &albion.id,
                &Uuid::new_v4().to_string(),
                "独立私聊",
                "albion-wsl",
            )
            .unwrap();
        assert_eq!(run.agent_id, "albion-wsl");
        assert_eq!(run.reasoning_effort.as_deref(), Some("low"));
        assert!(store
            .begin_agent_run(
                &albion.id,
                &Uuid::new_v4().to_string(),
                "重复",
                "albion-wsl"
            )
            .is_err());
        store.bind_thread(&mut run, "albion-session").unwrap();
        run.status = "completed".into();
        store.checkpoint(&run, "来自阿尔比恩", "").unwrap();
        assert_eq!(
            store.detail(&albion.id).unwrap().messages[1].sender_id,
            "albion-wsl"
        );
        assert_eq!(
            store.detail(&codex.id).unwrap().messages[1].content,
            "已保存回复"
        );
        assert_eq!(
            store
                .connection
                .query_row("PRAGMA foreign_key_check", [], |_| Ok(()))
                .optional()
                .unwrap(),
            None
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unfinished_run_recovers_without_replay_and_preserves_partial_text() {
        let root = std::env::temp_dir().join(format!("hub-recover-{}", Uuid::new_v4()));
        let path = root.join("hub.db");
        let mut store = Store::open(&path).unwrap();
        let room = direct(&mut store, "中断恢复");
        let mut run = store
            .begin_run(&room.id, &Uuid::new_v4().to_string(), "提问")
            .unwrap();
        store.bind_thread(&mut run, "thread-survives").unwrap();
        store.mark_turn(&mut run, "turn-interrupted").unwrap();
        store.checkpoint(&run, "部分输出", "").unwrap();
        drop(store);
        let mut store = Store::open(&path).unwrap();
        let detail = store.detail(&room.id).unwrap();
        assert_eq!(detail.messages[1].status, "interrupted");
        assert_eq!(detail.messages[1].content, "部分输出");
        assert_eq!(
            detail.sessions[0].native_session_id.as_deref(),
            Some("thread-survives")
        );
        assert_eq!(
            store
                .run_for_message(&run.user_message_id)
                .unwrap()
                .unwrap()
                .status,
            "interrupted"
        );
        assert!(store
            .begin_run(&room.id, &Uuid::new_v4().to_string(), "新消息")
            .is_ok());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn v1_migration_preserves_message_order_and_identity() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("CREATE TABLE conversations(id TEXT PRIMARY KEY,title TEXT,kind TEXT,archived INTEGER DEFAULT 0,created_at INTEGER,updated_at INTEGER);
          INSERT INTO conversations VALUES('legacy-room','旧会话','direct',0,1,1);
          CREATE TABLE messages(sequence INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT UNIQUE,conversation_id TEXT REFERENCES conversations(id),sender_id TEXT,content TEXT,status TEXT CHECK(status='local_only'),created_at INTEGER);
          INSERT INTO messages VALUES(7,'legacy-message','legacy-room','user','旧版本草稿','local_only',1);
          CREATE TABLE metadata(key TEXT PRIMARY KEY,value TEXT);
          INSERT INTO metadata VALUES('seeded','1'); PRAGMA user_version=1;").unwrap();
        let mut store = Store::initialize(connection, PathBuf::from(":memory:")).unwrap();
        let detail = store.detail("legacy-room").unwrap();
        assert_eq!(detail.messages[0].id, "legacy-message");
        assert_eq!(detail.messages[0].content, "旧版本草稿");
        assert_eq!(
            store
                .connection
                .query_row("SELECT sequence FROM messages", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            7
        );
        assert_eq!(
            store
                .connection
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            SCHEMA_VERSION
        );
        let room = direct(&mut store, "升级后");
        assert!(store
            .begin_run(&room.id, &Uuid::new_v4().to_string(), "升级后可发送")
            .is_ok());
    }

    #[test]
    fn native_failure_before_delivery_never_marks_user_as_delivered() {
        let mut store = memory();
        let room = direct(&mut store, "启动失败");
        let mut run = store
            .begin_run(&room.id, &Uuid::new_v4().to_string(), "提问")
            .unwrap();
        run.status = "failed".into();
        store.checkpoint(&run, "", "").unwrap();
        let detail = store.detail(&room.id).unwrap();
        assert_eq!(detail.messages[0].status, "failed");
        assert_eq!(detail.messages[1].status, "failed");
    }

    #[test]
    fn quota_stalled_runs_only_lists_untouched_quota_failures() {
        let mut store = memory();
        let room = direct(&mut store, "额度");
        let mut run = store
            .begin_run(&room.id, &Uuid::new_v4().to_string(), "做事")
            .unwrap();
        run.status = "failed".into();
        run.error =
            Some("Codex 额度限制（额度预计 10月14日 14:29 重置，resets_at=1792006140）".into());
        store.checkpoint(&run, "做了一半", "").unwrap();
        let stalled = store.quota_stalled_runs("codex-win").unwrap();
        assert_eq!(stalled.len(), 1);
        assert_eq!(stalled[0].0, room.id);
        assert!(stalled[0].1.contains("resets_at=1792006140"));
        assert!(store.quota_stalled_runs("claude-win").unwrap().is_empty());
        // 用户在失败后自己又说了话：交给用户，不再自动续。
        store
            .save_message(&room.id, &Uuid::new_v4().to_string(), "先别做了")
            .unwrap();
        assert!(store.quota_stalled_runs("codex-win").unwrap().is_empty());
    }

    #[test]
    fn seeds_five_private_and_one_technical_group() {
        let store = memory();
        let rooms = store.list("", false).unwrap();
        assert_eq!(rooms.len(), 6);
        assert_eq!(rooms.iter().filter(|room| room.kind == "direct").count(), 5);
        assert_eq!(
            rooms
                .iter()
                .find(|room| room.kind == "group")
                .unwrap()
                .members
                .len(),
            3
        );
    }
    #[test]
    fn persists_messages_and_stable_session_keys_across_reopen() {
        let root = std::env::temp_dir().join(format!("agent-hub-test-{}", Uuid::new_v4()));
        let path = root.join("hub.db");
        let mut store = Store::open(&path).unwrap();
        let room = direct(&mut store, "项目 A");
        message(&mut store, &room.id, "恢复后仍应存在");
        let key = store.detail(&room.id).unwrap().sessions[0]
            .session_key
            .clone();
        drop(store);
        let store = Store::open(&path).unwrap();
        let detail = store.detail(&room.id).unwrap();
        assert_eq!(detail.messages[0].content, "恢复后仍应存在");
        assert_eq!(detail.sessions[0].session_key, key);
        assert_eq!(store.list("", false).unwrap().len(), 7);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn private_and_group_messages_and_sessions_are_isolated() {
        let mut store = memory();
        let one = direct(&mut store, "私聊 A");
        let two = direct(&mut store, "私聊 B");
        let group = store
            .create("开发群", "group", &["codex-win".into(), "dsh-win".into()])
            .unwrap();
        message(&mut store, &one.id, "私有内容");
        assert!(store.detail(&two.id).unwrap().messages.is_empty());
        assert!(store.detail(&group.id).unwrap().messages.is_empty());
        let keys = [&one, &two, &group].map(|room| {
            store
                .detail(&room.id)
                .unwrap()
                .sessions
                .iter()
                .find(|session| session.agent_id == "codex-win")
                .unwrap()
                .session_key
                .clone()
        });
        assert_ne!(keys[0], keys[1]);
        assert_ne!(keys[1], keys[2]);
    }

    #[test]
    fn native_tool_events_are_persisted_with_their_assistant_turn() {
        let mut store = memory();
        let room = direct(&mut store, "工具轨迹");
        let run = store
            .begin_agent_run(
                &room.id,
                &Uuid::new_v4().to_string(),
                "读取并检查文件",
                "codex-win",
            )
            .unwrap();
        let event = RunToolEvent {
            event_id: "item-1".into(),
            kind: "commandExecution".into(),
            title: "cargo test".into(),
            input: Some("cargo test".into()),
            output: Some("12 tests passed".into()),
            status: "completed".into(),
            created_at: now(),
        };
        store.append_tool_event(&run, &event).unwrap();
        let detail = store.detail(&room.id).unwrap();
        let assistant = detail
            .messages
            .iter()
            .find(|message| message.id == run.assistant_message_id)
            .unwrap();
        assert_eq!(assistant.tool_events, vec![event]);
    }
    #[test]
    fn duplicate_requests_are_idempotent_but_payload_drift_is_rejected() {
        let mut store = memory();
        let room = direct(&mut store, "测试");
        let id = Uuid::new_v4().to_string();
        let first = store.save_message(&room.id, &id, "内容").unwrap();
        assert_eq!(store.save_message(&room.id, &id, "内容").unwrap(), first);
        assert!(store.save_message(&room.id, &id, "其他内容").is_err());
        assert_eq!(store.detail(&room.id).unwrap().messages.len(), 1);
    }
    #[test]
    fn rename_does_not_reset_history_or_session() {
        let mut store = memory();
        let room = direct(&mut store, "旧名称");
        message(&mut store, &room.id, "证据");
        let key = store.detail(&room.id).unwrap().sessions[0]
            .session_key
            .clone();
        assert_eq!(store.rename(&room.id, "新名称").unwrap().title, "新名称");
        let detail = store.detail(&room.id).unwrap();
        assert_eq!(detail.messages.len(), 1);
        assert_eq!(detail.sessions[0].session_key, key);
    }
    #[test]
    fn archive_prevents_writes_and_restore_preserves_history() {
        let mut store = memory();
        let room = direct(&mut store, "归档");
        message(&mut store, &room.id, "保留");
        store.archive(&room.id, true).unwrap();
        assert_eq!(store.list("归档", true).unwrap().len(), 1);
        assert!(store.list("归档", false).unwrap().is_empty());
        assert!(store
            .save_message(&room.id, &Uuid::new_v4().to_string(), "错误写入")
            .is_err());
        store.archive(&room.id, false).unwrap();
        assert_eq!(store.detail(&room.id).unwrap().messages.len(), 1);
    }
    #[test]
    fn deleting_conversation_cascades_only_its_records_and_does_not_reseed() {
        let mut store = memory();
        let room = direct(&mut store, "删除");
        let other = direct(&mut store, "保留");
        message(&mut store, &room.id, "删除内容");
        message(&mut store, &other.id, "保留内容");
        store.delete(&room.id).unwrap();
        assert!(store.detail(&room.id).is_err());
        assert_eq!(store.detail(&other.id).unwrap().messages.len(), 1);
        let count: i64 = store
            .connection
            .query_row(
                "SELECT count(*) FROM sessions WHERE conversation_id=?1",
                [room.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }
    #[test]
    fn member_validation_rejects_unknown_duplicate_and_wrong_counts() {
        let mut store = memory();
        assert!(store.create("bad", "direct", &[]).is_err());
        assert!(store.create("bad", "group", &[]).is_err());
        // 一位成员的群聊可以：同一成员在项目里多开的会话也算成员。
        assert!(store.create("solo", "group", &["claude-win".into()]).is_ok());
        assert!(store
            .create("bad", "group", &["codex-win".into(), "codex-win".into()])
            .is_err());
        assert!(store.create("bad", "direct", &["unknown".into()]).is_err());
    }
    #[test]
    fn group_member_change_preserves_retained_session_and_history() {
        let mut store = memory();
        let group = store
            .create("群", "group", &["codex-win".into(), "dsh-win".into()])
            .unwrap();
        let before = store
            .detail(&group.id)
            .unwrap()
            .sessions
            .into_iter()
            .find(|s| s.agent_id == "codex-win")
            .unwrap()
            .session_key;
        message(&mut store, &group.id, "历史");
        store
            .replace_members(&group.id, &["codex-win".into(), "albion-wsl".into()])
            .unwrap();
        let after = store.detail(&group.id).unwrap();
        assert_eq!(after.messages.len(), 1);
        assert_eq!(after.sessions.len(), 2);
        assert_eq!(
            after
                .sessions
                .iter()
                .find(|s| s.agent_id == "codex-win")
                .unwrap()
                .session_key,
            before
        );
        assert!(!after.sessions.iter().any(|s| s.agent_id == "dsh-win"));
    }
    #[test]
    fn searches_literal_text_in_titles_and_messages() {
        let mut store = memory();
        let room = direct(&mut store, "搜索目标");
        message(&mut store, &room.id, "100%_唯一关键词");
        assert_eq!(store.list("唯一关键词", false).unwrap()[0].id, room.id);
        assert_eq!(store.list("%_", false).unwrap().len(), 1);
        assert!(store.list("' OR 1=1 --", false).unwrap().is_empty());
    }
    #[test]
    fn invalid_text_and_missing_conversation_are_rejected_without_writes() {
        let mut store = memory();
        let room = direct(&mut store, "输入边界");
        assert!(store.rename(&room.id, " ").is_err());
        assert!(store.rename(&room.id, "a\nb").is_err());
        assert!(store.save_message(&room.id, "invalid", "内容").is_err());
        assert!(store
            .save_message(&room.id, &Uuid::new_v4().to_string(), &"字".repeat(16001))
            .is_err());
        assert!(store
            .save_message("missing", &Uuid::new_v4().to_string(), "内容")
            .is_err());
        assert!(store.detail(&room.id).unwrap().messages.is_empty());
    }
    #[test]
    fn stored_messages_cannot_claim_native_delivery_or_agent_identity() {
        let mut store = memory();
        let room = direct(&mut store, "诚实状态");
        let saved = message(&mut store, &room.id, "hello");
        assert_eq!(saved.sender_id, "user");
        assert_eq!(saved.status, "local_only");
        assert!(store.detail(&room.id).unwrap().sessions[0]
            .native_session_id
            .is_none());
    }
}

#[cfg(test)]
mod thought_mark_tests {
    use super::{push_break, push_steer, push_thought, push_tool};

    #[test]
    fn marks_where_each_thought_segment_starts_in_the_reply() {
        let mut thought = String::new();
        push_thought(&mut thought, "", "先想。");
        push_thought(&mut thought, "", "再想。");
        push_thought(&mut thought, "答😀", "接着想。");
        push_thought(&mut thought, "答😀", "还在想。");
        assert_eq!(thought, "先想。再想。\u{1e}3\u{1f}接着想。还在想。");
    }

    #[test]
    fn marks_bubble_breaks_once_and_steers_with_their_message() {
        let mut thought = String::new();
        push_break(&mut thought, "");
        push_break(&mut thought, "第一段");
        push_break(&mut thought, "第一段");
        push_steer(&mut thought, "第一段", "m-1");
        assert_eq!(thought, "\u{1d}3\u{1f}\u{1c}3:m-1\u{1f}");
        let mut thought = String::new();
        push_tool(&mut thought, "说", "call-1");
        push_tool(&mut thought, "说完了", "call-1");
        assert_eq!(thought, "\u{1b}1:call-1\u{1f}");
    }
}
