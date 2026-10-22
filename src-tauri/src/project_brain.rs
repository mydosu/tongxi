//! 主脑：每个项目一条常驻的主脑会话（`claude-win-brain` 这类席位），用户直接和它说话；
//! 它按能力把活派给成员会话、看交回的结果、自己合并收口。
//! 派活、插话、停止先记进 brain_jobs（主脑的工具进程只碰数据库），由主程序里的投递线程真正发给成员；
//! 成员交回的结果和用户在问答面板的回答记进 brain_inbox，再作为新消息送进主脑会话（主脑正在回复时用插话）。
use crate::codex::RuntimeSnapshot;
use crate::project_store::{base_agent, is_brain, RoleChoice, BRAIN_SLOT, EXECUTOR_AGENTS};
use crate::store::{now, RunRecord, Store};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::Emitter;
use uuid::Uuid;

type Result<T> = std::result::Result<T, String>;

/// 没指定主脑成员时按这个顺序挑。
const BRAIN_PREFERENCE: [&str; 4] = ["claude-win", "codex-win", "hermes-win", "dsh-win"];
const REPORT_LIMIT: usize = 6000;
/// 派活页里每件活的结果最多带这么多字，全文去成员会话看。
const RESULT_LIMIT: usize = 8000;

pub fn migrate(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS brain_jobs (
           id TEXT PRIMARY KEY,room TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
           seat TEXT NOT NULL,chat TEXT NOT NULL,kind TEXT NOT NULL,content TEXT NOT NULL,
           model TEXT,effort TEXT,worktree TEXT,branch TEXT,
           status TEXT NOT NULL,run_id TEXT,error TEXT,created_at INTEGER NOT NULL,finished_at INTEGER);
         CREATE INDEX IF NOT EXISTS brain_job_status ON brain_jobs(status);
         CREATE TABLE IF NOT EXISTS brain_inbox (
           id TEXT PRIMARY KEY,room TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
           content TEXT NOT NULL,delivered INTEGER NOT NULL DEFAULT 0,created_at INTEGER NOT NULL);",
    )
    .map_err(|_| "主脑结构迁移失败".into())
}

impl Store {
    pub fn project_coordinator(&self, room: &str) -> Result<Option<RoleChoice>> {
        let value: Option<String> = self
            .connection
            .query_row(
                "SELECT settings FROM project_coordinator_settings WHERE conversation_id=?1",
                [room],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| "主脑设置读取失败")?;
        value
            .map(|value| serde_json::from_str(&value).map_err(|_| "主脑设置无效".into()))
            .transpose()
    }
    pub fn set_project_coordinator(&mut self, room: &str, choice: &RoleChoice) -> Result<()> {
        let conversation = self.conversation(room)?;
        if conversation.kind != "group"
            || conversation.archived
            || self.conversation_project(room)?.is_none()
        {
            return Err("请先绑定未归档项目群".into());
        }
        if !crate::project_store::EXECUTOR_AGENTS.contains(&choice.agent.as_str())
            || !conversation.members.contains(&choice.agent)
        {
            return Err("主脑应从本项目的 Codex、Hermes、DSH 或 Claude 成员中选择".into());
        }
        self.connection.execute("INSERT INTO project_coordinator_settings(conversation_id,settings) VALUES(?1,?2) ON CONFLICT(conversation_id) DO UPDATE SET settings=excluded.settings",params![room,serde_json::to_string(choice).map_err(|_|"主脑设置无效")?]).map_err(|_|"主脑设置保存失败")?;
        Ok(())
    }
    /// 用户在项目设置里写的长期授权（比如「允许提交并推送 main」）：每次协作都算用户原话授权，没写就是空。
    pub fn project_grants(&self, room: &str) -> String {
        self.connection
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [format!("project_grants:{room}")],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
            .unwrap_or_default()
    }
    pub fn set_project_grants(&mut self, room: &str, grants: &str) -> Result<()> {
        let grants = grants.trim();
        if grants.chars().count() > 500 {
            return Err("长期授权不超过 500 字".into());
        }
        if self.conversation_project(room)?.is_none() {
            return Err("请先绑定项目".into());
        }
        self.connection
            .execute(
                "INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![format!("project_grants:{room}"), grants],
            )
            .map_err(|_| "长期授权保存失败")?;
        Ok(())
    }
}

