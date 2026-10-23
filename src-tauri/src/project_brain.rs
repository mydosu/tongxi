//! 主脑：每个项目一条常驻的主脑会话（`claude-win-brain` 这类席位），用户直接和它说话；
//! 它按能力把活派给成员会话、看交回的结果、自己合并收口。
//! 派活、插话、停止先记进 brain_jobs（主脑的工具进程只碰数据库），由主程序里的投递线程真正发给成员；
//! 成员交回的结果和用户在问答面板的回答记进 brain_inbox，再作为新消息送进主脑会话（主脑正在回复时用插话）。
use crate::codex::RuntimeSnapshot;
use crate::project_store::{base_agent, is_brain, RoleChoice, BRAIN_SLOT, EXECUTOR_AGENTS};
use crate::store::{now, RunRecord, Store};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::Deserialize;
use sha2::Digest;
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
    .map_err(|_| "主脑结构迁移失败")?;
    crate::project_store::add_column(tx, "brain_jobs", "category", "TEXT")?;
    // 谁送来的（成员席位 / user / hub），送进主脑会话时记在他名下，不混成用户的话。
    crate::project_store::add_column(tx, "brain_inbox", "sender", "TEXT")
}

/// 近 30 天派出的活按「成员 × 模型 × 强度 × 类别」统计做完几件、失败几件、平均用时，给主脑挑人用。
fn track_record(connection: &Connection) -> Vec<Value> {
    #[derive(Default)]
    struct Tally {
        jobs: u32,
        done: u32,
        failed: u32,
        quota: u32,
        stopped: u32,
        minutes: f64,
    }
    let since = now() - 30 * 24 * 3600 * 1000;
    let mut tallies: std::collections::BTreeMap<(String, Option<String>, Option<String>, String), Tally> = Default::default();
    let rows = connection
        .prepare(
            "SELECT j.seat,COALESCE(r.model,j.model),COALESCE(r.reasoning_effort,j.effort),COALESCE(j.category,'未分类'),j.status,r.status,COALESCE(j.error,''),j.created_at,j.finished_at
             FROM brain_jobs j LEFT JOIN runs r ON r.id=j.run_id WHERE j.kind='task' AND j.status IN ('done','failed') AND j.created_at>=?1",
        )
        .and_then(|mut query| {
            query
                .query_map([since], |row| {
                    Ok((row.get::<_, String>(0)?, row.get(1)?, row.get(2)?, row.get::<_, String>(3)?, row.get::<_, String>(4)?, row.get::<_, Option<String>>(5)?, row.get::<_, String>(6)?, row.get::<_, i64>(7)?, row.get::<_, Option<i64>>(8)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .unwrap_or_default();
    for (seat, model, effort, category, status, run, error, created, finished) in rows {
        let tally = tallies.entry((base_agent(&seat).to_owned(), model, effort, category)).or_default();
        tally.jobs += 1;
        if status == "done" {
            tally.done += 1;
            tally.minutes += finished.map_or(0.0, |at| (at - created) as f64 / 60000.0);
        } else if error.contains("额度") || error.contains("频率") {
            tally.quota += 1;
        } else if run.as_deref() == Some("interrupted") {
            tally.stopped += 1;
        } else {
            tally.failed += 1;
        }
    }
    let mut record: Vec<(u32, Value)> = tallies
        .into_iter()
        .map(|((agent, model, effort, category), t)| {
            let minutes = (t.done > 0).then(|| (t.minutes / t.done as f64 * 10.0).round() / 10.0);
            (t.jobs, json!({"agent":agent,"model":model,"effort":effort,"category":category,"jobs":t.jobs,"done":t.done,"failed":t.failed,"quota_failed":t.quota,"stopped":t.stopped,"avg_minutes":minutes}))
        })
        .collect();
    record.sort_by(|a, b| b.0.cmp(&a.0));
    record.into_iter().take(40).map(|(_, row)| row).collect()
}

pub(crate) struct FinishedJob {
    id: String,
    room: String,
    seat: String,
    worktree: Option<String>,
    branch: Option<String>,
    status: Option<String>,
    error: Option<String>,
    reply: Option<String>,
    run_id: Option<String>,
    resumed: bool,
}

impl Store {
    /// 跑完了、该交回主脑的派活。撞额度失败的 Codex 派活会被同席到点自动续跑（在成员会话里补一回合），
    /// 那一回合跑完也算这件活的交回；成员会话之后又被派了新活就不认。
    pub(crate) fn finished_brain_jobs(&self) -> Vec<FinishedJob> {
        let Ok(mut query) = self.connection.prepare(
            "SELECT j.id,j.room,j.seat,j.worktree,j.branch,r.status,r.error,m.content,r.id,j.status='failed' FROM brain_jobs j
             LEFT JOIN runs r ON r.id=CASE WHEN j.status='running' THEN j.run_id
               ELSE (SELECT n.id FROM runs n WHERE n.conversation_id=j.chat ORDER BY n.rowid DESC LIMIT 1) END
             LEFT JOIN messages m ON m.id=r.assistant_message_id
             WHERE (j.status='running' AND (r.id IS NULL OR r.status IN ('completed','interrupted','failed')))
                OR (j.status='failed' AND j.kind='task' AND j.error LIKE '%自动继续%' AND r.id<>j.run_id
                    AND r.status IN ('completed','interrupted','failed')
                    AND NOT EXISTS(SELECT 1 FROM brain_jobs k WHERE k.chat=j.chat AND k.created_at>j.created_at))",
        ) else {
            return Vec::new();
        };
        query
            .query_map([], |row| {
                Ok(FinishedJob {
                    id: row.get(0)?,
                    room: row.get(1)?,
                    seat: row.get(2)?,
                    worktree: row.get(3)?,
                    branch: row.get(4)?,
                    status: row.get(5)?,
                    error: row.get(6)?,
                    reply: row.get(7)?,
                    run_id: row.get(8)?,
                    resumed: row.get(9)?,
                })
            })
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

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
        let root: Option<String> = self
            .connection
            .query_row(
                "SELECT p.root FROM conversation_projects cp JOIN projects p ON p.id=cp.project_id WHERE cp.conversation_id=?1",
                [room],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| "项目目录读取失败")?;
        let plans = root.map(|root| plan_docs(Path::new(&root))).unwrap_or_default();
        let brain_runs = "FROM runs r JOIN messages m ON m.id=r.assistant_message_id WHERE r.conversation_id=?1";
        let target = chat.clone().unwrap_or_default();
        let usage = UsageStats {
            brain_today: usage_sum(&self.connection, brain_runs, &target, true)?,
            brain_total: usage_sum(&self.connection, brain_runs, &target, false)?,
            context: self
                .connection
                .query_row(
                    &format!("SELECT json_extract(r.usage,'$.context') {brain_runs} AND r.usage IS NOT NULL ORDER BY m.created_at DESC LIMIT 1"),
                    [&target],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|_| "主脑用量读取失败")?
                .flatten(),
        };
        Ok(BrainOverview { seat, chat, waiting, jobs, plans, usage })
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
                "INSERT INTO brain_inbox(id,room,content,created_at,sender) VALUES(?1,?2,?3,?4,'hub')",
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

/// 一段时间里的 token 合计：输入含缓存读写，新写入 = input - cache_read - cache_write。
#[derive(serde::Serialize, Default, Debug, PartialEq)]
pub struct UsageSum {
    turns: i64,
    input: i64,
    cache_read: i64,
    cache_write: i64,
    output: i64,
}

#[derive(serde::Serialize)]
pub struct UsageStats {
    brain_today: UsageSum,
    brain_total: UsageSum,
    /// 主脑最近一回合结束时的上下文大小。
    context: Option<i64>,
}

/// 主脑按回合报的用量加起来；today 只算本地时间今天的。
fn usage_sum(connection: &Connection, runs: &str, target: &str, today: bool) -> Result<UsageSum> {
    let day = if today { " AND date(m.created_at/1000,'unixepoch','localtime')=date('now','localtime')" } else { "" };
    connection
        .query_row(
            &format!(
                "SELECT count(r.usage),
                        COALESCE(sum(json_extract(r.usage,'$.usage.inputTokens')),0),
                        COALESCE(sum(json_extract(r.usage,'$.usage.cachedReadTokens')),0),
                        COALESCE(sum(json_extract(r.usage,'$.usage.cachedWriteTokens')),0),
                        COALESCE(sum(json_extract(r.usage,'$.usage.outputTokens')),0)
                 {runs} AND r.usage IS NOT NULL{day}"
            ),
            [target],
            |row| Ok(UsageSum { turns: row.get(0)?, input: row.get(1)?, cache_read: row.get(2)?, cache_write: row.get(3)?, output: row.get(4)? }),
        )
        .map_err(|_| "用量统计读取失败".into())
}

#[derive(serde::Serialize)]
pub struct BrainOverview {
    seat: String,
    chat: Option<String>,
    /// 交回的结果、问答回答里还没送进主脑会话的条数。
    waiting: i64,
    jobs: Vec<BrainJob>,
    plans: Vec<PlanDoc>,
    usage: UsageStats,
}

#[derive(serde::Serialize)]
pub struct PlanDoc {
    name: String,
    /// 任务书（.tongxi/任务书/*.md）还是长期计划（.tongxi/计划.md）。
    brief: bool,
    text: String,
    modified: i64,
}

/// 派活页的「计划与任务书」：只读主脑写在项目里的文件，最近改过的任务书在前，计划放最后。
fn plan_docs(root: &Path) -> Vec<PlanDoc> {
    let read = |path: &Path, brief: bool| -> Option<PlanDoc> {
        let modified = path.metadata().ok()?.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as i64;
        Some(PlanDoc { name: path.file_stem()?.to_string_lossy().into(), brief, text: std::fs::read_to_string(path).ok()?, modified })
    };
    let dir = root.join(".tongxi");
    let mut docs: Vec<PlanDoc> = std::fs::read_dir(dir.join("任务书"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .filter_map(|path| read(&path, true))
        .collect();
    docs.sort_by(|a, b| b.modified.cmp(&a.modified));
    docs.extend(read(&dir.join("计划.md"), false));
    docs
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

/// 成员会话现在多大、上件活交回到现在几分钟，给主脑权衡续用还是新开。
/// 上下文取同一原生会话最后一回合报的数；会话刚清掉算 0；报不出来的成员是 null。
fn session_state(connection: &Connection, seat: &str, chat: &str) -> (Option<i64>, Option<i64>) {
    let native: Option<String> = connection
        .query_row("SELECT native_session_id FROM sessions WHERE conversation_id=?1 AND agent_id=?2", params![chat, base_agent(seat)], |row| row.get(0))
        .ok()
        .flatten();
    let context = match native {
        None => Some(0),
        Some(native) => connection
            .query_row(
                "SELECT usage FROM runs WHERE conversation_id=?1 AND native_thread_id=?2 AND usage IS NOT NULL ORDER BY rowid DESC LIMIT 1",
                params![chat, native],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .and_then(|usage| serde_json::from_str::<Value>(&usage).ok())
            .and_then(|usage| usage["context"].as_i64()),
    };
    let idle = connection
        .query_row("SELECT MAX(finished_at) FROM brain_jobs WHERE seat=?1 AND chat=?2 AND kind='task'", params![seat, chat], |row| row.get::<_, Option<i64>>(0))
        .ok()
        .flatten()
        .map(|at| (now() - at).max(0) / 60_000);
    (context, idle)
}

pub fn tool_specs() -> Vec<Value> {
    let string = json!({"type":"string"});
    let definitions = [
        ("hub_team", "List the members you can delegate to (seat id, name, base agent, busy or idle, current model), the models each base agent can use with their effort levels, current subscription quota (shared by all projects), and other_projects: which base agents (members, not your seats) other projects' brains have working right now, sharing the same quota, and track_record: over the last 30 days, per base agent × model × effort × category, how many delegated jobs were done, failed, failed on quota, stopped, and the average minutes of done ones. Each member also has context (tokens in its current session after its last turn; 0 = fresh, null = unknown) and idle_minutes (since its last job came back). Check this before delegating.", json!({}), vec![]),
        ("hub_delegate", "Hand a task to a member. Returns immediately with a job id; the member works in its own session and its result arrives later as a new message to you. Write the task concretely: goal, where to change, what counts as done, what to hand back. model/effort are optional (default: the member session's current setting). worktree=true gives the member its own git worktree and branch from the current HEAD; use it for code changes, not for read-only, test or deploy work. category: a short label of the kind of work (e.g. 实现, 排查, 测试构建, 审查, 文档, 部署); keep using the same labels, track_record in hub_team is grouped by it. fresh=true starts the member in a new session (it forgets earlier jobs; write all the background it needs into task); default continues its current session.", json!({"member":string,"task":string,"model":string,"effort":string,"worktree":{"type":"boolean"},"category":string,"fresh":{"type":"boolean"}}), vec!["member","task"]),
        ("hub_member", "Look at one member: whether it is busy, its recent jobs and the tail of its last reply.", json!({"member":string}), vec!["member"]),
        ("hub_tell_member", "Interject into a member's reply that is in progress (to correct course or add information). Only works while the member is replying; to continue with an idle member, use hub_delegate again (same session, it remembers what it did).", json!({"member":string,"content":string}), vec!["member","content"]),
        ("hub_stop_member", "Stop a member's reply in progress.", json!({"member":string}), vec!["member"]),
        ("hub_finish_worktree", "Close out a finished job that had a worktree: action=merge merges its branch into the project's current branch and removes the worktree; action=discard throws the work away. On merge conflicts the merge is left in the project directory for you to resolve.", json!({"job":string,"action":{"type":"string","enum":["merge","discard"]}}), vec!["job","action"]),
        ("hub_ask_user", "Put a question on the user's Q&A panel (does not wait). Only for what to build, or unauthorized outward actions that can't be undone; decide everything else yourself. key identifies the matter (same key = same question). For reversible matters, give default and go ahead with it meanwhile; for irreversible ones set reversible=false and work on something else. The answer arrives later as a new message.", json!({"key":string,"question":string,"background":string,"options":{"type":"array","items":string},"default":string,"reversible":{"type":"boolean"}}), vec!["question"]),
        ("hub_broadcast", "Post one short progress line to the project group the user watches (started what, delegated to whom, finished what, needs the user to decide what). Do not flood it. When a round of work is done, also post the progress report here: done, not done and why, next step, what the user must decide.", json!({"content":string}), vec!["content"]),
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
                    let (context, idle_minutes) = session_state(connection, &seat, &chat);
                    json!({"seat":seat,"name":label(connection,&ctx.room,&seat),"agent":base_agent(&seat),"busy":busy(connection,&seat,&chat),"model":model,"effort":effort,"context":context,"idle_minutes":idle_minutes})
                })
                .collect();
            let parse = |key: &str| metadata(connection, key).and_then(|text| serde_json::from_str::<Value>(&text).ok()).unwrap_or(Value::Null);
            // 额度是几个项目共用的：列出别的项目的主脑正交给哪些成员在做。
            let elsewhere: Vec<Value> = connection
                .prepare(
                    "SELECT j.seat,j.model,(SELECT p.name FROM project_agent_chats c JOIN projects p ON p.id=c.project_id WHERE c.group_conversation_id=j.room LIMIT 1)
                     FROM brain_jobs j WHERE j.room<>?1 AND j.kind='task' AND j.status IN ('pending','running') ORDER BY j.created_at",
                )
                .and_then(|mut query| {
                    query
                        .query_map([&ctx.room], |row| {
                            // 席位 id 各项目自己编，跨项目会重名，只给成员（共用同一份额度）。
                            Ok(json!({"project":row.get::<_,Option<String>>(2)?,"agent":base_agent(&row.get::<_,String>(0)?),"model":row.get::<_,Option<String>>(1)?}))
                        })?
                        .collect()
                })
                .unwrap_or_default();
            Ok(json!({"members":members,"models":parse("brain_models"),"quota":parse("brain_quota"),"other_projects":elsewhere,"track_record":track_record(connection)}))
        }
        "hub_delegate" => {
            let (seat, chat) = find_member(connection, &ctx, args["member"].as_str().unwrap_or(""))?;
            if let Some(job) = busy(connection, &seat, &chat) {
                return Err(format!("{seat} 手上还有活（{}），等它交回，或用 hub_tell_member / hub_stop_member", short(&job)));
            }
            let task = text_arg(&args, "task", 20000)?;
            if args["fresh"].as_bool() == Some(true) {
                let live: bool = connection
                    .query_row("SELECT EXISTS(SELECT 1 FROM runs WHERE conversation_id=?1 AND status IN ('starting','running','cancelling'))", [&chat], |row| row.get(0))
                    .unwrap_or(true);
                if live {
                    return Err(format!("{seat} 正在回复，不能新开会话；等它停下再派"));
                }
                connection
                    .execute("UPDATE sessions SET native_session_id=NULL WHERE conversation_id=?1 AND agent_id=?2", params![chat, base_agent(&seat)])
                    .map_err(|_| "新开会话失败")?;
            }
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
            // 没指定的沿用席位上次的设置（上次派活指定过的模型会留在席位上），告诉主脑实际用哪个，连同这家现在的额度。
            let agent = base_agent(&seat);
            let (seat_model, seat_effort): (Option<String>, Option<String>) = connection
                .query_row("SELECT model,reasoning_effort FROM sessions WHERE conversation_id=?1 AND agent_id=?2", params![chat, agent], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap_or((None, None));
            let runs_with = json!({"model": model.clone().or(seat_model), "effort": effort.clone().or(seat_effort)});
            let quota: serde_json::Map<String, Value> = metadata(connection, "brain_quota")
                .and_then(|text| serde_json::from_str::<serde_json::Map<String, Value>>(&text).ok())
                .unwrap_or_default()
                .into_iter()
                .filter(|(key, _)| key == agent || key.starts_with(&format!("{agent}:")))
                .collect();
            let job = insert_job(connection, &ctx, &seat, &chat, "task", &task, (model, effort, worktree.clone(), branch.clone()))?;
            if let Some(category) = optional_arg(&args, "category") {
                connection
                    .execute("UPDATE brain_jobs SET category=?1 WHERE id=?2", params![category.chars().take(20).collect::<String>(), job])
                    .map_err(|_| "任务保存失败")?;
            }
            Ok(json!({"job":short(&job),"member":seat,"runs_with":runs_with,"quota":quota,"worktree":worktree,"branch":branch,"note":"已交给成员，它做完后结果会作为新消息发给你；不用等也不用查"}))
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
            Ok(json!({"seat":seat,"busy":busy(connection,&seat,&chat),"jobs":jobs,"last_reply_tail":last.map(|text| tail(&text, REPORT_LIMIT))}))
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

/// 主脑会话每开一条新的原生会话，第一条消息前面带上主脑说明、模型档案和项目信息；之后的消息不再重复，
/// 除非这段内容变了（改了说明文件或项目授权），那就在下一条消息前重新带一次。
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
    let (name, root): (String, String) = connection
        .query_row("SELECT name,root FROM projects WHERE id=?1", [&project], |row| Ok((row.get(0)?, row.get(1)?)))
        .ok()?;
    let grants = metadata(&connection, &format!("project_grants:{room}")).filter(|text| !text.trim().is_empty());
    let text = format!(
        "{}

## 模型档案
文件 {}，所有项目共用，由你调研维护。
{}

## 本项目
项目「{name}」，目录 {root}。计划文件是 .tongxi/计划.md，任务书放在 .tongxi/任务书/，交接写在 .tongxi/交接.md。
项目长期授权：{}",
        crate::prompts::brain(),
        crate::prompts::prompt_files("models.md").first().map_or("（未设置数据目录）".into(), |path| path.display().to_string()),
        crate::prompts::models(),
        grants.as_deref().unwrap_or("（没有）"),
    );
    let digest = format!("{:x}", sha2::Sha256::digest(text.as_bytes()));
    let primed = format!("{native}:{digest}");
    let key = format!("brain_primed:{conversation_id}");
    let stored = metadata(&connection, &key);
    // 原生会话还没建时记下的标记，属于随这条消息新建的那个会话。
    if !native.is_empty() && (stored.as_deref() == Some(primed.as_str()) || stored == Some(format!(":{digest}"))) {
        if stored.as_deref() != Some(primed.as_str()) {
            let _ = connection.execute("UPDATE metadata SET value=?2 WHERE key=?1", params![key, primed]);
        }
        return Some(None);
    }
    connection
        .execute("INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=?2", params![key, primed])
        .ok()?;
    // 新会话带上上一轮的交接；交接不算进说明的指纹，主脑改交接不会让说明重发。
    let handoff = native
        .is_empty()
        .then(|| std::fs::read_to_string(handoff_path(&root)).ok())
        .flatten()
        .filter(|text| !text.trim().is_empty())
        .map(|text| format!("

## 上一轮的交接
这是新会话。下面是你上一轮做完时写的交接，照着接上：

{}", tail(text.trim(), RESULT_LIMIT)))
        .unwrap_or_default();
    Some(Some(format!("{text}{handoff}

---
以下是用户的消息：")))
}

fn handoff_path(root: &str) -> PathBuf {
    Path::new(root).join(".tongxi").join("交接.md")
}

/// 主脑一轮做完写了新的交接（.tongxi/交接.md 比上次换会话时新），项目里也没有在等的活：
/// 清掉主脑的原生会话，这条消息开新会话，开场说明里带上交接。没写交接就一直续旧会话。
pub fn rollover(connection: &Connection, conversation_id: &str) {
    let Ok((room, seat, root)) = connection.query_row(
        "SELECT c.group_conversation_id,c.agent_id,p.root FROM project_agent_chats c JOIN projects p ON p.id=c.project_id WHERE c.conversation_id=?1",
        [conversation_id],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)),
    ) else {
        return;
    };
    if !is_brain(&seat) {
        return;
    }
    let Some(written) = std::fs::metadata(handoff_path(&root))
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|at| at.as_millis() as i64)
    else {
        return;
    };
    let key = format!("brain_handoff:{conversation_id}");
    if metadata(connection, &key).and_then(|seen| seen.parse::<i64>().ok()).is_some_and(|seen| seen >= written) {
        return;
    }
    let waiting: i64 = connection
        .query_row("SELECT COUNT(*) FROM brain_jobs WHERE room=?1 AND kind='task' AND status IN ('pending','running')", [&room], |row| row.get(0))
        .unwrap_or(1);
    if waiting > 0 {
        return;
    }
    let _ = connection.execute(
        "UPDATE sessions SET native_session_id=NULL WHERE conversation_id=?1 AND agent_id=?2",
        params![conversation_id, base_agent(&seat)],
    );
    let _ = connection.execute(
        "INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=?2",
        params![key, written.to_string()],
    );
}

/// 成员按说明把交回写在最后一段「## 交回」里，主脑只收这段；没写就收结尾一截。
fn key_points(reply: &str) -> String {
    let reply = reply.trim();
    match reply.rfind("## 交回") {
        Some(at) if at > 0 => format!("{}

（只送了交回这段，过程要看用 hub_member）", tail(&reply[at..], REPORT_LIMIT)),
        _ => tail(reply, REPORT_LIMIT),
    }
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

    /// 先按真正的发送者存好消息，运行时再按同一 id 保存时沿用它；没发出去就删掉，免得重试时攒下一串没发的。
    fn send(&self, agent: &str, chat: &str, content: &str, sender: &str) -> Result<RunRecord> {
        let message = Uuid::new_v4().to_string();
        self.store.lock().unwrap().save_message_from(chat, &message, content, sender)?;
        let sent = match agent {
            "codex-win" => self.codex.send(chat, &message, content),
            "claude-win" => self.claude.send(chat, &message, content),
            "hermes-win" => self.hermes.send(chat, &message, content),
            "dsh-win" => self.dsh.send(chat, &message, content),
            _ => Err("未知成员".into()),
        };
        if sent.is_err() {
            self.execute("DELETE FROM messages WHERE id=?1 AND status='local_only'", [&message]);
        }
        sent
    }

    fn steer(&self, agent: &str, chat: &str, content: &str, sender: &str) -> Result<()> {
        let message = Uuid::new_v4().to_string();
        match agent {
            "codex-win" => self.codex.steer(chat, content, &message),
            "claude-win" => self.claude.steer(chat, content, &message),
            _ => return Err("这位成员不支持回复中插话，可以先 hub_stop_member 再重新派活".into()),
        }?;
        self.store.lock().unwrap().save_delivered_message_from(chat, &message, content, sender)?;
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

    fn inbox(&self, room: &str, content: &str, sender: &str) {
        self.execute(
            "INSERT INTO brain_inbox(id,room,content,created_at,sender) VALUES(?1,?2,?3,?4,?5)",
            params![Uuid::new_v4().to_string(), room, content, now(), sender],
        );
    }

    fn name(&self, room: &str, seat: &str) -> String {
        self.store.lock().unwrap().seat_name(room, seat)
    }

    /// 派活、插话记在主脑席位名下。
    fn brain(&self, room: &str) -> String {
        brain_seat(&self.store.lock().unwrap(), room).unwrap_or_else(|_| "hub".into())
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
        self.inbox(&job.room, &format!("【同席】给 {} 的{what}（任务 {}）没成：{error}", self.name(&job.room, &job.seat), short(&job.id)), "hub");
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
                } else if let Err(error) = self.steer(&agent, &job.chat, &format!("【主脑插话】{}", job.content), &self.brain(&job.room)) {
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
                match self.send(&agent, &job.chat, &content, &self.brain(&job.room)) {
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
        let rows = self.store.lock().unwrap().finished_brain_jobs();
        let mut rooms = Vec::new();
        for FinishedJob { id, room, seat, worktree, branch, status, error, reply, run_id, resumed } in rows {
            let status = status.unwrap_or_else(|| "failed".into());
            let state = match status.as_str() {
                "completed" if resumed => "额度恢复后接着做完了".to_owned(),
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
            report.push_str(&key_points(reply.as_deref().unwrap_or("（没有回复内容）")));
            self.inbox(&room, &report, &seat);
            let job_status = if status == "completed" { "done" } else { "failed" };
            self.execute(
                "UPDATE brain_jobs SET status=?1,error=?2,finished_at=?3,run_id=COALESCE(?4,run_id) WHERE id=?5",
                params![job_status, error, now(), run_id, id],
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

    /// 用户在问答面板答了主脑的问题：送进主脑。旧调度（coordinator）问过、用户后来才答的也归主脑接手。
    fn collect_answers(&self) -> Vec<String> {
        let rows: Vec<(String, String, String, String)> = {
            let store = self.store.lock().unwrap();
            let Ok(mut query) = store.connection.prepare(&format!(
                "SELECT id,conversation_id,question,COALESCE(answer,'') FROM project_inquiries WHERE status='answered' AND seen=0 AND (asked_by LIKE '%-{BRAIN_SLOT}' OR asked_by='coordinator')"
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
            self.inbox(&room, &format!("【用户回答】问：{question}\n答：{answer}"), "user");
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
            let (seat, chat, items, hold) = {
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
                let items: Vec<(String, String, String)> = store
                    .connection
                    .prepare("SELECT id,content,COALESCE(sender,'hub') FROM brain_inbox WHERE room=?1 AND delivered=0 ORDER BY created_at,rowid")
                    .and_then(|mut query| query.query_map([&room], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?.collect())
                    .unwrap_or_default();
                // 还有成员在做时，交回的先攒着，收件箱静下来 30 秒再一起送，少唤醒主脑几次。
                let hold: bool = store
                    .connection
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM brain_jobs WHERE room=?1 AND kind='task' AND status IN ('pending','running'))
                           AND EXISTS(SELECT 1 FROM brain_inbox WHERE room=?1 AND delivered=0 AND created_at>?2)",
                        params![room, now() - 30_000],
                        |row| row.get(0),
                    )
                    .unwrap_or(false);
                (seat, chat, items, hold)
            };
            if items.is_empty() {
                continue;
            }
            let agent = base_agent(&seat).to_owned();
            if !matches!(self.ready(&agent), Ok(true)) {
                continue;
            }
            let content = items.iter().map(|(_, text, _)| text.as_str()).collect::<Vec<_>>().join("\n\n");
            // 攒在一起的来自不同人时，记成同席转来的。
            let sender = if items.iter().all(|(_, _, from)| *from == items[0].2) { items[0].2.as_str() } else { "hub" };
            let sent = if self.live(&agent, &chat) {
                matches!(agent.as_str(), "codex-win" | "claude-win") && self.steer(&agent, &chat, &content, sender).is_ok()
            } else if hold {
                false
            } else {
                self.send(&agent, &chat, &content, sender).is_ok()
            };
            if sent {
                for (id, _, _) in &items {
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
        assert!(overview.plans.is_empty(), "主脑还没写计划时什么都不列");

        std::fs::create_dir_all(root.join(".tongxi/任务书")).unwrap();
        std::fs::write(root.join(".tongxi/计划.md"), "# 计划").unwrap();
        std::fs::write(root.join(".tongxi/任务书/删旧.md"), "- [x] 一
- [ ] 二").unwrap();
        std::fs::write(root.join(".tongxi/任务书/草稿.txt"), "不是任务书").unwrap();
        let plans = store.brain_overview(&room).unwrap().plans;
        let names: Vec<(&str, bool)> = plans.iter().map(|doc| (doc.name.as_str(), doc.brief)).collect();
        assert_eq!(names, [("删旧", true), ("计划", false)], "只列 .md 任务书，计划放最后");

        store.stop_brain_job(&room, "job-new").unwrap();
        let overview = store.brain_overview(&room).unwrap();
        assert_eq!(overview.jobs[0].status, "failed");
        assert_eq!(overview.jobs[0].error.as_deref(), Some("你在派活页取消了"));
        assert_eq!(overview.waiting, 1, "主脑会收到一条说明");
        assert!(store.stop_brain_job(&room, "job-old").is_err(), "已经结束的活不能再停");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn relayed_messages_keep_their_real_sender_and_old_ones_are_relabelled() {
        let base = std::env::temp_dir().join(format!("tongxi-brain-sender-{}", Uuid::new_v4()));
        let root = base.join("project");
        std::fs::create_dir_all(&root).unwrap();
        let db = base.join("data").join("hub.db");
        let mut store = Store::open(&db).unwrap();
        let room = store.create("项目群", "group", &["claude-win".into(), "codex-win".into()]).unwrap().id;
        let project = store.register_project("主脑项目", root.to_str().unwrap()).unwrap().id;
        store.bind_project(&room, Some(&project)).unwrap();
        store.ensure_project_agent_chats(&room).unwrap();
        let chat = |store: &Store, seat: &str| -> String {
            store.connection.query_row("SELECT conversation_id FROM project_agent_chats WHERE group_conversation_id=?1 AND agent_id=?2", params![room, seat], |row| row.get(0)).unwrap()
        };
        let (brain, member) = (chat(&store, "claude-win-brain"), chat(&store, "claude-win"));
        let say = |store: &mut Store, chat: &str, content: &str, sender: &str| -> String {
            let id = Uuid::new_v4().to_string();
            store.save_delivered_message_from(chat, &id, content, sender).unwrap();
            id
        };
        // 运行时随后按同一 id 再存一次，发送者不能被改回用户。
        let report = say(&mut store, &brain, "【成员交回】Claude（claude-win）· 任务 a · 做完了", "claude-win");
        assert_eq!(store.save_message(&brain, &report, "【成员交回】Claude（claude-win）· 任务 a · 做完了").unwrap().sender_id, "claude-win");
        let old_task = say(&mut store, &member, "【主脑派活】任务 b\n做事", "user");
        let old_report = say(&mut store, &brain, "【成员交回】Codex（codex-win）· 任务 c · 做完了", "user");
        let old_batch = say(&mut store, &brain, "【同席】给 Codex 的派活没成\n\n【成员交回】Codex（codex-win）· 任务 d", "user");
        let old_answer = say(&mut store, &brain, "【用户回答】问：要不要发版\n答：发", "user");
        let typed = say(&mut store, &brain, "帮我看看【主脑派活】这几个字", "user");
        drop(store);

        let store = Store::open(&db).unwrap();
        let sender = |id: &str| -> String { store.connection.query_row("SELECT sender_id FROM messages WHERE id=?1", [id], |row| row.get(0)).unwrap() };
        assert_eq!(sender(&report), "claude-win", "成员交回不是主脑的回复，启动迁移不能把它改到主脑名下");
        assert_eq!(sender(&old_task), "claude-win-brain");
        assert_eq!(sender(&old_report), "codex-win");
        assert_eq!(sender(&old_batch), "hub", "攒在一起、来自不同人的记成同席");
        assert_eq!(sender(&old_answer), "user", "问答面板的回答本来就是用户的话");
        assert_eq!(sender(&typed), "user");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn brain_preamble_is_sent_once_and_again_after_it_changes() {
        let base = std::env::temp_dir().join(format!("tongxi-brain-preamble-{}", Uuid::new_v4()));
        let root = base.join("project");
        std::fs::create_dir_all(&root).unwrap();
        let db = base.join("data").join("hub.db");
        let mut store = Store::open(&db).unwrap();
        let room = store.create("项目群", "group", &["claude-win".into()]).unwrap().id;
        let project = store.register_project("主脑项目", root.to_str().unwrap()).unwrap().id;
        store.bind_project(&room, Some(&project)).unwrap();
        let chat = store
            .ensure_project_agent_chats(&room)
            .unwrap()
            .into_iter()
            .find(|chat| chat.agent_id == "claude-win-brain")
            .unwrap()
            .conversation_id;
        store.connection.execute("UPDATE sessions SET native_session_id='native-1' WHERE conversation_id=?1", [&chat]).unwrap();

        assert!(matches!(super::preamble(&db, &chat), Some(Some(_))), "第一条带说明");
        assert_eq!(super::preamble(&db, &chat), Some(None), "说明没变就不再带");
        store
            .connection
            .execute("INSERT INTO metadata(key,value) VALUES(?1,'可以推送')", [format!("project_grants:{room}")])
            .unwrap();
        let again = super::preamble(&db, &chat).unwrap().expect("授权改了要重新带说明");
        assert!(again.contains("可以推送"));
        assert_eq!(super::preamble(&db, &chat), Some(None));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn a_new_handoff_starts_a_fresh_brain_session_that_carries_it() {
        let base = std::env::temp_dir().join(format!("tongxi-brain-handoff-{}", Uuid::new_v4()));
        let root = base.join("project");
        std::fs::create_dir_all(root.join(".tongxi")).unwrap();
        let db = base.join("data").join("hub.db");
        let mut store = Store::open(&db).unwrap();
        let room = store.create("项目群", "group", &["claude-win".into()]).unwrap().id;
        let project = store.register_project("主脑项目", root.to_str().unwrap()).unwrap().id;
        store.bind_project(&room, Some(&project)).unwrap();
        let chat = store
            .ensure_project_agent_chats(&room)
            .unwrap()
            .into_iter()
            .find(|chat| chat.agent_id == "claude-win-brain")
            .unwrap()
            .conversation_id;
        let native = |store: &Store| -> Option<String> {
            store.connection.query_row("SELECT native_session_id FROM sessions WHERE conversation_id=?1", [&chat], |row| row.get(0)).unwrap()
        };
        let set_native = |store: &Store, id: &str| {
            store.connection.execute("UPDATE sessions SET native_session_id=?1 WHERE conversation_id=?2", params![id, chat]).unwrap();
        };

        // 新会话的第一条带说明；会话建好后不再重带。
        assert!(matches!(super::preamble(&db, &chat), Some(Some(_))));
        set_native(&store, "native-1");
        assert_eq!(super::preamble(&db, &chat), Some(None), "随第一条消息建的会话已经带过说明");

        super::rollover(&store.connection, &chat);
        assert_eq!(native(&store).as_deref(), Some("native-1"), "没写交接就续旧会话");

        std::fs::write(root.join(".tongxi/交接.md"), "B6 做到第二步").unwrap();
        store
            .connection
            .execute("INSERT INTO brain_jobs(id,room,seat,chat,kind,content,status,created_at) VALUES('job-1',?1,'codex-win','chat-x','task','做事','running',?2)", params![room, now()])
            .unwrap();
        super::rollover(&store.connection, &chat);
        assert_eq!(native(&store).as_deref(), Some("native-1"), "还有活没交回不换");

        store.connection.execute("UPDATE brain_jobs SET status='done'", []).unwrap();
        super::rollover(&store.connection, &chat);
        assert_eq!(native(&store), None, "一轮做完写了交接就换新会话");
        let opening = super::preamble(&db, &chat).unwrap().expect("新会话带说明");
        assert!(opening.contains("B6 做到第二步"), "新会话带上交接");
        assert!(opening.ends_with("以下是用户的消息："));

        set_native(&store, "native-2");
        super::rollover(&store.connection, &chat);
        assert_eq!(native(&store).as_deref(), Some("native-2"), "同一份交接只换一次");
        assert_eq!(super::preamble(&db, &chat), Some(None));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn hub_team_shows_what_other_projects_have_members_doing() {
        let base = std::env::temp_dir().join(format!("tongxi-brain-team-{}", Uuid::new_v4()));
        let mut store = Store::open(&base.join("data").join("hub.db")).unwrap();
        let mut brain = |name: &str| {
            let root = base.join(name);
            std::fs::create_dir_all(&root).unwrap();
            let room = store.create(name, "group", &["claude-win".into(), "codex-win".into()]).unwrap().id;
            let project = store.register_project(name, root.to_str().unwrap()).unwrap().id;
            store.bind_project(&room, Some(&project)).unwrap();
            let chat = store.ensure_project_agent_chats(&room).unwrap().into_iter().find(|chat| chat.agent_id == "claude-win-brain").unwrap().conversation_id;
            (room, chat)
        };
        let (room_a, chat_a) = brain("甲项目");
        let (room_b, _) = brain("乙项目");
        for (id, room, status) in [("job-a", &room_a, "running"), ("job-b", &room_b, "running"), ("job-old", &room_b, "done")] {
            store
                .connection
                .execute("INSERT INTO brain_jobs(id,room,seat,chat,kind,content,model,status,created_at) VALUES(?1,?2,'codex-win-2','chat-x','task','做事','gpt-6-luna',?3,?4)", params![id, room, status, now()])
                .unwrap();
        }
        let team = super::call(&store.connection, &chat_a, "hub_team", serde_json::Value::Null).unwrap();
        let others = team["other_projects"].as_array().unwrap();
        assert_eq!(others.len(), 1, "只列别的项目、还在做的活");
        assert_eq!(others[0]["project"], "乙项目");
        assert_eq!(others[0]["agent"], "codex-win");
        assert_eq!(others[0]["model"], "gpt-6-luna");
        drop(store);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn hub_team_shows_member_context_and_fresh_delegation_starts_a_new_session() {
        let base = std::env::temp_dir().join(format!("tongxi-brain-fresh-{}", Uuid::new_v4()));
        let root = base.join("project");
        std::fs::create_dir_all(&root).unwrap();
        let mut store = Store::open(&base.join("data").join("hub.db")).unwrap();
        let room = store.create("项目群", "group", &["claude-win".into(), "codex-win".into()]).unwrap().id;
        let project = store.register_project("主脑项目", root.to_str().unwrap()).unwrap().id;
        store.bind_project(&room, Some(&project)).unwrap();
        let chats = store.ensure_project_agent_chats(&room).unwrap();
        let chat = |seat: &str| chats.iter().find(|chat| chat.agent_id == seat).unwrap().conversation_id.clone();
        let (brain, member) = (chat("claude-win-brain"), chat("codex-win"));
        let message = Uuid::new_v4().to_string();
        store.save_message(&member, &message, "上一件活").unwrap();
        let run = store.insert_run(&member, &message, "codex-win", None, None).unwrap().id;
        store
            .connection
            .execute_batch(&format!(
                "UPDATE sessions SET native_session_id='thread-1' WHERE conversation_id='{member}' AND agent_id='codex-win';
                 UPDATE runs SET status='completed',native_thread_id='thread-1',usage='{{\"context\":91000}}' WHERE id='{run}';"
            ))
            .unwrap();
        store
            .connection
            .execute("INSERT INTO brain_jobs(id,room,seat,chat,kind,content,status,created_at,finished_at) VALUES('job-1',?1,'codex-win',?2,'task','上一件活','done',?3,?4)", params![room, member, now(), now() - 30 * 60_000])
            .unwrap();
        let codex = |store: &Store| {
            let team = super::call(&store.connection, &brain, "hub_team", serde_json::Value::Null).unwrap();
            team["members"].as_array().unwrap().iter().find(|seat| seat["seat"] == "codex-win").unwrap().clone()
        };
        let before = codex(&store);
        assert_eq!(before["context"], 91000, "上下文取同一原生会话最后一回合");
        assert_eq!(before["idle_minutes"], 30);

        super::call(&store.connection, &brain, "hub_delegate", serde_json::json!({"member":"codex-win","task":"不相关的新活","fresh":true})).unwrap();
        let native: Option<String> = store.connection.query_row("SELECT native_session_id FROM sessions WHERE conversation_id=?1 AND agent_id='codex-win'", [&member], |row| row.get(0)).unwrap();
        assert_eq!(native, None, "fresh 清掉原生会话，运行时会开新的");
        assert_eq!(codex(&store)["context"], 0);
        drop(store);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn a_job_failed_on_quota_comes_back_when_the_auto_resume_finishes() {
        let base = std::env::temp_dir().join(format!("tongxi-brain-resume-{}", Uuid::new_v4()));
        let root = base.join("project");
        std::fs::create_dir_all(&root).unwrap();
        let mut store = Store::open(&base.join("data").join("hub.db")).unwrap();
        let room = store.create("项目群", "group", &["codex-win".into()]).unwrap().id;
        let project = store.register_project("续跑项目", root.to_str().unwrap()).unwrap().id;
        store.bind_project(&room, Some(&project)).unwrap();
        let chat = store.ensure_project_agent_chats(&room).unwrap().into_iter().find(|chat| chat.agent_id == "codex-win").unwrap().conversation_id;
        let mut failed = store.begin_agent_run(&chat, &Uuid::new_v4().to_string(), "做 B6", "codex-win").unwrap();
        failed.status = "failed".into();
        failed.error = Some("Codex 额度限制，额度恢复后同席会自动继续".into());
        store.checkpoint(&failed, "", "").unwrap();
        store
            .connection
            .execute("INSERT INTO brain_jobs(id,room,seat,chat,kind,content,status,run_id,error,created_at) VALUES('job-b6',?1,'codex-win',?2,'task','做 B6','failed',?3,?4,?5)", params![room, chat, failed.id, failed.error, now()])
            .unwrap();
        assert!(store.finished_brain_jobs().is_empty(), "失败已经交回过，续跑前不再送");

        let mut resumed = store.begin_agent_run(&chat, &Uuid::new_v4().to_string(), "继续", "codex-win").unwrap();
        assert!(store.finished_brain_jobs().is_empty(), "续跑还没做完");
        resumed.status = "completed".into();
        store.checkpoint(&resumed, "## 交回
B6 做完了", "").unwrap();
        let jobs = store.finished_brain_jobs();
        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].resumed && jobs[0].run_id.as_deref() == Some(resumed.id.as_str()));
        assert_eq!(jobs[0].reply.as_deref(), Some("## 交回
B6 做完了"));

        store.connection.execute("UPDATE brain_jobs SET status='done',run_id=?1 WHERE id='job-b6'", [&resumed.id]).unwrap();
        assert!(store.finished_brain_jobs().is_empty(), "交回一次就够");
        drop(store);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn hub_team_tallies_how_each_model_did_by_kind_of_work() {
        let base = std::env::temp_dir().join(format!("tongxi-brain-record-{}", Uuid::new_v4()));
        let root = base.join("project");
        std::fs::create_dir_all(&root).unwrap();
        let mut store = Store::open(&base.join("data").join("hub.db")).unwrap();
        let room = store.create("项目群", "group", &["claude-win".into(), "codex-win".into()]).unwrap().id;
        let project = store.register_project("统计项目", root.to_str().unwrap()).unwrap().id;
        store.bind_project(&room, Some(&project)).unwrap();
        let brain = store.ensure_project_agent_chats(&room).unwrap().into_iter().find(|chat| chat.agent_id == "claude-win-brain").unwrap().conversation_id;
        let t = now();
        for (id, seat, status, error, minutes) in [
            ("a", "codex-win", "done", None, 10),
            ("b", "codex-win-2", "done", None, 20),
            ("c", "codex-win", "failed", Some("Codex 额度限制"), 0),
            ("d", "codex-win", "failed", Some("编译不过"), 0),
            ("e", "codex-win", "running", None, 0),
        ] {
            store
                .connection
                .execute(
                    "INSERT INTO brain_jobs(id,room,seat,chat,kind,content,model,effort,category,status,error,created_at,finished_at) VALUES(?1,?2,?3,'chat-x','task','做事','gpt-6.1-sol','high','实现',?4,?5,?6,?7)",
                    params![id, room, seat, status, error, t - minutes * 60000, t],
                )
                .unwrap();
        }
        let team = super::call(&store.connection, &brain, "hub_team", serde_json::Value::Null).unwrap();
        let record = team["track_record"].as_array().unwrap();
        assert_eq!(record.len(), 1, "两个席位同一成员同一模型合成一行，还在做的不算");
        let row = &record[0];
        assert_eq!((row["agent"].as_str(), row["category"].as_str()), (Some("codex-win"), Some("实现")));
        assert_eq!((row["jobs"].as_u64(), row["done"].as_u64(), row["failed"].as_u64(), row["quota_failed"].as_u64()), (Some(4), Some(2), Some(1), Some(1)));
        assert_eq!(row["avg_minutes"].as_f64(), Some(15.0));
        drop(store);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn member_reports_send_only_the_handback_section() {
        let reply = "先看日志。

改了三处。

## 交回
修好了 a.rs，测试通过。";
        let points = super::key_points(reply);
        assert!(points.starts_with("## 交回
修好了 a.rs"));
        assert!(!points.contains("先看日志"));
        assert_eq!(super::key_points("没按格式写"), "没按格式写");
    }

    #[test]
    fn overview_adds_up_the_usage_the_brain_reported() {
        let base = std::env::temp_dir().join(format!("tongxi-brain-usage-{}", Uuid::new_v4()));
        let root = base.join("project");
        std::fs::create_dir_all(&root).unwrap();
        let mut store = Store::open(&base.join("data").join("hub.db")).unwrap();
        let room = store.create("项目群", "group", &["claude-win".into()]).unwrap().id;
        let project = store.register_project("主脑项目", root.to_str().unwrap()).unwrap().id;
        store.bind_project(&room, Some(&project)).unwrap();
        let chat = store
            .ensure_project_agent_chats(&room)
            .unwrap()
            .into_iter()
            .find(|chat| chat.agent_id == "claude-win-brain")
            .unwrap()
            .conversation_id;
        for (read, context) in [(9000, 12000), (11000, 15000)] {
            let mut run = store.begin_agent_run(&chat, &Uuid::new_v4().to_string(), "做事", "claude-win").unwrap();
            run.status = "completed".into();
            store.checkpoint(&run, "好了", "").unwrap();
            let response = serde_json::json!({"stopReason": "end_turn",
                "usage": {"inputTokens": read + 1500, "cachedReadTokens": read, "cachedWriteTokens": 1000, "outputTokens": 300},
                "_meta": {"contextTokens": context}});
            store.record_usage(&run.id, &response);
        }
        let silent = store.begin_agent_run(&chat, &Uuid::new_v4().to_string(), "不报用量", "claude-win").unwrap();
        store.record_usage(&silent.id, &serde_json::json!({"stopReason": "end_turn"}));

        let usage = store.brain_overview(&room).unwrap().usage;
        let expected = super::UsageSum { turns: 2, input: 23000, cache_read: 20000, cache_write: 2000, output: 600 };
        assert_eq!(usage.brain_today, expected, "不报用量的回合不算");
        assert_eq!(usage.brain_total, expected);
        assert_eq!(usage.context, Some(15000), "上下文取最近一回合");
        let _ = std::fs::remove_dir_all(base);
    }
}
