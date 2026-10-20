use crate::{
    coordinator_policy::{Reference, Route, Source, GIT_REPAIR_ACTIVITY, MANAGER_ACTIVITY},
    project_store::{RoleChoice, Task, Workflow},
    store::{now, Store},
};
use rusqlite::{params, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;
type Result<T> = std::result::Result<T, String>;

struct PreparedRoute<'a> {
    route: &'a Route,
    model: &'a Option<String>,
    effort: &'a Option<String>,
    task_id: String,
    position: u32,
    dependencies: Vec<u32>,
    needs_update: bool,
    is_new: bool,
    rework: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskRoute {
    pub task_id: String,
    pub purpose: String,
    pub source_id: String,
    pub quote: String,
    pub context_ids: Vec<String>,
    /// 子代理验证：由修改者本人派自带的子代理验证。
    pub subagent: bool,
    /// 调度写给成员的任务说明。
    #[serde(default)]
    pub instructions: Option<String>,
    /// 调度为什么这样派（成员、模型、强度）。
    #[serde(default)]
    pub reason: Option<String>,
    /// 允许成员在本任务里用自带的子 agent。
    #[serde(default)]
    pub allow_subagents: bool,
    /// 调度要求合并前先通过验证。
    #[serde(default)]
    pub hold_merge: bool,
    /// 合并状态：held 已完成、等验证后合并；merged 已并入主线；None 还没完成或不在工作树里做。
    #[serde(default)]
    pub merge_state: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Coordination {
    pub coordinator: RoleChoice,
    pub status: String,
    pub routes: Vec<TaskRoute>,
    /// 调度的计划账本，每回合更新。
    pub ledger: Option<String>,
}

/// 调度倾向：省额度 / 平衡 / 质量优先。
pub const PREFERENCES: [&str; 3] = ["save", "balanced", "quality"];

/// 每种倾向下，一次协作最多新开几个会话。
pub fn temporary_limit(preference: &str) -> usize {
    match preference {
        "save" => 1,
        "quality" => 3,
        _ => 2,
    }
}
pub fn migrate(tx: &Transaction<'_>) -> Result<()> {
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
    crate::project_store::add_column(tx, "coordination_routes", "subagent", "INTEGER NOT NULL DEFAULT 0")?;
    crate::project_store::add_column(tx, "coordination_routes", "instructions", "TEXT")?;
    crate::project_store::add_column(tx, "coordination_routes", "reason", "TEXT")?;
    crate::project_store::add_column(tx, "coordination_routes", "allow_subagents", "INTEGER NOT NULL DEFAULT 0")?;
    crate::project_store::add_column(tx, "coordination_routes", "clear_context", "INTEGER NOT NULL DEFAULT 0")?;
    crate::project_store::add_column(tx, "coordination_routes", "category", "TEXT")?;
    crate::project_store::add_column(tx, "coordination_routes", "complexity", "TEXT")?;
    crate::project_store::add_column(tx, "coordination_routes", "hold_merge", "INTEGER NOT NULL DEFAULT 0")?;
    crate::project_store::add_column(tx, "coordination_routes", "merge_state", "TEXT")?;
    crate::project_store::add_column(tx, "coordination_runs", "ledger", "TEXT")
}
/// 同一原文最多再开几轮（不含第一次）。
const MAX_REWORK_ROUNDS: i64 = 2;

/// 子代理验证何时可派：只能由最近完成实现的成员派出；用户要求过子代理，
/// 或指定的验证成员已有至少 2 次未通过/失败时才放行。
fn check_subagent(
    job: &Workflow,
    routes: &[TaskRoute],
    sources: &[crate::coordinator_policy::Source],
    route: &Route,
) -> Result<()> {
    let purpose = |task: &Task| {
        routes
            .iter()
            .find(|entry| entry.task_id == task.id)
            .map(|entry| (entry.purpose.as_str(), entry.subagent))
    };
    let implementer = job
        .tasks
        .iter()
        .filter(|task| task.status == "completed" && purpose(task) == Some(("implementation", false)))
        .max_by_key(|task| task.position)
        .ok_or("还没有已完成的实现任务，不能派子代理验证")?;
    if implementer.agent_id != route.agent_id {
        return Err(format!(
            "子代理验证只能由改动源文件的成员（{}）派出",
            crate::store::agent_name(&implementer.agent_id)
        ));
    }
    let asked = sources.iter().any(|source| {
        source.kind == "user" && {
            let text = source.text.to_lowercase();
            text.contains("子代理") || text.contains("subagent") || text.contains("sub-agent")
        }
    });
    let stuck = job
        .tasks
        .iter()
        .filter(|task| purpose(task) == Some(("verification", false)))
        .filter(|task| task.status == "failed" || (task.status == "completed" && task.output.contains("验证结论：未通过")))
        .count();
    if !asked && stuck < 2 {
        return Err("只有用户要求子代理验证，或验证成员已有 2 次未通过/异常时，才能派子代理验证".into());
    }
    Ok(())
}

/// 是否为返修/复核轮次：context_ids 里引用了排在原任务之后完成的成员结果。
fn follow_up(job: &Workflow, task: &Task, route: &Route) -> bool {
    matches!(
        route.purpose.as_str(),
        "implementation" | "solution" | "verification"
    ) && route.context_ids.iter().any(|context| {
        job.tasks.iter().any(|other| {
            other.id == *context
                && other.id != task.id
                && other.position > task.position
                && other.status == "completed"
        })
    })
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
            .map_err(|_| "管家配置读取失败")?;
        value
            .map(|value| serde_json::from_str(&value).map_err(|_| "管家配置无效".into()))
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
            return Err("管家应从本项目的Codex、Hermes、DSH或Claude成员中选择".into());
        }
        self.connection.execute("INSERT INTO project_coordinator_settings(conversation_id,settings) VALUES(?1,?2) ON CONFLICT(conversation_id) DO UPDATE SET settings=excluded.settings",params![room,serde_json::to_string(choice).map_err(|_|"管家配置无效")?]).map_err(|_|"管家配置保存失败")?;
        Ok(())
    }
    pub fn coordination(&self, id: &str) -> Result<Option<Coordination>> {
        let row: Option<(String, String, Option<String>)> = self
            .connection
            .query_row(
                "SELECT coordinator,status,ledger FROM coordination_runs WHERE workflow_id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(|_| "调度状态读取失败")?;
        let Some((encoded, status, ledger)) = row else {
            return Ok(None);
        };
        let coordinator = serde_json::from_str(&encoded).map_err(|_| "本轮管家配置无效")?;
        Ok(Some(Coordination {
            coordinator,
            status,
            routes: self.coordination_routes(id)?,
            ledger,
        }))
    }
    /// 同一个群上一次协作留下的计划账本：项目分板块推进时，下一次协作接着它做。
    pub fn previous_ledger(&self, id: &str) -> Option<String> {
        self.connection
            .query_row(
                "SELECT r.ledger FROM coordination_runs r JOIN workflows w ON w.id=r.workflow_id \
                 WHERE w.conversation_id=(SELECT conversation_id FROM workflows WHERE id=?1) AND r.workflow_id<>?1 \
                 AND TRIM(COALESCE(r.ledger,''))<>'' ORDER BY w.rowid DESC LIMIT 1",
                [id],
                |row| row.get(0),
            )
            .ok()
    }
    /// 这次尝试要不要从零上下文开始：调度给任务设了 clear_context，且这是任务的第一次尝试
    /// （之后的重试续接第一次新开的会话，保留已做的进度）。
    pub fn clears_context(&self, attempt: &crate::project_store::Attempt) -> bool {
        let Some(task) = attempt.task_id.as_deref() else {
            return false;
        };
        let clear: bool = self
            .connection
            .query_row(
                "SELECT clear_context FROM coordination_routes WHERE task_id=?1 ORDER BY rowid DESC LIMIT 1",
                [task],
                |row| row.get(0),
            )
            .unwrap_or(false);
        clear
            && !self
                .connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM project_attempts WHERE task_id=?1 AND id<>?2)",
                    params![task, attempt.id],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap_or(true)
    }
    /// 分板块收尾：还没开始过的排队任务留到下次协作（记为跳过），返回留下的数量。
    pub fn defer_unstarted_tasks(&self, id: &str) -> Result<usize> {
        self.connection
            .execute(
                "UPDATE project_tasks SET status='skipped',error='留到下一次协作' WHERE workflow_id=?1 AND status='queued' \
                 AND NOT EXISTS(SELECT 1 FROM project_attempts a WHERE a.task_id=project_tasks.id)",
                [id],
            )
            .map_err(|_| "排队任务留存失败".into())
    }
    /// 协作结束后，这次干过活的成员会话位置清空：下次协作从零上下文开始，旧会话留在原生历史里。
    pub fn reset_worked_sessions(&mut self, id: &str) -> Result<usize> {
        let workflow = self.workflow(id)?;
        let mut worked = workflow
            .attempts
            .iter()
            .filter(|attempt| attempt.activity.as_deref() != Some(MANAGER_ACTIVITY))
            .map(|attempt| attempt.agent_id.clone())
            .collect::<Vec<_>>();
        worked.sort();
        worked.dedup();
        let mut reset = 0;
        for agent in worked {
            if self.bind_member_session(&workflow.conversation_id, &agent, None, None).is_ok() {
                reset += 1;
            }
        }
        Ok(reset)
    }
    pub fn set_coordination_ledger(&self, id: &str, ledger: &str) -> Result<()> {
        self.connection
            .execute(
                "UPDATE coordination_runs SET ledger=?1 WHERE workflow_id=?2",
                params![ledger.chars().take(3000).collect::<String>(), id],
            )
            .map_err(|_| "计划账本保存失败")?;
        Ok(())
    }
    /// 项目的调度倾向，没设过就是 balanced。
    pub fn project_preference(&self, room: &str) -> String {
        self.connection
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [format!("project_preference:{room}")],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
            .filter(|value| PREFERENCES.contains(&value.as_str()))
            .unwrap_or_else(|| "balanced".into())
    }
    pub fn set_project_preference(&mut self, room: &str, preference: &str) -> Result<()> {
        if !PREFERENCES.contains(&preference) {
            return Err("调度倾向无效".into());
        }
        if self.conversation_project(room)?.is_none() {
            return Err("请先绑定项目".into());
        }
        self.connection
            .execute(
                "INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![format!("project_preference:{room}"), preference],
            )
            .map_err(|_| "调度倾向保存失败")?;
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
    /// 这个会话最近一次任务结束时的上下文大小（token）；没记录就是 None。
    pub fn seat_context(&self, room: &str, seat: &str) -> Option<u64> {
        let usage: Option<String> = self
            .connection
            .query_row(
                "SELECT a.usage FROM project_attempts a JOIN workflows w ON w.id=a.workflow_id WHERE w.conversation_id=?1 AND a.agent_id=?2 AND a.usage IS NOT NULL AND COALESCE(a.activity,'')<>?3 ORDER BY a.rowid DESC LIMIT 1",
                params![room, seat, MANAGER_ACTIVITY],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten();
        usage
            .and_then(|text| serde_json::from_str::<crate::project_store::Usage>(&text).ok())
            .map(|usage| usage.context)
            .filter(|context| *context > 0)
    }
    /// 能力画像用的历史执行：since 之后所有协作里由调度派出的成员执行（含失败与验证）。
    pub fn capability_samples(&self, since: i64) -> Result<Vec<crate::capability::Sample>> {
        let mut query=self.connection.prepare("SELECT a.workflow_id,a.task_id,COALESCE(t.assigned_agent,a.agent_id),a.model,a.reasoning_effort,a.status,a.error,CASE WHEN r.purpose='verification' THEN substr(a.output,-400) ELSE '' END,a.usage,a.created_at,r.purpose,r.category,r.complexity,r.source_id,r.quote,r.context_ids FROM project_attempts a JOIN coordination_routes r ON r.task_id=a.task_id JOIN project_tasks t ON t.id=a.task_id WHERE a.created_at>=?1 AND a.status IN ('completed','failed','interrupted') ORDER BY a.created_at,a.rowid").map_err(|_|"能力画像读取失败")?;
        let rows = query
            .query_map([since], |r| {
                Ok(crate::capability::Sample {
                    workflow_id: r.get(0)?,
                    task_id: r.get(1)?,
                    agent_id: r.get(2)?,
                    model: r.get(3)?,
                    effort: r.get(4)?,
                    status: r.get(5)?,
                    error: r.get(6)?,
                    output: r.get(7)?,
                    usage: r.get::<_, Option<String>>(8)?.and_then(|text| serde_json::from_str(&text).ok()),
                    created_at: r.get(9)?,
                    purpose: r.get(10)?,
                    category: r.get(11)?,
                    complexity: r.get(12)?,
                    source_id: r.get(13)?,
                    quote: r.get(14)?,
                    context_ids: serde_json::from_str(&r.get::<_, String>(15)?).unwrap_or_default(),
                })
            })
            .map_err(|_| "能力画像读取失败")?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| "能力画像记录无效".into());
        rows
    }
    pub fn set_merge_state(&self, task: &str, state: Option<&str>) -> Result<()> {
        self.connection
            .execute("UPDATE coordination_routes SET merge_state=?1 WHERE task_id=?2", params![state, task])
            .map_err(|_| "合并状态保存失败")?;
        Ok(())
    }
    pub fn coordination_routes(&self, id: &str) -> Result<Vec<TaskRoute>> {
        let mut query=self.connection.prepare("SELECT task_id,purpose,source_id,quote,context_ids,subagent,instructions,reason,allow_subagents,hold_merge,merge_state FROM coordination_routes WHERE workflow_id=?1 ORDER BY rowid").map_err(|_|"任务路由读取失败")?;
        let rows = query
            .query_map([id], |r| {
                Ok(TaskRoute {
                    task_id: r.get(0)?,
                    purpose: r.get(1)?,
                    source_id: r.get(2)?,
                    quote: r.get(3)?,
                    context_ids: serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or_default(),
                    subagent: r.get(5)?,
                    instructions: r.get(6)?,
                    reason: r.get(7)?,
                    allow_subagents: r.get(8)?,
                    hold_merge: r.get(9)?,
                    merge_state: r.get(10)?,
                })
            })
            .map_err(|_| "任务路由读取失败")?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| "任务路由无效".into());
        rows
    }
    pub fn create_coordination(&mut self, id: &str, coordinator: &RoleChoice) -> Result<()> {
        let job = self.workflow(id)?;
        if !self
            .conversation(&job.conversation_id)?
            .members
            .contains(&coordinator.agent)
        {
            return Err("管家不属于本群".into());
        }
        self.connection.execute("INSERT INTO coordination_runs(workflow_id,coordinator,status) VALUES(?1,?2,'routing')",params![id,serde_json::to_string(coordinator).map_err(|_|"管家配置无效")?]).map_err(|_|"调度工作流创建失败")?;
        Ok(())
    }
    pub fn coordination_status(&self, id: &str, status: &str) -> Result<()> {
        self.connection
            .execute(
                "UPDATE coordination_runs SET status=?1 WHERE workflow_id=?2",
                params![status, id],
            )
            .map_err(|_| "调度状态保存失败")?;
        Ok(())
    }
    pub fn coordination_sources(&self, id: &str) -> Result<Vec<Source>> {
        let job = self.workflow(id)?;
        let mut sources = vec![Source {
            id: job.user_message_id,
            text: job.request,
            kind: "user".into(),
        }];
        sources.extend(
            self.project_user_updates(id)?
                .into_iter()
                .map(|update| Source {
                    id: update.id,
                    text: update.content,
                    kind: "user".into(),
                }),
        );
        for message in self
            .team_messages(&job.conversation_id)?
            .into_iter()
            .filter(|message| message.workflow_id.as_deref() != Some(id))
            .rev()
            .take(24)
        {
            if !sources.iter().any(|source| source.id == message.id) {
                sources.push(Source {
                    id: message.id,
                    kind: "context".into(),
                    text: message.content,
                });
            }
        }
        for task in job.tasks {
            if matches!(task.status.as_str(), "completed" | "failed" | "interrupted") {
                sources.push(Source {
                    id: task.id,
                    kind: "result".into(),
                    text: if task.output.is_empty() {
                        task.error
                            .unwrap_or_else(|| "此任务已中断，已有文件保留".into())
                    } else {
                        task.output
                    },
                });
            }
        }
        Ok(sources)
    }
    pub fn enqueue_routes(
        &mut self,
        id: &str,
        routes: &[(Route, Option<String>, Option<String>)],
    ) -> Result<Vec<String>> {
        let job = self.workflow(id)?;
        if self.coordination(id)?.is_none() || !crate::project_store::live(&job.status) {
            return Err("调度已停止".into());
        }
        let sources = self.coordination_sources(id)?;
        let members = self.conversation(&job.conversation_id)?.members;
        // 额外席位不是群成员，但底层成员在群里、席位确实开过，就能接任务。
        let seats = self.project_seats(&job.conversation_id)?;
        let aliases = self.member_aliases(&job.conversation_id);
        let mut keys: HashMap<String, (String, u32)> = HashMap::new();
        let mut prepared: Vec<PreparedRoute<'_>> = Vec::new();
        let mut next_position = job.tasks.len() as u32;
        for (route, model, effort) in routes {
            crate::coordinator_policy::check_assignment_with(route, &sources, &aliases)?;
            let base = crate::project_store::base_agent(&route.agent_id);
            let seat = route.agent_id != base
                && members.iter().any(|member| member == base)
                && seats.iter().any(|(seat, _)| *seat == route.agent_id);
            if !members.contains(&route.agent_id) && !seat {
                return Err("任务成员不在项目群里".into());
            }
            if keys.contains_key(&route.key) {
                return Err("路由key重复".into());
            }
            if route.subagent {
                check_subagent(&job, &self.coordination_routes(id)?, &sources, route)?;
            }
            let mut rework = false;
            // 依赖里最靠后的任务位置：依赖本批新派或更新的任务，说明这是新一轮工作而不是重复派发。
            let newest_dependency = route
                .depends_on
                .iter()
                .filter_map(|dependency| {
                    keys.get(dependency).map(|(_, at)| *at).or_else(|| {
                        job.tasks
                            .iter()
                            .find(|task| task.id == *dependency)
                            .map(|task| task.position)
                    })
                })
                .max();
            let existing = if let Some(task_id) = &route.task_id {
                Some(
                    job.tasks
                        .iter()
                        .find(|task| task.id == *task_id)
                        .ok_or("恢复任务不存在")?
                        .clone(),
                )
            } else {
                // 子代理验证与原验证成员的任务分开去重：前者不是后者的重复派发。
                let known:Option<String>=self.connection.query_row("SELECT task_id FROM coordination_routes WHERE workflow_id=?1 AND purpose=?2 AND source_id=?3 AND quote=?4 AND subagent=?5 ORDER BY rowid DESC LIMIT 1",params![id,route.purpose,route.source_id,route.quote,route.subagent],|r|r.get(0)).optional().map_err(|_|"路由去重失败")?;
                let latest =
                    known.and_then(|id| job.tasks.iter().find(|task| task.id == id).cloned());
                // 用户的一句话（如「验证让克小姐来做」）会被很多项工作引用：调度写了和原任务不同的任务名，就是另一项工作。
                let named = route.title.as_deref().map(str::trim).filter(|title| !title.is_empty());
                let distinct = |task: &crate::project_store::Task| {
                    named.is_some_and(|title| !task.title.ends_with(&format!(" · {title}")))
                };
                // 同一原文的已完成任务：带上比它更新的成员结果（例如验证发现的问题）时开新一轮返修/复核，
                // 否则仍视为重复派发，返回原任务。
                match latest {
                    Some(task) if distinct(&task) => {
                        rework = task.status == "completed" && follow_up(&job, &task, route);
                        None
                    }
                    Some(task)
                        if task.status == "completed"
                            && (follow_up(&job, &task, route)
                                || newest_dependency.is_some_and(|at| at > task.position)) =>
                    {
                        let rounds: i64 = self.connection.query_row("SELECT COUNT(*) FROM coordination_routes WHERE workflow_id=?1 AND purpose=?2 AND source_id=?3 AND quote=?4",params![id,route.purpose,route.source_id,route.quote],|r|r.get(0)).map_err(|_|"路由去重失败")?;
                        // 用户回答管家的提问（例如同意继续返修）后，从这次回答起再放行一批轮次：
                        // 只数回答之后才开始（或尚未开始）的同一原文任务。
                        let answered_at: Option<i64> = self.connection.query_row("SELECT MAX(answered_at) FROM project_questions WHERE workflow_id=?1 AND status='answered'",[id],|r|r.get(0)).map_err(|_|"路由去重失败")?;
                        let since: i64 = match answered_at {
                            Some(at) => self.connection.query_row("SELECT COUNT(*) FROM coordination_routes r WHERE r.workflow_id=?1 AND r.purpose=?2 AND r.source_id=?3 AND r.quote=?4 AND COALESCE((SELECT MIN(a.created_at) FROM project_attempts a WHERE a.task_id=r.task_id),?5+1)>?5",params![id,route.purpose,route.source_id,route.quote,at],|r|r.get(0)).map_err(|_|"路由去重失败")?,
                            None => rounds,
                        };
                        if rounds > MAX_REWORK_ROUNDS && since >= MAX_REWORK_ROUNDS {
                            return Err(format!("这项工作已返修 {} 轮仍需修改，请用 ask_user 询问用户如何处理", rounds - 1));
                        }
                        rework = true;
                        None
                    }
                    other => other,
                }
            };
            if let Some(task) = &existing {
                if route.task_id.is_some()
                    && !matches!(task.status.as_str(), "failed" | "interrupted" | "queued")
                {
                    return Err("只能恢复未完成任务，已完成工作不重跑".into());
                }
                if route.task_id.is_none()
                    && matches!(task.status.as_str(), "failed" | "interrupted")
                {
                    return Err("此路由已有未完成任务，请用task_id恢复而非重复创建".into());
                }
            }
            let task_id = existing
                .as_ref()
                .map(|task| task.id.clone())
                .unwrap_or_else(|| Uuid::new_v4().to_string());
            // 尚未开始的排队任务改为依赖更靠后的新任务时（例如收尾要等新一轮实现与验证），
            // 挪到队尾；已有任务依赖它时不挪，避免打乱别人的前置关系。
            let relocate = existing.as_ref().is_some_and(|task| {
                route.task_id.is_none()
                    && task.status == "queued"
                    && newest_dependency.is_some_and(|at| at >= task.position)
                    && !job
                        .tasks
                        .iter()
                        .any(|other| other.depends_on.contains(&task.position))
            });
            let position = match &existing {
                Some(task) if !relocate => task.position,
                _ => {
                    next_position += 1;
                    next_position - 1
                }
            };
            let mut deps = Vec::new();
            for dependency in &route.depends_on {
                let (owner, at) = keys
                    .get(dependency)
                    .cloned()
                    .or_else(|| {
                        job.tasks
                            .iter()
                            .find(|task| task.id == *dependency)
                            .map(|task| (task.id.clone(), task.position))
                    })
                    .ok_or("依赖任务不存在")?;
                if owner == task_id || at >= position {
                    return Err("依赖必须指向已声明的前置任务".into());
                }
                if !deps.contains(&at) {
                    deps.push(at);
                }
            }
            if route.task_id.is_none() && existing.as_ref().is_some_and(|task| task.status == "completed") {
                return Err(format!(
                    "{} 和已完成的任务（{}）是同一项工作，不能重复派发；要做新的一项就写不同的 title，要返修或复核就在 context_ids 引用新结果",
                    route.key,
                    existing.as_ref().map(|task| task.title.as_str()).unwrap_or_default()
                ));
            }
            keys.insert(route.key.clone(), (task_id.clone(), position));
            let is_new = existing.is_none();
            // 撤回过的任务按同一原文再派：重新打开，换上新的说明。
            let reopened = existing.as_ref().is_some_and(|task| task.status == "skipped");
            let needs_update = is_new || relocate || reopened || route.task_id.is_some();
            prepared.push(PreparedRoute {
                route,
                model,
                effort,
                task_id,
                position,
                dependencies: deps,
                needs_update,
                is_new,
                rework,
            });
        }
        let tx = self
            .connection
            .transaction()
            .map_err(|_| "任务分派保存失败")?;
        for PreparedRoute {
            route,
            model,
            effort,
            task_id,
            position,
            dependencies: deps,
            needs_update,
            is_new,
            rework,
        } in &prepared
        {
            if !needs_update {
                continue;
            }
            let label = match (*rework, route.purpose.as_str()) {
                _ if route.subagent => "子代理验证",
                (true, "verification") => "复核",
                (true, _) => "返修",
                _ => purpose_label(&route.purpose),
            };
            let named = route.title.as_deref().map(str::trim).filter(|title| !title.is_empty());
            let title = format!(
                "{} · {}",
                label,
                named.unwrap_or(&route.quote).chars().take(70).collect::<String>()
            );
            let stored_agent = if crate::project_store::base_agent(&route.agent_id) == "hermes-win" {
                "dsh-win"
            } else {
                route.agent_id.as_str()
            };
            let category = route.category.as_deref().filter(|value| crate::capability::CATEGORIES.contains(value));
            let complexity = route.complexity.as_deref().filter(|value| crate::capability::COMPLEXITIES.contains(value));
            if *is_new {
                tx.execute("INSERT INTO project_tasks(id,workflow_id,position,title,agent_id,assigned_agent,instructions,files,depends_on,status,model,effort) VALUES(?1,?2,?3,?4,?5,?6,?7,'[]',?8,'queued',?9,?10)",params![task_id,id,position,title,stored_agent,route.agent_id,route.quote,serde_json::to_string(deps).unwrap(),model,effort]).map_err(|_|"任务保存失败")?;
                tx.execute("INSERT INTO coordination_routes(task_id,workflow_id,purpose,source_id,quote,context_ids,subagent,instructions,reason,allow_subagents,clear_context,category,complexity,hold_merge) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",params![task_id,id,route.purpose,route.source_id,route.quote,serde_json::to_string(&route.context_ids).unwrap(),route.subagent,route.instructions,route.reason,route.allow_subagents,route.clear_context,category,complexity,route.hold_merge]).map_err(|_|"任务原文引用保存失败")?;
            } else {
                tx.execute("UPDATE project_tasks SET title=?1,assigned_agent=?2,instructions=?3,depends_on=?4,status='queued',model=?5,effort=?6,error=NULL,position=?9 WHERE id=?7 AND workflow_id=?8",params![title,route.agent_id,route.quote,serde_json::to_string(deps).unwrap(),model,effort,task_id,id,position]).map_err(|_|"任务恢复失败")?;
                tx.execute("UPDATE coordination_routes SET purpose=?1,source_id=?2,quote=?3,context_ids=?4,subagent=?5,instructions=COALESCE(?7,instructions),reason=COALESCE(?8,reason),allow_subagents=?9,clear_context=?10,category=COALESCE(?11,category),complexity=COALESCE(?12,complexity),hold_merge=?13 WHERE task_id=?6",params![route.purpose,route.source_id,route.quote,serde_json::to_string(&route.context_ids).unwrap(),route.subagent,task_id,route.instructions,route.reason,route.allow_subagents,route.clear_context,category,complexity,route.hold_merge]).map_err(|_|"任务引用恢复失败")?;
            }
        }
        tx.execute(
            "UPDATE workflows SET status='running',updated_at=?1 WHERE id=?2",
            params![now(), id],
        )
        .map_err(|_| "协作状态保存失败")?;
        tx.commit().map_err(|_| "路由保存失败")?;
        Ok(prepared.into_iter().map(|entry| entry.task_id).collect())
    }
    pub fn withdraw_coordination_tasks(
        &mut self,
        id: &str,
        withdrawals: &[crate::coordinator_policy::Withdrawal],
        replacements: &[(String, String)],
    ) -> Result<()> {
        let job = self.workflow(id)?;
        if self.coordination(id)?.is_none() || !crate::project_store::live(&job.status) {
            return Err("调度已停止".into());
        }
        let sources = self.coordination_sources(id)?;
        for withdrawal in withdrawals {
            let task = job
                .tasks
                .iter()
                .find(|task| task.id == withdrawal.task_id)
                .ok_or("取消任务不存在")?;
            if !matches!(task.status.as_str(), "queued" | "failed" | "interrupted") {
                return Err("已完成或正在运行的任务不能由管家撤回".into());
            }
            if withdrawal.source_id == job.user_message_id {
                return Err("取消旧任务必须引用用户后续纠正消息".into());
            }
            crate::coordinator_policy::source(
                &sources,
                &Reference {
                    source_id: withdrawal.source_id.clone(),
                    quote: withdrawal.quote.clone(),
                },
                true,
            )?;
            let text = withdrawal.quote.to_lowercase();
            // 同一回合按同一原文重新派发：这是按用户后续消息修订任务（比如新给了授权），不是丢掉用户要的活。
            let revised = self
                .connection
                .query_row(
                    "SELECT source_id,quote FROM coordination_routes WHERE task_id=?1",
                    [&task.id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .is_ok_and(|original| replacements.contains(&original));
            if !revised && ![
                "取消",
                "不要",
                "不做",
                "改成",
                "改为",
                "改方向",
                "调整方向",
                "纠正",
                "cancel",
                "instead",
                "change direction",
            ]
            .iter()
            .any(|term| text.contains(term))
            {
                return Err("没有用户取消或纠正方向的原文授权".into());
            }
            let named = [
                ("codex", "codex-win"),
                ("hermes", "hermes-win"),
                ("dsh", "dsh-win"),
                ("claude", "claude-win"),
            ]
            .into_iter()
            .filter(|(label, _)| text.contains(label))
            .map(|(_, id)| id)
            .collect::<Vec<_>>();
            // 修订是按同一原文重派同一项工作，不受「取消别人的活」那两条范围限制。
            if !revised && !named.is_empty() && !named.contains(&task.agent_id.as_str()) {
                return Err("取消原文点名了另一成员，不能撤回此任务".into());
            }
            let chinese = ["一", "二", "三", "四", "五", "六", "七", "八", "九", "十"];
            let numbered = job
                .tasks
                .iter()
                .filter(|candidate| {
                    let number = candidate.position as usize + 1;
                    text.contains(&format!("第{number}项"))
                        || chinese
                            .get(number - 1)
                            .is_some_and(|label| text.contains(&format!("第{label}项")))
                })
                .map(|candidate| candidate.id.as_str())
                .collect::<Vec<_>>();
            if !revised && !numbered.is_empty() && !numbered.contains(&task.id.as_str()) {
                return Err("取消原文明示其他任务编号，不能扩大撤回范围".into());
            }
        }
        let tx = self.connection.transaction().map_err(|_| "任务撤回失败")?;
        for withdrawal in withdrawals {
            tx.execute(
                "UPDATE project_tasks SET status='skipped',error=?1 WHERE id=?2 AND workflow_id=?3",
                params![
                    format!("按用户纠正取消：{}", withdrawal.quote),
                    withdrawal.task_id,
                    id
                ],
            )
            .map_err(|_| "任务撤回失败")?;
        }
        tx.commit().map_err(|_| "任务撤回失败")?;
        Ok(())
    }
    pub fn begin_coordinated_attempt(
        &mut self,
        id: &str,
        task: Option<&Task>,
        agent: &str,
        model: Option<String>,
        effort: Option<String>,
    ) -> Result<crate::project_store::Attempt> {
        let job = self.workflow(id)?;
        let manager = self.coordination(id)?.ok_or("没有调度工作流")?;
        if !crate::project_store::live(&job.status) || job.status == "cancelling" {
            return Err("协作已停止".into());
        }
        // 维护与私聊运行按底层成员算；项目任务按席位算，同一成员的不同席位可以并行。
        let base = crate::project_store::base_agent(agent);
        self.guard_service_maintenance(base)?;
        let activity = if let Some(task) = task {
            if task.workflow_id != id
                || task.agent_id != agent
                || task.status != "queued"
                || task.depends_on.iter().any(|at| {
                    job.tasks
                        .get(*at as usize)
                        .is_none_or(|task| task.status != "completed")
                })
            {
                return Err("任务成员或依赖尚未就绪".into());
            }
            let busy:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM runs WHERE agent_id=?1 AND status IN ('starting','running','cancelling')) OR EXISTS(SELECT 1 FROM project_attempts WHERE agent_id=?2 AND COALESCE(activity,'')!='coordinate' AND status IN ('starting','running','cancelling'))",params![base,agent],|r|r.get(0)).map_err(|_|"成员状态不可用")?;
            if busy {
                return Err("成员仍有项目任务或私聊运行".into());
            }
            let routes = self.coordination_routes(id)?;
            let purpose = routes
                .iter()
                .find(|route| route.task_id == task.id)
                .ok_or("任务缺少原文引用")?
                .purpose
                .clone();
            format!("task:{purpose}")
        } else {
            if agent != manager.coordinator.agent {
                return Err("管家身份不匹配".into());
            }
            let busy:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM project_attempts WHERE workflow_id=?1 AND activity='coordinate' AND status IN ('starting','running','cancelling'))",[id],|r|r.get(0)).map_err(|_|"管家状态不可用")?;
            if busy {
                return Err("管家仍在处理上一回合".into());
            }
            MANAGER_ACTIVITY.into()
        };
        let stage = if activity == MANAGER_ACTIVITY
            || activity == "task:solution"
            || activity == "task:reply"
        {
            "plan"
        } else if activity == "task:verification" {
            "review"
        } else {
            "implement"
        };
        let attempt = Uuid::new_v4().to_string();
        let tx = self.connection.transaction().map_err(|_| "任务启动失败")?;
        tx.execute("INSERT INTO project_attempts(id,workflow_id,task_id,agent_id,stage,activity,status,model,reasoning_effort,created_at) VALUES(?1,?2,?3,?4,?5,?6,'starting',?7,?8,?9)",params![attempt,id,task.map(|task|&task.id),agent,stage,activity,model,effort,now()]).map_err(|_|"原生任务记录保存失败")?;
        if let Some(task) = task {
            tx.execute(
                "UPDATE project_tasks SET status='running' WHERE id=?1",
                [&task.id],
            )
            .map_err(|_| "任务状态保存失败")?;
        }
        tx.execute(
            "UPDATE workflows SET status='running',updated_at=?1 WHERE id=?2",
            params![now(), id],
        )
        .map_err(|_| "协作运行状态保存失败")?;
        tx.commit().map_err(|_| "任务启动失败")?;
        self.attempt(&attempt)
    }
    /// 管家所在成员用自己的项目会话处理原仓库的合并/变基中途状态；成员忙时返回 `Ok(None)` 由调用方等待。
    pub fn begin_git_repair_attempt(
        &mut self,
        id: &str,
        agent: &str,
        model: Option<String>,
        effort: Option<String>,
    ) -> Result<Option<crate::project_store::Attempt>> {
        let job = self.workflow(id)?;
        if !crate::project_store::live(&job.status) || job.status == "cancelling" {
            return Err("协作已停止".into());
        }
        self.guard_service_maintenance(agent)?;
        let busy:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM runs WHERE agent_id=?1 AND status IN ('starting','running','cancelling')) OR EXISTS(SELECT 1 FROM project_attempts WHERE agent_id=?1 AND COALESCE(activity,'')!='coordinate' AND status IN ('starting','running','cancelling'))",[agent],|r|r.get(0)).map_err(|_|"成员状态不可用")?;
        if busy {
            return Ok(None);
        }
        let attempt = Uuid::new_v4().to_string();
        self.connection.execute("INSERT INTO project_attempts(id,workflow_id,task_id,agent_id,stage,activity,status,model,reasoning_effort,created_at) VALUES(?1,?2,NULL,?3,'implement',?4,'starting',?5,?6,?7)",params![attempt,id,agent,GIT_REPAIR_ACTIVITY,model,effort,now()]).map_err(|_|"冲突处理记录保存失败")?;
        self.attempt(&attempt).map(Some)
    }
    pub fn resume_coordination(&mut self, id: &str) -> Result<Workflow> {
        let job = self.workflow(id)?;
        if self.coordination(id)?.is_none()
            || !matches!(job.status.as_str(), "interrupted" | "failed")
        {
            return Err("只有暂停的调度协作可以继续".into());
        }
        if self.attempts(id)?.iter().any(|attempt| {
            matches!(
                attempt.status.as_str(),
                "starting" | "running" | "cancelling"
            )
        }) {
            return Err("原生任务尚未停止".into());
        }
        // 回到排队：调度循环会像新协作一样重新占用项目目录（失败时占用已释放），再接着原任务走。
        // 用户点“继续协作”就是恢复授权：被中断或失败的任务直接重新排队（保留原工作树与分支），
        // 不再交给管家二次决定——否则管家按规则选择 wait，协作会原地再停一次。
        let tx = self.connection.transaction().map_err(|_| "协作恢复失败")?;
        tx.execute(
            "UPDATE project_tasks SET status='queued',error=NULL WHERE workflow_id=?1 AND status IN ('interrupted','failed')",
            [id],
        )
        .map_err(|_| "任务恢复失败")?;
        tx.execute(
            "UPDATE workflows SET status='queued',error=NULL,updated_at=?1 WHERE id=?2",
            params![now(), id],
        )
        .map_err(|_| "协作恢复失败")?;
        tx.commit().map_err(|_| "协作恢复失败")?;
        self.coordination_status(id, "routing")?;
        self.workflow(id)
    }
    pub fn finish_coordinated_task(
        &mut self,
        id: &str,
        attempt: &crate::project_store::Attempt,
    ) -> Result<()> {
        let status = if attempt.status == "completed" {
            "completed"
        } else if attempt.status == "interrupted" {
            "interrupted"
        } else {
            "failed"
        };
        let tx = self
            .connection
            .transaction()
            .map_err(|_| "任务结果保存失败")?;
        tx.execute(
            "UPDATE project_tasks SET status=?1,output=?2,error=?3 WHERE id=?4",
            params![status, attempt.output, attempt.error, id],
        )
        .map_err(|_| "任务结果保存失败")?;
        tx.commit().map_err(|_| "任务结果保存失败")?;
        Ok(())
    }
    pub fn publish_coordinated_reply(
        &self,
        workflow_id: &str,
        attempt: &crate::project_store::Attempt,
        content: &str,
    ) -> Result<()> {
        if content.trim().is_empty() || attempt.activity.as_deref() == Some(MANAGER_ACTIVITY) {
            return Ok(());
        }
        let workflow = self.workflow(workflow_id)?;
        self.connection.execute("INSERT OR IGNORE INTO project_team_messages(id,project_id,conversation_id,workflow_id,attempt_id,sender_agent,message_kind,content,created_at) VALUES(?1,?2,?3,?4,?5,?6,'agent',?7,?8)",params![format!("task-result:{}",attempt.task_id.as_deref().unwrap_or(&attempt.id)),workflow.project_id,workflow.conversation_id,workflow_id,attempt.id,attempt.agent_id,content,now()]).map_err(|_|"成员回复发布失败")?;
        Ok(())
    }
    #[cfg(test)]
    pub fn manager_session(&self, id: &str) -> Result<Option<String>> {
        self.connection
            .query_row(
                "SELECT native_session_id FROM coordinator_sessions WHERE workflow_id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()
            .map(|row: Option<Option<String>>| row.flatten())
            .map_err(|_| "管家会话读取失败".into())
    }
    pub fn bind_manager_session(
        &self,
        id: &str,
        agent: &str,
        session: &str,
        cwd: &std::path::Path,
    ) -> Result<()> {
        self.connection.execute("INSERT INTO coordinator_sessions(workflow_id,agent_id,native_session_id,cwd) VALUES(?1,?2,?3,?4) ON CONFLICT(workflow_id) DO UPDATE SET native_session_id=excluded.native_session_id,cwd=excluded.cwd",params![id,agent,session,cwd.to_string_lossy()]).map_err(|_|"管家会话绑定失败")?;
        Ok(())
    }
    pub fn finished_coordination_summary(
        &self,
        id: &str,
        result: Option<&Reference>,
    ) -> Result<String> {
        let job = self.workflow(id)?;
        if job.tasks.is_empty()
            || job
                .tasks
                .iter()
                .any(|task| !matches!(task.status.as_str(), "completed" | "skipped"))
        {
            return Err("仍有未完成任务，不能宣称项目完成".into());
        }
        if let Some(reference) = result {
            let sources = self.coordination_sources(id)?;
            let source = crate::coordinator_policy::source(&sources, reference, false)?;
            if source.kind != "result"
                || !job
                    .tasks
                    .iter()
                    .any(|task| task.id == reference.source_id && task.status == "completed")
            {
                return Err("最终结果必须引用成员实际回复".into());
            }
            Ok(reference.quote.clone())
        } else {
            Ok(format!(
                "本轮协作已完成，共完成{}项工作。",
                job.tasks
                    .iter()
                    .filter(|task| task.status == "completed")
                    .count()
            ))
        }
    }

    pub fn pause_coordination_wait(&mut self, id: &str) -> Result<bool> {
        let job = self.workflow(id)?;
        if self.coordination(id)?.is_none() || !crate::project_store::live(&job.status) {
            return Err("调度已停止".into());
        }
        if job.tasks.iter().any(|task| {
            task.status == "queued"
                && task.depends_on.iter().all(|at| {
                    job.tasks
                        .get(*at as usize)
                        .is_some_and(|previous| previous.status == "completed")
                })
        }) {
            // Continue independent ready work even while another task awaits recovery.
            return Ok(false);
        }
        if !job
            .tasks
            .iter()
            .any(|task| matches!(task.status.as_str(), "failed" | "interrupted"))
        {
            return Err("当前没有失败或被中断的任务需要等待恢复。请按用户原文继续分派尚未完成的工作；目标已完成时finish，需要用户信息时ask_user，不能让用户额外点击继续。".into());
        }
        self.coordination_status(id, "paused")?;
        self.finish_workflow(
            id,
            "interrupted",
            "",
            Some("未完成任务正在等待恢复，已完成结果会保留"),
        )?;
        Ok(true)
    }
}
pub fn purpose_label(purpose: &str) -> &str {
    match purpose {
        "solution" => "方案",
        "implementation" => "实现",
        "verification" => "验证",
        "closeout" => "收尾",
        _ => "回复",
    }
}