impl Store {
    /// 主脑派给这位成员（任一席位）、还没交回的活。
    pub fn brain_busy(&self, agent: &str) -> bool {
        self.connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM brain_jobs WHERE kind='task' AND status IN ('pending','running') AND (seat=?1 OR seat LIKE ?1 || '-%'))",
                [agent],
                |row| row.get::<_, bool>(0),
            )
            .unwrap_or(true)
    }
    /// 主脑在这个群里还有派出去没交回的活时，不能归档、删除或改成员。
    pub fn guard_brain_jobs(&self, room: &str) -> Result<()> {
        let busy: bool = self
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM brain_jobs WHERE room=?1 AND status IN ('pending','running'))",
                [room],
                |row| row.get(0),
            )
            .map_err(|_| "主脑派活状态读取失败")?;
        if busy {
            return Err("主脑派的活还没交回，先停掉或等成员交回".into());
        }
        Ok(())
    }
    /// 开了「公开动态」的项目群里最近的进展摘要：(项目名, 内容)，最新的在前。
    pub fn shared_team_updates(&self, limit: usize) -> Result<Vec<(String, String)>> {
        let mut query = self
            .connection
            .prepare("SELECT p.name,t.content FROM project_team_messages t JOIN projects p ON p.id=t.project_id WHERE p.summary_enabled=1 ORDER BY t.created_at DESC,t.id DESC LIMIT ?1")
            .map_err(|_| "项目动态读取失败")?;
        let rows = query
            .query_map([limit as i64], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(|_| "项目动态读取失败")?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(|_| "项目动态读取失败".into())
    }
    /// 派活页：主脑派出去的每件活（最新的在前）和主脑还没收到的消息数。
    pub fn brain_overview(&self, room: &str) -> Result<BrainOverview> {
        let seat = brain_seat(self, room)?;
        let chat: Option<String> = self
            .connection
            .query_row(
                "SELECT conversation_id FROM project_agent_chats WHERE group_conversation_id=?1 AND agent_id=?2",
                params![room, seat],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| "主脑会话读取失败")?;
        let waiting: i64 = self
            .connection
            .query_row("SELECT count(*) FROM brain_inbox WHERE room=?1 AND delivered=0", [room], |row| row.get(0))
            .map_err(|_| "主脑收件读取失败")?;
        let mut query = self
            .connection
            .prepare(
                "SELECT j.id,j.seat,j.chat,j.content,COALESCE(j.model,(SELECT model FROM sessions WHERE conversation_id=j.chat LIMIT 1)),
                        COALESCE(j.effort,(SELECT reasoning_effort FROM sessions WHERE conversation_id=j.chat LIMIT 1)),
                        j.worktree,j.branch,j.status,j.error,r.status,m.content,j.created_at,j.finished_at
                 FROM brain_jobs j LEFT JOIN runs r ON r.id=j.run_id LEFT JOIN messages m ON m.id=r.assistant_message_id
                 WHERE j.room=?1 AND j.kind='task' ORDER BY j.created_at DESC",
            )
            .map_err(|_| "派活记录读取失败")?;
        let jobs = query
            .query_map([room], |row| {
                let seat: String = row.get(1)?;
                let status: String = row.get(8)?;
                // 还在做的活结果不完整，做完了才带上。
                let result: Option<String> = if matches!(status.as_str(), "done" | "failed") { row.get(11)? } else { None };
                Ok(BrainJob {
                    id: row.get(0)?,
                    name: label(&self.connection, room, &seat),
                    seat,
                    chat: row.get(2)?,
                    content: row.get(3)?,
                    model: row.get(4)?,
                    effort: row.get(5)?,
                    worktree: row.get(6)?,
                    branch: row.get(7)?,
                    status,
                    error: row.get(9)?,
                    run_status: row.get(10)?,
                    result: result.map(|text| tail(text.trim(), RESULT_LIMIT)),
                    created_at: row.get(12)?,
                    finished_at: row.get(13)?,
                })
            })
            .map_err(|_| "派活记录读取失败")?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| "派活记录读取失败")?;
        Ok(BrainOverview { seat, chat, waiting, jobs })
    }
    /// 用户在派活页停掉一件活：还没开始的直接取消，正在做的叫停成员；都告诉主脑一声。
    pub fn stop_brain_job(&self, room: &str, job: &str) -> Result<()> {
        let (id, seat, status, _, _) = job_status(&self.connection, room, job)?;
        let name = label(&self.connection, room, &seat);
        let cancelled = status == "pending"
            && self
                .connection
                .execute(
                    "UPDATE brain_jobs SET status='failed',error='你在派活页取消了',finished_at=?1 WHERE id=?2 AND status='pending'",
                    params![now(), id],
                )
                .map_err(|_| "任务状态保存失败")?
                > 0;
        let note = if cancelled {
            format!("【同席】用户在派活页取消了给 {name} 的任务 {}（还没开始）", short(&id))
        } else {
            let chat: String = self
                .connection
                .query_row("SELECT chat FROM brain_jobs WHERE id=?1 AND status='running'", [&id], |row| row.get(0))
                .optional()
                .map_err(|_| "任务读取失败")?
                .ok_or("这件活已经结束了")?;
            self.connection
                .execute(
                    "INSERT INTO brain_jobs(id,room,seat,chat,kind,content,status,created_at) VALUES(?1,?2,?3,?4,'stop','','pending',?5)",
                    params![Uuid::new_v4().to_string(), room, seat, chat, now()],
                )
                .map_err(|_| "停止请求保存失败")?;
            format!("【同席】用户在派活页停掉了 {name} 正在做的任务 {}", short(&id))
        };
        self.connection
            .execute(
                "INSERT INTO brain_inbox(id,room,content,created_at) VALUES(?1,?2,?3,?4)",
                params![Uuid::new_v4().to_string(), room, note, now()],
            )
            .map_err(|_| "主脑消息保存失败")?;
        Ok(())
    }
}

#[derive(serde::Serialize)]
pub struct BrainJob {
    id: String,
    seat: String,
    name: String,
    chat: String,
    content: String,
    model: Option<String>,
    effort: Option<String>,
    /// 工作树还在时的路径；合并或丢弃后为空，分支名留着。
    worktree: Option<String>,
    branch: Option<String>,
    /// pending 排队、running 在做、done 做完、failed 失败或被停下（看 run_status）。
    status: String,
    error: Option<String>,
    run_status: Option<String>,
    result: Option<String>,
    created_at: i64,
    finished_at: Option<i64>,
}

#[derive(serde::Serialize)]
pub struct BrainOverview {
    seat: String,
    chat: Option<String>,
    /// 交回的结果、问答回答里还没送进主脑会话的条数。
    waiting: i64,
    jobs: Vec<BrainJob>,
}

/// 本项目的主脑跑在哪个成员上：项目设置里选的，没选或已不在群里就按默认顺序挑。
pub fn brain_agent(store: &Store, room: &str) -> Result<String> {
    let members = store.conversation(room)?.members;
    if let Some(choice) = store.project_coordinator(room)? {
        if EXECUTOR_AGENTS.contains(&choice.agent.as_str()) && members.contains(&choice.agent) {
            return Ok(choice.agent);
        }
    }
    BRAIN_PREFERENCE
        .into_iter()
        .find(|agent| members.iter().any(|member| member == agent))
        .map(String::from)
        .ok_or_else(|| "项目里还没有能当主脑的成员".into())
}

pub fn brain_seat(store: &Store, room: &str) -> Result<String> {
    Ok(format!("{}-{BRAIN_SLOT}", brain_agent(store, room)?))
}

/// 项目设置里的主脑模型、思考强度改了，就同步到主脑会话；没改过就不动（用户可能在主脑会话里自己换过）。
pub fn sync_brain_settings(store: &mut Store, room: &str) -> Result<()> {
    let Some(choice) = store.project_coordinator(room)? else {
        return Ok(());
    };
    let seat = brain_seat(store, room)?;
    if base_agent(&seat) != choice.agent {
        return Ok(());
    }
    let wanted = serde_json::to_string(&choice).map_err(|_| "主脑设置无效")?;
    let key = format!("brain_synced:{room}");
    let synced: Option<String> = store
        .connection
        .query_row("SELECT value FROM metadata WHERE key=?1", [&key], |row| row.get(0))
        .optional()
        .map_err(|_| "主脑设置读取失败")?;
    if synced.as_deref() == Some(wanted.as_str()) {
        return Ok(());
    }
    let chat: Option<String> = store
        .connection
        .query_row(
            "SELECT conversation_id FROM project_agent_chats WHERE group_conversation_id=?1 AND agent_id=?2",
            params![room, seat],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| "主脑会话读取失败")?;
    let Some(chat) = chat else {
        return Ok(());
    };
    store
        .connection
        .execute(
            "UPDATE sessions SET model=?1,reasoning_effort=?2 WHERE conversation_id=?3 AND agent_id=?4",
            params![choice.model, choice.effort, chat, choice.agent],
        )
        .map_err(|_| "主脑设置保存失败")?;
    store
        .connection
        .execute(
            "INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=?2",
            params![key, wanted],
        )
        .map_err(|_| "主脑设置保存失败")?;
    Ok(())
}

// ---------- 工具进程这一侧：只有数据库连接 ----------

struct Context {
    project: String,
    room: String,
    seat: String,
    root: PathBuf,
    db: PathBuf,
}

pub fn is_brain_chat(connection: &Connection, conversation_id: &str) -> bool {
    connection
        .query_row(
            "SELECT agent_id FROM project_agent_chats WHERE conversation_id=?1",
            [conversation_id],
            |row| row.get::<_, String>(0),
        )
        .is_ok_and(|seat| is_brain(&seat))
}

fn context(connection: &Connection, conversation_id: &str) -> Result<Context> {
    let (project, room, seat): (String, String, String) = connection
        .query_row(
            "SELECT project_id,group_conversation_id,agent_id FROM project_agent_chats WHERE conversation_id=?1",
            [conversation_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(|_| "主脑会话归属读取失败")?
        .ok_or("这不是主脑会话")?;
    if !is_brain(&seat) {
        return Err("这不是主脑会话".into());
    }
    let root: String = connection
        .query_row("SELECT root FROM projects WHERE id=?1", [&project], |row| row.get(0))
        .map_err(|_| "项目目录读取失败")?;
    let db: String = connection
        .query_row("SELECT file FROM pragma_database_list WHERE name='main'", [], |row| row.get(0))
        .map_err(|_| "数据库路径不可用")?;
    Ok(Context { project, room, seat, root: root.into(), db: db.into() })
}

fn metadata(connection: &Connection, key: &str) -> Option<String> {
    connection
        .query_row("SELECT value FROM metadata WHERE key=?1", [key], |row| row.get(0))
        .optional()
        .ok()
        .flatten()
}

fn label(connection: &Connection, room: &str, seat: &str) -> String {
    let base = base_agent(seat);
    let custom: Option<String> = if base == seat {
        connection
            .query_row("SELECT label FROM project_member_names WHERE conversation_id=?1 AND agent_id=?2", params![room, seat], |row| row.get(0))
            .optional()
            .ok()
            .flatten()
    } else {
        connection
            .query_row("SELECT label FROM project_seats WHERE conversation_id=?1 AND seat_id=?2", params![room, seat], |row| row.get(0))
            .optional()
            .ok()
            .flatten()
    };
    if let Some(label) = custom.filter(|label| !label.trim().is_empty()) {
        return label;
    }
    let name = crate::store::agents()
        .into_iter()
        .find(|agent| agent.id == base)
        .map_or_else(|| base.to_owned(), |agent| agent.name);
    if base == seat {
        name
    } else {
        format!("{name} {}号", seat.rsplit('-').next().unwrap_or(""))
    }
}

/// 主脑能派活的成员会话：群里还在的成员，各自的主会话和多开的席位。
fn team(connection: &Connection, ctx: &Context) -> Result<Vec<(String, String)>> {
    let members: Vec<String> = connection
        .prepare("SELECT agent_id FROM members WHERE conversation_id=?1")
        .and_then(|mut query| query.query_map([&ctx.room], |row| row.get(0))?.collect())
        .map_err(|_| "项目成员读取失败")?;
    let chats: Vec<(String, String)> = connection
        .prepare("SELECT agent_id,conversation_id FROM project_agent_chats WHERE group_conversation_id=?1 ORDER BY agent_id")
        .and_then(|mut query| query.query_map([&ctx.room], |row| Ok((row.get(0)?, row.get(1)?)))?.collect())
        .map_err(|_| "成员会话读取失败")?;
    Ok(chats
        .into_iter()
        .filter(|(seat, _)| !is_brain(seat) && members.iter().any(|member| member == base_agent(seat)))
        .collect())
}

fn find_member(connection: &Connection, ctx: &Context, member: &str) -> Result<(String, String)> {
    let member = member.trim();
    team(connection, ctx)?
        .into_iter()
        .find(|(seat, _)| seat == member || label(connection, &ctx.room, seat) == member)
        .ok_or_else(|| format!("没有这个成员：{member}（用 hub_team 看成员的 seat）"))
}

/// 成员手上有没有没交回的活，或者用户正在它的会话里聊。
fn busy(connection: &Connection, seat: &str, chat: &str) -> Option<String> {
    let job: Option<String> = connection
        .query_row(
            "SELECT id FROM brain_jobs WHERE seat=?1 AND chat=?2 AND kind='task' AND status IN ('pending','running') ORDER BY created_at DESC LIMIT 1",
            params![seat, chat],
            |row| row.get(0),
        )
        .optional()
        .ok()
        .flatten();
    if job.is_some() {
        return job;
    }
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM runs WHERE conversation_id=?1 AND status IN ('starting','running','cancelling'))",
            [chat],
            |row| row.get::<_, bool>(0),
        )
        .ok()
        .filter(|live| *live)
        .map(|_| "（成员正在回复别的消息）".into())
}

fn short(id: &str) -> &str {
    &id[..8.min(id.len())]
}

fn job_status(connection: &Connection, room: &str, job: &str) -> Result<(String, String, String, Option<String>, Option<String>)> {
    connection
        .query_row(
            "SELECT id,seat,status,worktree,branch FROM brain_jobs WHERE room=?1 AND kind='task' AND (id=?2 OR substr(id,1,8)=?2)",
            params![room, job.trim()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .optional()
        .map_err(|_| "任务读取失败")?
        .ok_or_else(|| format!("没有这个任务：{job}"))
}

fn insert_job(connection: &Connection, ctx: &Context, seat: &str, chat: &str, kind: &str, content: &str, extra: (Option<String>, Option<String>, Option<String>, Option<String>)) -> Result<String> {
    let id = Uuid::new_v4().to_string();
    connection
        .execute(
            "INSERT INTO brain_jobs(id,room,seat,chat,kind,content,model,effort,worktree,branch,status,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'pending',?11)",
            params![id, ctx.room, seat, chat, kind, content, extra.0, extra.1, extra.2, extra.3, now()],
        )
        .map_err(|_| "任务保存失败")?;
    Ok(id)
}

fn text_arg(args: &Value, name: &str, max: usize) -> Result<String> {
    let text = args[name].as_str().unwrap_or("").trim().to_owned();
    if text.is_empty() || text.chars().count() > max {
        return Err(format!("{name} 需为 1–{max} 个字"));
    }
    Ok(text)
}

fn optional_arg(args: &Value, name: &str) -> Option<String> {
    args[name].as_str().map(str::trim).filter(|text| !text.is_empty()).map(String::from)
}

pub fn tool_specs() -> Vec<Value> {
    let string = json!({"type":"string"});
    let definitions = [
        ("hub_team", "List the members you can delegate to (seat id, name, base agent, busy or idle, current model), the models each base agent can use with their effort levels, and current subscription quota. Check this before delegating.", json!({}), vec![]),
        ("hub_delegate", "Hand a task to a member. Returns immediately with a job id; the member works in its own session and its result arrives later as a new message to you. Write the task concretely: goal, where to change, what counts as done, what to hand back. model/effort are optional (default: the member session's current setting). worktree=true gives the member its own git worktree and branch from the current HEAD; use it for code changes, not for read-only, test or deploy work.", json!({"member":string,"task":string,"model":string,"effort":string,"worktree":{"type":"boolean"}}), vec!["member","task"]),
        ("hub_member", "Look at one member: whether it is busy, its recent jobs and the tail of its last reply.", json!({"member":string}), vec!["member"]),
        ("hub_tell_member", "Interject into a member's reply that is in progress (to correct course or add information). Only works while the member is replying; to continue with an idle member, use hub_delegate again (same session, it remembers what it did).", json!({"member":string,"content":string}), vec!["member","content"]),
        ("hub_stop_member", "Stop a member's reply in progress.", json!({"member":string}), vec!["member"]),
        ("hub_finish_worktree", "Close out a finished job that had a worktree: action=merge merges its branch into the project's current branch and removes the worktree; action=discard throws the work away. On merge conflicts the merge is left in the project directory for you to resolve.", json!({"job":string,"action":{"type":"string","enum":["merge","discard"]}}), vec!["job","action"]),
        ("hub_ask_user", "Put a question on the user's Q&A panel (does not wait). key identifies the matter (same key = same question). For reversible matters, give default and go ahead with it meanwhile; for irreversible ones set reversible=false and work on something else. The answer arrives later as a new message.", json!({"key":string,"question":string,"background":string,"options":{"type":"array","items":string},"default":string,"reversible":{"type":"boolean"}}), vec!["question"]),
        ("hub_broadcast", "Post one short progress line to the project group the user watches (started what, delegated to whom, finished what, needs the user to decide what). Do not flood it.", json!({"content":string}), vec!["content"]),
    ];
    let mut specs: Vec<Value> = definitions
        .into_iter()
        .map(|(name, description, properties, required)| json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false}}))
        .collect();
    specs.extend(
        crate::project_tools::tool_specs()
            .into_iter()
            .filter(|tool| tool["name"].as_str().is_some_and(|name| name.starts_with("hub_memory_"))),
    );
    specs
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FinishArg {
    job: String,
    action: String,
}

pub fn call(connection: &Connection, conversation_id: &str, name: &str, args: Value) -> Result<Value> {
    let ctx = context(connection, conversation_id)?;
    let args = if args.is_null() { json!({}) } else { args };
    match name {
        "hub_team" => {
            let members: Vec<Value> = team(connection, &ctx)?
                .into_iter()
                .map(|(seat, chat)| {
                    let (model, effort): (Option<String>, Option<String>) = connection
                        .query_row("SELECT model,reasoning_effort FROM sessions WHERE conversation_id=?1 AND agent_id=?2", params![chat, base_agent(&seat)], |row| Ok((row.get(0)?, row.get(1)?)))
                        .unwrap_or((None, None));
                    json!({"seat":seat,"name":label(connection,&ctx.room,&seat),"agent":base_agent(&seat),"busy":busy(connection,&seat,&chat),"model":model,"effort":effort})
                })
                .collect();
            let parse = |key: &str| metadata(connection, key).and_then(|text| serde_json::from_str::<Value>(&text).ok()).unwrap_or(Value::Null);
            Ok(json!({"members":members,"models":parse("brain_models"),"quota":parse("brain_quota")}))
        }
        "hub_delegate" => {
            let (seat, chat) = find_member(connection, &ctx, args["member"].as_str().unwrap_or(""))?;
            if let Some(job) = busy(connection, &seat, &chat) {
                return Err(format!("{seat} 手上还有活（{}），等它交回，或用 hub_tell_member / hub_stop_member", short(&job)));
            }
            let task = text_arg(&args, "task", 20000)?;
            let model = optional_arg(&args, "model");
            let effort = optional_arg(&args, "effort");
            if let Some(model) = &model {
                let catalog = metadata(connection, "brain_models").and_then(|text| serde_json::from_str::<Value>(&text).ok());
                if let Some(models) = catalog.as_ref().and_then(|catalog| catalog[base_agent(&seat)]["models"].as_array()) {
                    if !models.is_empty() && !models.iter().any(|item| item["id"] == model.as_str()) {
                        let ids: Vec<&str> = models.iter().filter_map(|item| item["id"].as_str()).collect();
                        return Err(format!("{seat} 没有模型 {model}，可选：{}", ids.join("、")));
                    }
                }
            }
            let (worktree, branch) = if args["worktree"].as_bool() == Some(true) {
                let tag = Uuid::new_v4().to_string();
                let name = format!("{seat}-{}", short(&tag));
                let branch = format!("tongxi/{name}");
                let root = ctx.db.parent().ok_or("数据目录不可用")?.join("worktrees");
                let path = crate::project_worktree::add(&ctx.root, &root, &name, &branch)?;
                (Some(path.to_string_lossy().to_string()), Some(branch))
            } else {
                (None, None)
            };
            let job = insert_job(connection, &ctx, &seat, &chat, "task", &task, (model, effort, worktree.clone(), branch.clone()))?;
            Ok(json!({"job":short(&job),"member":seat,"worktree":worktree,"branch":branch,"note":"已交给成员，它做完后结果会作为新消息发给你；不用等也不用查"}))
        }
        "hub_member" => {
            let (seat, chat) = find_member(connection, &ctx, args["member"].as_str().unwrap_or(""))?;
            let jobs: Vec<Value> = connection
                .prepare("SELECT id,status,content,worktree,branch,error,created_at FROM brain_jobs WHERE seat=?1 AND chat=?2 AND kind='task' ORDER BY created_at DESC LIMIT 5")
                .and_then(|mut query| {
                    query
                        .query_map(params![seat, chat], |row| {
                            let content: String = row.get(2)?;
                            Ok(json!({"job":short(&row.get::<_,String>(0)?),"status":row.get::<_,String>(1)?,"task":content.chars().take(200).collect::<String>(),"worktree":row.get::<_,Option<String>>(3)?,"branch":row.get::<_,Option<String>>(4)?,"error":row.get::<_,Option<String>>(5)?,"created_at":row.get::<_,i64>(6)?}))
                        })?
                        .collect()
                })
                .map_err(|_| "成员任务读取失败")?;
            let last: Option<String> = connection
                .query_row(
                    "SELECT m.content FROM runs r JOIN messages m ON m.id=r.assistant_message_id WHERE r.conversation_id=?1 ORDER BY r.rowid DESC LIMIT 1",
                    [&chat],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|_| "成员回复读取失败")?;
            Ok(json!({"seat":seat,"busy":busy(connection,&seat,&chat),"jobs":jobs,"last_reply_tail":last.map(|text| tail(&text, 1500))}))
        }
        "hub_tell_member" => {
            let (seat, chat) = find_member(connection, &ctx, args["member"].as_str().unwrap_or(""))?;
            let content = text_arg(&args, "content", 4000)?;
            insert_job(connection, &ctx, &seat, &chat, "steer", &content, (None, None, None, None))?;
            Ok(json!({"queued":true,"note":"一两秒内送进成员正在进行的回复；送不进会作为新消息告诉你"}))
        }
        "hub_stop_member" => {
            let (seat, chat) = find_member(connection, &ctx, args["member"].as_str().unwrap_or(""))?;
            insert_job(connection, &ctx, &seat, &chat, "stop", "", (None, None, None, None))?;
            Ok(json!({"queued":true}))
        }
        "hub_finish_worktree" => {
            let arg: FinishArg = serde_json::from_value(args).map_err(|_| "参数无效：要 job 和 action（merge / discard）")?;
            let (id, seat, status, worktree, branch) = job_status(connection, &ctx.room, &arg.job)?;
            if matches!(status.as_str(), "pending" | "running") {
                return Err(format!("{seat} 还在做这项任务，等它交回再收"));
            }
            let branch = branch.ok_or("这项任务没开工作树")?;
            let mut outcome = String::new();
            if arg.action == "merge" {
                if !crate::project_worktree::merged(&ctx.root, &branch) {
                    outcome = crate::project_worktree::merge(&ctx.root, &branch)?;
                }
            } else if arg.action != "discard" {
                return Err("action 只能是 merge 或 discard".into());
            }
            if let Some(path) = &worktree {
                crate::project_worktree::remove(&ctx.root, Path::new(path))?;
            }
            let flag = if arg.action == "merge" { "-d" } else { "-D" };
            let _ = crate::project_worktree::git_output(&ctx.root, &["branch", flag, &branch]);
            connection
                .execute("UPDATE brain_jobs SET worktree=NULL WHERE id=?1", [&id])
                .map_err(|_| "任务状态保存失败")?;
            Ok(json!({"done":true,"action":arg.action,"merge":outcome}))
        }
        "hub_ask_user" => {
            let question = text_arg(&args, "question", 400)?;
            let key = optional_arg(&args, "key").unwrap_or_else(|| question.chars().take(60).collect());
            let ask = crate::project_inquiries::Ask {
                key,
                question,
                background: optional_arg(&args, "background"),
                options: args["options"].as_array().map(|items| items.iter().filter_map(|item| item.as_str().map(String::from)).collect()).unwrap_or_default(),
                default: optional_arg(&args, "default"),
                reversible: args["reversible"].as_bool().unwrap_or(true),
            };
            crate::project_inquiries::check(std::slice::from_ref(&ask))?;
            let asked = crate::project_inquiries::ask_on(connection, &ctx.room, None, &ctx.seat, &ask)?;
            Ok(json!({"asked":asked,"note":if asked {"已挂到问答面板，用户答了会作为新消息发给你"} else {"这个 key 用户已经答过了，看之前的回答"}}))
        }
        "hub_broadcast" => {
            let content = text_arg(&args, "content", 4000)?;
            team_message(connection, &ctx.project, &ctx.room, Some(&ctx.seat), &content)?;
            Ok(json!({"sent":true}))
        }
        name if name.starts_with("hub_memory_") => crate::project_memory::call(&ctx.project, base_agent(&ctx.seat), name, args),
        _ => Err("主脑不支持这个工具".into()),
    }
}

fn team_message(connection: &Connection, project: &str, room: &str, sender: Option<&str>, content: &str) -> Result<()> {
    connection
        .execute(
            "INSERT INTO project_team_messages(id,project_id,conversation_id,sender_agent,message_kind,content,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![Uuid::new_v4().to_string(), project, room, sender, if sender.is_some() { "agent" } else { "system" }, content, now()],
        )
        .map_err(|_| "项目群消息保存失败")?;
    Ok(())
}

fn tail(text: &str, limit: usize) -> String {
    let count = text.chars().count();
    if count <= limit {
        return text.to_owned();
    }
    format!("……{}", text.chars().skip(count - limit).collect::<String>())
}

// ---------- 主程序这一侧：开场说明 ----------

/// 主脑会话每开一条新的原生会话，第一条消息前面带上主脑说明、模型档案和项目信息；之后的消息不再重复。
/// 不是主脑会话返回 None；是主脑会话但已经带过说明返回 Some(None)。
pub fn preamble(db: &Path, conversation_id: &str) -> Option<Option<String>> {
    let connection = Connection::open(db).ok()?;
    connection.busy_timeout(Duration::from_secs(5)).ok()?;
    let (project, room, seat): (String, String, String) = connection
        .query_row(
            "SELECT project_id,group_conversation_id,agent_id FROM project_agent_chats WHERE conversation_id=?1",
            [conversation_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .ok()?;
    if !is_brain(&seat) {
        return None;
    }
    let native: String = connection
        .query_row(
            "SELECT COALESCE(native_session_id,'') FROM sessions WHERE conversation_id=?1 AND agent_id=?2",
            params![conversation_id, base_agent(&seat)],
            |row| row.get(0),
        )
        .ok()?;
    let key = format!("brain_primed:{conversation_id}");
    if !native.is_empty() && metadata(&connection, &key).as_deref() == Some(native.as_str()) {
        return Some(None);
    }
    connection
        .execute("INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=?2", params![key, native])
        .ok()?;
    let (name, root): (String, String) = connection
        .query_row("SELECT name,root FROM projects WHERE id=?1", [&project], |row| Ok((row.get(0)?, row.get(1)?)))
        .ok()?;
    let grants = metadata(&connection, &format!("project_grants:{room}")).filter(|text| !text.trim().is_empty());
    Some(Some(format!(
        "{}\n\n## 模型档案\n{}\n\n## 本项目\n项目「{name}」，目录 {root}。计划文件是 .tongxi/计划.md，任务书放在 .tongxi/任务书/。\n项目长期授权：{}\n\n---\n以下是用户的消息：",
        crate::prompts::brain(),
        crate::prompts::models(),
        grants.as_deref().unwrap_or("（没有）"),
    )))
}

// ---------- 主程序这一侧：投递线程 ----------

pub struct Courier {
    app: tauri::AppHandle,
    store: Arc<Mutex<Store>>,
    codex: Arc<crate::codex::Runtime>,
    hermes: Arc<crate::hermes::Runtime>,
    dsh: Arc<crate::dsh::Runtime>,
    claude: Arc<crate::claude::Runtime>,
}

struct Pending {
    id: String,
    room: String,
    seat: String,
    chat: String,
    kind: String,
    content: String,
    model: Option<String>,
    effort: Option<String>,
    worktree: Option<String>,
    branch: Option<String>,
}

impl Courier {
    pub fn start(
        app: tauri::AppHandle,
        store: Arc<Mutex<Store>>,
        codex: Arc<crate::codex::Runtime>,
        hermes: Arc<crate::hermes::Runtime>,
        dsh: Arc<crate::dsh::Runtime>,
        claude: Arc<crate::claude::Runtime>,
    ) {
        let courier = Arc::new(Self { app, store, codex, hermes, dsh, claude });
        let watcher = courier.clone();
        std::thread::spawn(move || {
            let mut tick: u64 = 0;
            loop {
                if tick % 30 == 0 {
                    watcher.refresh_models();
                }
                let rooms = watcher.tick();
                for room in rooms {
                    let _ = watcher.app.emit("brain-state", room);
                }
                tick += 1;
                std::thread::sleep(Duration::from_secs(1));
            }
        });
        // 额度查一次可能要十几秒，单独慢慢刷。
        std::thread::spawn(move || loop {
            let quota = crate::quota::overview(&courier.codex);
            let _ = courier.store.lock().unwrap().connection.execute(
                "INSERT INTO metadata(key,value) VALUES('brain_quota',?1) ON CONFLICT(key) DO UPDATE SET value=?1",
                [quota.to_string()],
            );
            std::thread::sleep(Duration::from_secs(600));
        });
    }

    fn snapshot(&self, agent: &str) -> Option<RuntimeSnapshot> {
        Some(match agent {
            "codex-win" => self.codex.snapshot(),
            "claude-win" => self.claude.snapshot(),
            "hermes-win" => self.hermes.snapshot(),
            "dsh-win" => self.dsh.snapshot(),
            _ => return None,
        })
    }

    /// 没连上的先连；正在连就等下一轮。
    fn ready(&self, agent: &str) -> Result<bool> {
        let snapshot = self.snapshot(agent).ok_or("未知成员")?;
        match snapshot.connection.as_str() {
            "connected" => Ok(true),
            "connecting" => Ok(false),
            _ => {
                match agent {
                    "codex-win" => self.codex.connect(),
                    "claude-win" => self.claude.connect(),
                    "hermes-win" => self.hermes.connect(),
                    _ => self.dsh.connect(),
                }?;
                Ok(true)
            }
        }
    }

    fn live(&self, agent: &str, chat: &str) -> bool {
        self.snapshot(agent).is_some_and(|snapshot| {
            snapshot
                .runs
                .iter()
                .any(|run| run.record.conversation_id == chat && matches!(run.record.status.as_str(), "starting" | "running" | "cancelling"))
        })
    }

    fn send(&self, agent: &str, chat: &str, content: &str) -> Result<RunRecord> {
        let message = Uuid::new_v4().to_string();
        match agent {
            "codex-win" => self.codex.send(chat, &message, content),
            "claude-win" => self.claude.send(chat, &message, content),
            "hermes-win" => self.hermes.send(chat, &message, content),
            "dsh-win" => self.dsh.send(chat, &message, content),
            _ => Err("未知成员".into()),
        }
    }

    fn steer(&self, agent: &str, chat: &str, content: &str) -> Result<()> {
        let message = Uuid::new_v4().to_string();
        match agent {
            "codex-win" => self.codex.steer(chat, content, &message),
            "claude-win" => self.claude.steer(chat, content, &message),
            _ => return Err("这位成员不支持回复中插话，可以先 hub_stop_member 再重新派活".into()),
        }?;
        self.store.lock().unwrap().save_delivered_message(chat, &message, content)?;
        Ok(())
    }

    fn cancel(&self, agent: &str, chat: &str) -> Result<()> {
        match agent {
            "codex-win" => self.codex.cancel(chat),
            "claude-win" => self.claude.cancel(chat),
            "hermes-win" => self.hermes.cancel(chat),
            "dsh-win" => self.dsh.cancel(chat),
            _ => return Err("未知成员".into()),
        }
        .map(|_| ())
    }

    fn execute(&self, sql: &str, values: impl rusqlite::Params) {
        let _ = self.store.lock().unwrap().connection.execute(sql, values);
    }

    fn inbox(&self, room: &str, content: &str) {
        self.execute(
            "INSERT INTO brain_inbox(id,room,content,created_at) VALUES(?1,?2,?3,?4)",
            params![Uuid::new_v4().to_string(), room, content, now()],
        );
    }

    fn name(&self, room: &str, seat: &str) -> String {
        self.store.lock().unwrap().seat_name(room, seat)
    }

    fn post(&self, room: &str, content: &str) {
        let store = self.store.lock().unwrap();
        if let Ok(Some(project)) = store.conversation_project(room) {
            let _ = team_message(&store.connection, &project.id, room, None, content);
        }
    }

    fn fail(&self, job: &Pending, error: &str) {
        self.execute(
            "UPDATE brain_jobs SET status='failed',error=?1,finished_at=?2 WHERE id=?3",
            params![error, now(), job.id],
        );
        let what = match job.kind.as_str() {
            "steer" => "插话",
            "stop" => "停止",
            _ => "派活",
        };
        self.inbox(&job.room, &format!("【同席】给 {} 的{what}（任务 {}）没成：{error}", self.name(&job.room, &job.seat), short(&job.id)));
    }

    /// 一轮：发出待发的活、收回做完的活、把回答和结果送进主脑。返回有变化的项目群。
    fn tick(&self) -> Vec<String> {
        let mut rooms: Vec<String> = Vec::new();
        for job in self.pending() {
            if self.start_job(&job) && !rooms.contains(&job.room) {
                rooms.push(job.room.clone());
            }
        }
        for room in self.collect_finished().into_iter().chain(self.collect_answers()).chain(self.deliver()) {
            if !rooms.contains(&room) {
                rooms.push(room);
            }
        }
        rooms
    }

    fn pending(&self) -> Vec<Pending> {
        let store = self.store.lock().unwrap();
        let Ok(mut query) = store.connection.prepare(
            "SELECT id,room,seat,chat,kind,content,model,effort,worktree,branch FROM brain_jobs WHERE status='pending' ORDER BY created_at",
        ) else {
            return Vec::new();
        };
        query
            .query_map([], |row| {
                Ok(Pending {
                    id: row.get(0)?,
                    room: row.get(1)?,
                    seat: row.get(2)?,
                    chat: row.get(3)?,
                    kind: row.get(4)?,
                    content: row.get(5)?,
                    model: row.get(6)?,
                    effort: row.get(7)?,
                    worktree: row.get(8)?,
                    branch: row.get(9)?,
                })
            })
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    /// 处理一条待发的活；还要等（成员没连上、正在回复别的）就返回 false 留到下一轮。
    fn start_job(&self, job: &Pending) -> bool {
        let agent = base_agent(&job.seat).to_owned();
        match job.kind.as_str() {
            "steer" => {
                if !self.live(&agent, &job.chat) {
                    self.fail(job, "成员现在没在回复；要它接着做就用 hub_delegate 再交代一次（同一个会话，它记得做过什么）");
                } else if let Err(error) = self.steer(&agent, &job.chat, &format!("【主脑插话】{}", job.content)) {
                    self.fail(job, &error);
                } else {
                    self.execute("UPDATE brain_jobs SET status='done',finished_at=?1 WHERE id=?2", params![now(), job.id]);
                }
                true
            }
            "stop" => {
                if let Err(error) = self.cancel(&agent, &job.chat) {
                    self.fail(job, &error);
                } else {
                    self.execute("UPDATE brain_jobs SET status='done',finished_at=?1 WHERE id=?2", params![now(), job.id]);
                }
                true
            }
            _ => {
                match self.ready(&agent) {
                    Ok(true) => {}
                    Ok(false) => return false,
                    Err(error) => {
                        self.fail(job, &format!("成员连不上：{error}"));
                        return true;
                    }
                }
                if self.live(&agent, &job.chat) {
                    return false;
                }
                let (root, grants) = {
                    let store = self.store.lock().unwrap();
                    if job.model.is_some() || job.effort.is_some() {
                        let _ = store.connection.execute(
                            "UPDATE sessions SET model=COALESCE(?1,model),reasoning_effort=COALESCE(?2,reasoning_effort) WHERE conversation_id=?3 AND agent_id=?4",
                            params![job.model, job.effort, job.chat, agent],
                        );
                    }
                    let root = store.conversation_project(&job.room).ok().flatten().map(|project| project.root).unwrap_or_default();
                    let grants = store.project_grants(&job.room);
                    (root, grants)
                };
                let cwd = job.worktree.clone().unwrap_or(root);
                let mut content = format!("【主脑派活】任务 {}\n工作目录：{cwd}\n", short(&job.id));
                if let Some(branch) = &job.branch {
                    content.push_str(&format!("这是你自己的工作树，分支 {branch}（从项目当前 HEAD 开出），只在这里改、提交到这个分支。\n"));
                }
                if !grants.trim().is_empty() {
                    content.push_str(&format!("项目长期授权：{}\n", grants.trim()));
                }
                content.push_str(&format!("\n{}\n\n---\n{}", job.content, crate::prompts::member()));
                match self.send(&agent, &job.chat, &content) {
                    Ok(record) => {
                        self.execute(
                            "UPDATE brain_jobs SET status='running',run_id=?1 WHERE id=?2",
                            params![record.id, job.id],
                        );
                        let first: String = job.content.lines().next().unwrap_or("").chars().take(80).collect();
                        self.post(&job.room, &format!("主脑 → {}：{first}", self.name(&job.room, &job.seat)));
                    }
                    Err(error) if error.contains("正在处理") => return false,
                    Err(error) => self.fail(job, &error),
                }
                true
            }
        }
    }

    /// 成员的回复结束了：把结果整理成一条消息放进主脑收件箱。
    fn collect_finished(&self) -> Vec<String> {
        type Row = (String, String, String, String, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>);
        let rows: Vec<Row> = {
            let store = self.store.lock().unwrap();
            let Ok(mut query) = store.connection.prepare(
                "SELECT j.id,j.room,j.seat,j.chat,j.worktree,j.branch,r.status,r.error,m.content FROM brain_jobs j LEFT JOIN runs r ON r.id=j.run_id LEFT JOIN messages m ON m.id=r.assistant_message_id WHERE j.status='running' AND (r.id IS NULL OR r.status IN ('completed','interrupted','failed'))",
            ) else {
                return Vec::new();
            };
            query
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?)))
                .map(|rows| rows.flatten().collect())
                .unwrap_or_default()
        };
        let mut rooms = Vec::new();
        for (id, room, seat, _chat, worktree, branch, status, error, reply) in rows {
            let status = status.unwrap_or_else(|| "failed".into());
            let state = match status.as_str() {
                "completed" => "做完了".to_owned(),
                "interrupted" => "被停下了".to_owned(),
                _ => format!("失败：{}", error.clone().unwrap_or_else(|| "运行记录丢失".into())),
            };
            let name = self.name(&room, &seat);
            let mut report = format!("【成员交回】{name}（{seat}）· 任务 {} · {state}\n", short(&id));
            if let (Some(path), Some(branch)) = (&worktree, &branch) {
                report.push_str(&format!("工作树 {path}，分支 {branch}{}\n", self.branch_summary(&room, path, branch)));
            }
            report.push_str("\n");
            report.push_str(&tail(reply.as_deref().unwrap_or("（没有回复内容）").trim(), REPORT_LIMIT));
            self.inbox(&room, &report);
            let job_status = if status == "completed" { "done" } else { "failed" };
            self.execute(
                "UPDATE brain_jobs SET status=?1,error=?2,finished_at=?3 WHERE id=?4",
                params![job_status, error, now(), id],
            );
            self.post(&room, &format!("{name} 交回任务 {}：{state}", short(&id)));
            rooms.push(room);
        }
        rooms
    }

    fn branch_summary(&self, room: &str, path: &str, branch: &str) -> String {
        let root = {
            let store = self.store.lock().unwrap();
            store.conversation_project(room).ok().flatten().map(|project| PathBuf::from(project.root))
        };
        let Some(root) = root else {
            return String::new();
        };
        let ahead = crate::project_worktree::git_output(&root, &["rev-list", "--count", &format!("HEAD..{branch}")]).unwrap_or_default();
        let stat = crate::project_worktree::git_output(&root, &["diff", "--shortstat", &format!("HEAD...{branch}")]).unwrap_or_default();
        let dirty = crate::project_worktree::git_output(Path::new(path), &["status", "--porcelain"]).is_ok_and(|text| !text.trim().is_empty());
        format!(
            "（比主线多 {} 个提交{}{}）",
            ahead.trim(),
            if stat.trim().is_empty() { String::new() } else { format!("，{}", stat.trim()) },
            if dirty { "；工作树里还有没提交的改动" } else { "" }
        )
    }

    /// 用户在问答面板答了主脑的问题：送进主脑。
    fn collect_answers(&self) -> Vec<String> {
        let rows: Vec<(String, String, String, String)> = {
            let store = self.store.lock().unwrap();
            let Ok(mut query) = store.connection.prepare(&format!(
                "SELECT id,conversation_id,question,COALESCE(answer,'') FROM project_inquiries WHERE status='answered' AND seen=0 AND asked_by LIKE '%-{BRAIN_SLOT}'"
            )) else {
                return Vec::new();
            };
            query
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
                .map(|rows| rows.flatten().collect())
                .unwrap_or_default()
        };
        let mut rooms = Vec::new();
        for (id, room, question, answer) in rows {
            self.inbox(&room, &format!("【用户回答】问：{question}\n答：{answer}"));
            self.execute("UPDATE project_inquiries SET seen=1 WHERE id=?1", [&id]);
            rooms.push(room);
        }
        rooms
    }

    /// 把收件箱里的东西送进主脑：主脑闲着就作为新消息发，正在回复就插话（不支持插话的等它这轮回复完）。
    fn deliver(&self) -> Vec<String> {
        let rooms: Vec<String> = {
            let store = self.store.lock().unwrap();
            store
                .connection
                .prepare("SELECT DISTINCT room FROM brain_inbox WHERE delivered=0")
                .and_then(|mut query| query.query_map([], |row| row.get(0))?.collect())
                .unwrap_or_default()
        };
        let mut delivered = Vec::new();
        for room in rooms {
            let (seat, chat, items) = {
                let mut store = self.store.lock().unwrap();
                let Ok(seat) = brain_seat(&store, &room) else {
                    continue;
                };
                let find = |store: &Store| -> Option<String> {
                    store
                        .connection
                        .query_row(
                            "SELECT conversation_id FROM project_agent_chats WHERE group_conversation_id=?1 AND agent_id=?2",
                            params![room, seat],
                            |row| row.get(0),
                        )
                        .ok()
                };
                let chat = match find(&store) {
                    Some(chat) => chat,
                    None => {
                        let _ = store.ensure_project_agent_chats(&room);
                        match find(&store) {
                            Some(chat) => chat,
                            None => continue,
                        }
                    }
                };
                let items: Vec<(String, String)> = store
                    .connection
                    .prepare("SELECT id,content FROM brain_inbox WHERE room=?1 AND delivered=0 ORDER BY created_at,rowid")
                    .and_then(|mut query| query.query_map([&room], |row| Ok((row.get(0)?, row.get(1)?)))?.collect())
                    .unwrap_or_default();
                (seat, chat, items)
            };
            if items.is_empty() {
                continue;
            }
            let agent = base_agent(&seat).to_owned();
            if !matches!(self.ready(&agent), Ok(true)) {
                continue;
            }
            let content = items.iter().map(|(_, text)| text.as_str()).collect::<Vec<_>>().join("\n\n");
            let sent = if self.live(&agent, &chat) {
                matches!(agent.as_str(), "codex-win" | "claude-win") && self.steer(&agent, &chat, &content).is_ok()
            } else {
                self.send(&agent, &chat, &content).is_ok()
            };
            if sent {
                for (id, _) in &items {
                    self.execute("UPDATE brain_inbox SET delivered=1 WHERE id=?1", [id]);
                }
                delivered.push(room);
            }
        }
        delivered
    }

    /// 各成员能选的模型和思考强度，给主脑的 hub_team 看（工具进程读不到运行时）。
    fn refresh_models(&self) {
        let mut catalog = serde_json::Map::new();
        for agent in EXECUTOR_AGENTS {
            let Some(snapshot) = self.snapshot(agent) else {
                continue;
            };
            // 主脑每次看都要占上下文：模型只给 id 和名字，思考强度合成一份。
            let models: Vec<Value> = snapshot
                .models
                .iter()
                .map(|model| json!({"id":model.id,"name":model.name}))
                .collect();
            let mut efforts: Vec<&String> = Vec::new();
            for effort in snapshot.models.iter().flat_map(|model| &model.efforts) {
                if !efforts.contains(&effort) {
                    efforts.push(effort);
                }
            }
            catalog.insert(
                agent.to_string(),
                json!({"connection":snapshot.connection,"default_model":snapshot.default_model,"models":models,"efforts":efforts}),
            );
        }
        self.execute(
            "INSERT INTO metadata(key,value) VALUES('brain_models',?1) ON CONFLICT(key) DO UPDATE SET value=?1",
            [Value::Object(catalog).to_string()],
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::store::{now, Store};
    use rusqlite::params;
    use uuid::Uuid;

    #[test]
    fn overview_lists_jobs_and_stopping_a_queued_job_cancels_it_and_tells_the_brain() {
        let base = std::env::temp_dir().join(format!("tongxi-brain-overview-{}", Uuid::new_v4()));
        let root = base.join("project");
        std::fs::create_dir_all(&root).unwrap();
        let mut store = Store::open(&base.join("data").join("hub.db")).unwrap();
        let room = store.create("项目群", "group", &["claude-win".into(), "codex-win".into()]).unwrap().id;
        let project = store.register_project("主脑项目", root.to_str().unwrap()).unwrap().id;
        store.bind_project(&room, Some(&project)).unwrap();
        for (id, status, at) in [("job-old", "done", 1), ("job-new", "pending", 2)] {
            store
                .connection
                .execute(
                    "INSERT INTO brain_jobs(id,room,seat,chat,kind,content,model,status,created_at) VALUES(?1,?2,'codex-win','chat-x','task','跑测试','gpt-6-luna',?3,?4)",
                    params![id, room, status, at],
                )
                .unwrap();
        }
        store
            .connection
            .execute("INSERT INTO brain_jobs(id,room,seat,chat,kind,content,status,created_at) VALUES('steer-1',?1,'codex-win','chat-x','steer','改方向','done',?2)", params![room, now()])
            .unwrap();

        let overview = store.brain_overview(&room).unwrap();
        assert_eq!(overview.seat, "claude-win-brain");
        let ids: Vec<&str> = overview.jobs.iter().map(|job| job.id.as_str()).collect();
        assert_eq!(ids, ["job-new", "job-old"], "只列派活，不列插话和停止，最新的在前");
        assert_eq!(overview.jobs[0].model.as_deref(), Some("gpt-6-luna"));
        assert_eq!(overview.waiting, 0);

        store.stop_brain_job(&room, "job-new").unwrap();
        let overview = store.brain_overview(&room).unwrap();
        assert_eq!(overview.jobs[0].status, "failed");
        assert_eq!(overview.jobs[0].error.as_deref(), Some("你在派活页取消了"));
        assert_eq!(overview.waiting, 1, "主脑会收到一条说明");
        assert!(store.stop_brain_job(&room, "job-old").is_err(), "已经结束的活不能再停");
        let _ = std::fs::remove_dir_all(base);
    }
}
