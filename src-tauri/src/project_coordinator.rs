//! Persistent agent-led coordination. No fixed plan/implement/review pipeline.
use crate::{
    coordinator_policy::{self, Decision},
    project_store::{RoleChoice, Task, Workflow},
    projects::Runtime,
};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
type Result<T> = std::result::Result<T, String>;

/// 最新一次完成的验证任务若结论为“未通过”，返回它的任务 ID。
pub(crate) fn failed_verification(
    tasks: &[Task],
    routes: &[crate::coordinator_store::TaskRoute],
) -> Option<String> {
    tasks
        .iter()
        .filter(|task| {
            task.status == "completed"
                && routes
                    .iter()
                    .any(|route| route.task_id == task.id && route.purpose == "verification")
        })
        .max_by_key(|task| task.position)
        .filter(|task| {
            let output = task.output.replace([' ', '　', '*'], "");
            output.contains("验证结论：未通过") || output.contains("验证结论:未通过")
        })
        .map(|task| task.id.clone())
}

fn clip(text: &str, limit: usize) -> String {
    let mut chars = text.chars();
    let head = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        format!("{head}……")
    } else {
        head
    }
}

/// 调度看到的来源：用户原话全文；成员结果只给交付摘要；群里的旁支消息截短。
fn compact_sources(sources: &[coordinator_policy::Source]) -> Vec<Value> {
    sources
        .iter()
        .map(|source| {
            let text = match source.kind.as_str() {
                "result" => coordinator_policy::delivery_summary(&source.text),
                // 上一次协作的成员交付也在旁支消息里：开头多是过程，遗留问题写在末尾的交付摘要里，只截开头会丢。
                "context" => match source.text.rfind(coordinator_policy::DELIVERY_MARK) {
                    Some(at) => clip(&source.text[at..], 900),
                    None => clip(&source.text, 600),
                },
                _ => source.text.clone(),
            };
            json!({"id":source.id,"kind":source.kind,"text":text})
        })
        .collect()
}

/// 一项任务累计的 token（各次尝试相加）；有尝试用量未知（没报或只有上下文占用）时为 null，不当成 0。
fn task_tokens(workflow: &Workflow, task: &str) -> Option<u64> {
    workflow
        .attempts
        .iter()
        .filter(|attempt| attempt.task_id.as_deref() == Some(task) && attempt.status != "starting")
        .map(|attempt| attempt.usage.as_ref().and_then(|usage| usage.spent()))
        .sum()
}

/// 任务进度：不带成员输出全文（结果在 sources 里只给摘要）。
fn compact_tasks(workflow: &Workflow) -> Vec<Value> {
    workflow
        .tasks
        .iter()
        .map(|task| {
            json!({"task_id":task.id,"position":task.position,"title":task.title,"agent_id":task.agent_id,"status":task.status,"model":task.model,"effort":task.effort,"depends_on":task.depends_on,"error":task.error,"tokens":task_tokens(workflow,&task.id)})
        })
        .collect()
}

/// 这项任务已完成、在等验证通过后合并。
fn held(routes: &[crate::coordinator_store::TaskRoute], task: &str) -> bool {
    routes
        .iter()
        .any(|route| route.task_id == task && route.merge_state.as_deref() == Some("held"))
}

fn compact_routes(routes: &[crate::coordinator_store::TaskRoute]) -> Vec<Value> {
    routes
        .iter()
        .map(|route| {
            json!({"task_id":route.task_id,"purpose":route.purpose,"source_id":route.source_id,"quote":clip(&route.quote,200),"context_ids":route.context_ids,"subagent":route.subagent,"merge":route.merge_state})
        })
        .collect()
}

/// 告诉调度哪项任务撞上了限额、多久后重置，让它选改派还是等。
fn quota_offer(task: &crate::project_store::Task) -> String {
    let error = task.error.as_deref().unwrap_or_default();
    let (offset, now) = crate::quota_reset::local_offset();
    let when = match crate::quota_reset::reset_at(error, now, offset) {
        Some(at) => format!(
            "预计 {} 重置（约 {:.1} 小时后）",
            crate::quota_reset::local_label(at, offset),
            (at - now) as f64 / 3600.0
        ),
        None => "没读到重置时间".into(),
    };
    format!(
        "任务「{}」（task_id {}）的成员 {} 撞上限额，{when}。离重置还早、别的成员能做好时，用同一 task_id 改派给额度充足的成员（agent_id 换人，在原工作树上接着做，instructions 写清前情和已做到哪）；否则返回 wait，同席等重置后自动重试。",
        task.title, task.id, task.agent_id
    )
}

/// 把首次执行、改派、返修、验证归到同一个逻辑任务：路由的 context_ids 显式引用了另一项任务的结果，
/// 并且两者锚定同一段用户原文，或其中一方是验证（验证引用被验的实现、返修引用验证结论）。
/// 不凭标题相似合并。返回 task_id → 逻辑任务 id（组内位置最靠前的那项任务）。
pub(crate) fn logical_tasks(
    workflow: &Workflow,
    routes: &[crate::coordinator_store::TaskRoute],
) -> std::collections::HashMap<String, String> {
    let mut tasks: Vec<&crate::project_store::Task> = workflow.tasks.iter().collect();
    tasks.sort_by_key(|task| task.position);
    let linked: Vec<&crate::coordinator_store::TaskRoute> = tasks
        .iter()
        .filter_map(|task| routes.iter().find(|route| route.task_id == task.id))
        .collect();
    let mut groups = crate::capability::link_tasks(linked.iter().map(|route| {
        (
            route.task_id.as_str(),
            route.purpose.as_str(),
            route.source_id.as_str(),
            route.quote.as_str(),
            route.context_ids.as_slice(),
        )
    }));
    for task in tasks {
        groups.entry(task.id.clone()).or_insert_with(|| task.id.clone());
    }
    groups
}

/// 逻辑任务的完整消耗：各环节（按任务的 purpose）分开列，再给合计；有未知用量时合计为 null。
/// 调度自己的消耗只在 usage_overview 里计一次，不摊进这里。
fn logical_usage(workflow: &Workflow, routes: &[crate::coordinator_store::TaskRoute]) -> Vec<Value> {
    let groups = logical_tasks(workflow, routes);
    let mut roots: Vec<&crate::project_store::Task> = workflow
        .tasks
        .iter()
        .filter(|task| groups.get(&task.id) == Some(&task.id))
        .collect();
    roots.sort_by_key(|task| task.position);
    roots
        .into_iter()
        .filter_map(|root| {
            let members: Vec<&String> = groups
                .iter()
                .filter(|(_, group)| *group == &root.id)
                .map(|(task, _)| task)
                .collect();
            if members.len() < 2 {
                return None;
            }
            let mut stages = serde_json::Map::new();
            let (mut total, mut attempts, mut failed) = (Some(0u64), 0, 0);
            for attempt in workflow.attempts.iter().filter(|attempt| {
                attempt.status != "starting"
                    && attempt.task_id.as_ref().is_some_and(|task| members.contains(&task))
            }) {
                attempts += 1;
                failed += usize::from(matches!(attempt.status.as_str(), "failed" | "interrupted"));
                let spent = attempt.usage.as_ref().and_then(|usage| usage.spent());
                total = total.zip(spent).map(|(a, b)| a + b);
                let purpose = attempt
                    .task_id
                    .as_deref()
                    .and_then(|task| routes.iter().find(|route| route.task_id == task))
                    .map_or("other", |route| route.purpose.as_str());
                let entry = stages.entry(purpose).or_insert(json!(0));
                *entry = match (entry.as_u64(), spent) {
                    (Some(a), Some(b)) => json!(a + b),
                    _ => Value::Null,
                };
            }
            let mut ids: Vec<&String> = members;
            ids.sort_by_key(|id| workflow.tasks.iter().find(|task| &&task.id == id).map(|task| task.position));
            Some(json!({"logical_task":root.id,"title":root.title,"task_ids":ids,"attempts":attempts,"failed_attempts":failed,"tokens":total,"by_purpose":stages}))
        })
        .collect()
}

/// 资源池：一份订阅或一个计费账户。Claude、Codex 走各自的订阅；DSH、Hermes 按模型所在线路（提供商配置）分池，
/// 两者各配各的 Key，同名线路也不合并。拿不到的数据不填，不当 0。
pub(crate) fn pool_of(base: &str, provider: Option<&str>) -> String {
    match (base, provider) {
        ("claude-win", _) => "claude-subscription".into(),
        ("codex-win", _) => "chatgpt-subscription".into(),
        (base, Some(provider)) => format!("{base}:{provider}"),
        (base, None) => base.into(),
    }
}

/// 各资源池在本次协作里的消耗与额度状态。订阅池只给额度使用率，不折成美元；
/// 线路池给服务商报的额度消耗（provider）与 API 等值估算，各自分开。
pub(crate) fn resource_pools(
    workflow: &Workflow,
    quota: &Value,
    providers: &std::collections::HashMap<(String, String), String>,
) -> Vec<Value> {
    let mut pools: Vec<(String, Value)> = Vec::new();
    for attempt in workflow.attempts.iter().filter(|attempt| attempt.status != "starting") {
        let base = crate::project_store::base_agent(&attempt.agent_id);
        let provider = attempt.model.as_ref().and_then(|model| {
            providers
                .get(&(base.to_owned(), model.clone()))
                .cloned()
                .or_else(|| crate::goat::line_of(model))
        });
        let key = pool_of(base, provider.as_deref());
        let index = match pools.iter().position(|(pool, _)| *pool == key) {
            Some(index) => index,
            None => {
                let kind = match base {
                    "claude-win" | "codex-win" => "subscription",
                    _ if provider.is_some() => "provider",
                    _ => "unknown",
                };
                // 线路池（如 GOAT）按池查，订阅按成员查。
                let quota = quota
                    .get(&key)
                    .or_else(|| quota.get(base))
                    .cloned()
                    .unwrap_or_else(|| json!("unknown"));
                pools.push((key.clone(), json!({"pool":key,"kind":kind,"agents":[],"quota":quota,"tokens":0,"unknown_attempts":0})));
                pools.len() - 1
            }
        };
        let pool = &mut pools[index].1;
        if !pool["agents"].as_array().unwrap().iter().any(|agent| agent == base) {
            pool["agents"].as_array_mut().unwrap().push(json!(base));
        }
        let usage = attempt.usage.as_ref();
        match usage.and_then(|usage| usage.spent()) {
            Some(spent) => pool["tokens"] = json!(pool["tokens"].as_u64().unwrap_or(0) + spent),
            None => pool["unknown_attempts"] = json!(pool["unknown_attempts"].as_u64().unwrap_or(0) + 1),
        }
        if let Some(cost) = usage.and_then(|usage| usage.cost_usd) {
            let field = match usage.and_then(|usage| usage.cost_kind.as_deref()) {
                Some("provider") => "provider_cost_usd",
                _ => "api_estimate_usd",
            };
            let sum = pool[field].as_f64().unwrap_or(0.0) + cost;
            pool[field] = json!((sum * 10000.0).round() / 10000.0);
        }
    }
    pools.into_iter().map(|(_, pool)| pool).collect()
}

/// 本次协作到目前为止的用量：成员干活与调度自己分开算。
fn usage_overview(workflow: &Workflow) -> Value {
    let (mut members, mut coordinator, mut unknown) = (0u64, 0u64, 0u64);
    for attempt in &workflow.attempts {
        let Some(spent) = attempt.usage.as_ref().and_then(|usage| usage.spent()) else {
            if attempt.status != "starting" {
                unknown += 1;
            }
            continue;
        };
        if attempt.activity.as_deref() == Some(coordinator_policy::MANAGER_ACTIVITY) {
            coordinator += spent;
        } else {
            members += spent;
        }
    }
    // unknown_attempts：用量未知的执行次数（成员没报、或只报了上下文占用），总数因此偏小。
    json!({"member_tokens":members,"coordinator_tokens":coordinator,"unknown_attempts":unknown})
}

impl Runtime {
    /// 新任务开工前：原仓库卡在合并/变基中途就先由管家汇报并处理；有未提交改动由管家先提交，
    /// 都在团队时间线说明，再继续派任务。
    pub(crate) fn checkpoint_source(
        &self,
        workflow: &str,
        root: &Path,
        deadline: Instant,
    ) -> Result<()> {
        if let Some(problem) = crate::project_worktree::midway_state(root)? {
            self.resolve_git_state(workflow, root, &problem, deadline)?;
        }
        if let Some(summary) = crate::project_worktree::checkpoint(root)? {
            let _ = self.store.lock().unwrap().add_system_team_message(
                workflow,
                &format!("项目里有未提交的改动，管家已先提交再继续任务：{summary}。"),
            );
            (self.notify)(workflow);
        }
        Ok(())
    }
    /// 管家汇报原仓库的冲突/中途状态，再由担任管家的成员用自己的项目会话在项目目录里解决。
    /// 处理后由框架重新核实；仍未解决就报错停下并保留现场，不假装完成。旧版协作没有管家，直接报错。
    pub(crate) fn resolve_git_state(
        &self,
        id: &str,
        root: &Path,
        problem: &str,
        deadline: Instant,
    ) -> Result<()> {
        let Some(manager) = self.store.lock().unwrap().coordination(id)? else {
            return Err(format!("{problem}，请先完成或放弃后继续协作"));
        };
        let agent = manager.coordinator.agent.clone();
        let name = crate::store::agent_name(&agent).to_owned();
        self.store.lock().unwrap().add_system_team_message(
            id,
            &format!("管家汇报：{problem}。由 {name} 在项目目录中处理后继续任务。"),
        )?;
        (self.notify)(id);
        let room = self.store.lock().unwrap().workflow(id)?.conversation_id;
        self.connect_coord_member(&agent)?;
        let settings = self.member_settings(&room, &agent)?;
        let attempt = loop {
            self.ensure_active(id, deadline)?;
            // 并行任务可能同时发现同一个冲突：等待期间已被处理好就直接继续。
            if crate::project_worktree::midway_state(root)?.is_none() {
                return Ok(());
            }
            let begun = self.store.lock().unwrap().begin_git_repair_attempt(
                id,
                &agent,
                settings.model.clone(),
                settings.effort.clone(),
            )?;
            if let Some(attempt) = begun {
                break attempt;
            }
            std::thread::sleep(Duration::from_millis(300));
        };
        let prompt = json!({"instruction":"项目仓库处于下面说明的合并、变基或冲突状态，挡住了协作中的后续任务。请在本轮工作目录（项目仓库本身）用 git 把这次操作完成：逐个解决冲突，保留双方改动的意图；不要用 reset --hard、checkout/restore 丢弃改动、clean 或 --abort 之类的方式放弃任何一方的工作，除非冲突双方确实无法共存且你已用 hub_ask_user 得到用户同意。解决后暂存并提交（合并用 git commit，变基用 git rebase --continue 直到完成）。完成后简短汇报处理了哪些文件、如何取舍；无法安全判断时用 hub_ask_user 提问并等待。","problem":problem});
        let result = self
            .native
            .run(attempt, root, &prompt.to_string(), deadline)?;
        if result.status == "completed" {
            let _ =
                self.store
                    .lock()
                    .unwrap()
                    .publish_coordinated_reply(id, &result, &result.output);
        }
        (self.notify)(id);
        if let Some(left) = crate::project_worktree::midway_state(root)? {
            return Err(format!("{name} 处理后{left}，已保留现场，请检查后继续协作"));
        }
        if result.status != "completed" {
            return Err(result
                .error
                .unwrap_or_else(|| format!("{name} 未完成冲突处理")));
        }
        Ok(())
    }
    fn connect_coord_member(&self, agent: &str) -> Result<()> {
        if self.snapshot(agent).connection == "connected" {
            return Ok(());
        }
        match crate::project_store::base_agent(agent) {
            "codex-win" => self.codex.connect().map(|_| ()),
            "hermes-win" => self.hermes.connect().map(|_| ()),
            "dsh-win" => self.dsh.connect().map(|_| ()),
            "claude-win" => self.claude.connect().map(|_| ()),
            _ => Err("不是项目技术成员".into()),
        }
    }
    fn member_settings(&self, room: &str, agent: &str) -> Result<RoleChoice> {
        let runtime = self.snapshot(agent);
        let store = self.store.lock().unwrap();
        let chat = store
            .project_agent_chats(room)?
            .into_iter()
            .find(|chat| chat.agent_id == agent)
            .ok_or("成员不在本项目")?;
        let session = store
            .detail(&chat.conversation_id)?
            .sessions
            .into_iter()
            .find(|session| session.agent_id == crate::project_store::base_agent(agent))
            .ok_or("项目模型配置不存在")?;
        Ok(RoleChoice {
            agent: agent.into(),
            model: session.model.or(runtime.default_model),
            effort: session.reasoning_effort.or(runtime.default_effort),
        })
    }
    pub(crate) fn start_coordinated(
        self: &Arc<Self>,
        room: &str,
        message: &str,
        content: &str,
    ) -> Result<Workflow> {
        let paused = self
            .store
            .lock()
            .unwrap()
            .workflows(room)?
            .into_iter()
            .find(|job| {
                job.coordination.is_some()
                    && matches!(job.status.as_str(), "failed" | "interrupted")
            });
        if let Some(job) = paused {
            if !["新一轮协作", "新项目目标", "重新开始协作"]
                .iter()
                .any(|term| content.starts_with(term))
            {
                self.store
                    .lock()
                    .unwrap()
                    .save_message(room, message, content)?;
                self.store
                    .lock()
                    .unwrap()
                    .connection
                    .execute(
                        "UPDATE messages SET status='delivered' WHERE id=?1",
                        [message],
                    )
                    .map_err(|_| "补充需求保存失败")?;
                self.store.lock().unwrap().connection.execute("INSERT OR IGNORE INTO project_team_messages(id,project_id,conversation_id,workflow_id,source_message_id,sender_agent,message_kind,content,created_at) VALUES(?1,?2,?3,?4,?1,NULL,'user',?5,?6)",rusqlite::params![message,job.project_id,room,job.id,content,crate::store::now()]).map_err(|_|"恢复需求保存失败")?;
                return self.resume_coordinated(&job.id);
            }
        }
        let configured = {
            let mut store = self.store.lock().unwrap();
            store.ensure_project_agent_chats(room)?;
            let members = store.conversation(room)?.members;
            store.project_coordinator(room)?.unwrap_or(RoleChoice {
                agent: ["hermes-win", "codex-win", "dsh-win", "claude-win"]
                    .iter()
                    .find(|id| members.iter().any(|member| member == **id))
                    .ok_or("项目没有可用技术成员")?
                    .to_string(),
                model: None,
                effort: None,
            })
        };
        self.connect_coord_member(&configured.agent)?;
        let fallback = self.member_settings(room, &configured.agent)?;
        let coordinator = RoleChoice {
            agent: configured.agent,
            model: configured.model.or(fallback.model),
            effort: configured.effort.or(fallback.effort),
        };
        let snapshot = self.snapshot(&coordinator.agent);
        crate::models::validate_selection(
            &snapshot.models,
            coordinator.model.as_deref(),
            coordinator.effort.as_deref(),
            snapshot.default_model.as_deref(),
        )?;
        let roles = self.default_roles_for_room(room)?;
        let (workflow, created) =
            self.create_with_connections(room, message, content, &roles, false)?;
        if !created {
            return Ok(workflow);
        }
        self.store
            .lock()
            .unwrap()
            .create_coordination(&workflow.id, &coordinator)?;
        let id = workflow.id.clone();
        let result = self.store.lock().unwrap().workflow(&id)?;
        self.spawn(id.clone(), move |runtime, deadline| {
            runtime.coordinate(&id, deadline)
        });
        Ok(result)
    }
    pub(crate) fn resume_coordinated(self: &Arc<Self>, id: &str) -> Result<Workflow> {
        // 先确认管家和待续任务的成员都已连接，否则恢复后会立即整轮失败。
        let needed = {
            let store = self.store.lock().unwrap();
            let job = store.workflow(id)?;
            let mut agents = vec![store
                .coordination(id)?
                .ok_or("调度记录缺失")?
                .coordinator
                .agent];
            for task in &job.tasks {
                if task.status != "completed" && !agents.contains(&task.agent_id) {
                    agents.push(task.agent_id.clone());
                }
            }
            agents
        };
        let offline: Vec<String> = needed
            .iter()
            .filter(|agent| {
                match agent.as_str() {
                    "codex-win" => self.codex.shared_project_client().map(|_| ()),
                    "hermes-win" => self.hermes.shared_project_client().map(|_| ()),
                    "dsh-win" => self.dsh.shared_project_client().map(|_| ()),
                    "claude-win" => self.claude.shared_project_client().map(|_| ()),
                    _ => Ok(()),
                }
                .is_err()
            })
            .map(|agent| crate::store::agent_name(agent).to_string())
            .collect();
        if !offline.is_empty() {
            return Err(format!("请先连接 {} 再继续协作", offline.join("、")));
        }
        self.coordinators.stop(id);
        let workflow = self.store.lock().unwrap().resume_coordination(id)?;
        let owned = id.to_owned();
        self.spawn(owned.clone(), move |runtime, deadline| {
            runtime.coordinate(&owned, deadline)
        });
        Ok(workflow)
    }
    pub(crate) fn coordinate(&self, id: &str, deadline: Instant) -> Result<()> {
        let outcome = self.coordinate_loop(id, deadline);
        self.coordinators.stop(id);
        outcome
    }
    /// 失败原因是额度用完或限流的任务。
    fn quota_blocked(&self, id: &str) -> Result<Option<crate::project_store::Task>> {
        Ok(self
            .store
            .lock()
            .unwrap()
            .workflow(id)?
            .tasks
            .into_iter()
            .find(|task| {
                task.status == "failed"
                    && task
                        .error
                        .as_deref()
                        .is_some_and(coordinator_policy::quota_exhausted)
            }))
    }
    /// 等额度恢复：公开说明后按间隔休眠（可随时停止），等待时间不计入协作时长，返回顺延后的截止时间。
    fn wait_for_quota(
        &self,
        id: &str,
        agent: &str,
        what: &str,
        error: &str,
        waits: u32,
        deadline: Instant,
    ) -> Result<Instant> {
        // 报错里带了重置时间（Claude/Codex 的五小时窗口等）就等到重置后一分钟；否则按递增间隔重试。
        let (offset, now) = crate::quota_reset::local_offset();
        let (wait, when) = match crate::quota_reset::reset_at(error, now, offset) {
            Some(at) => (
                Duration::from_secs((at - now + 60).max(60) as u64),
                format!("额度预计 {} 重置，届时", crate::quota_reset::local_label(at, offset)),
            ),
            None => {
                let minutes = coordinator_policy::quota_retry_minutes(waits);
                (
                    Duration::from_secs(minutes * 60),
                    format!("没读到重置时间，{minutes} 分钟后"),
                )
            }
        };
        let _ = self.store.lock().unwrap().add_system_team_message(
            id,
            &format!(
                "{} 的模型额度用完或被限流，{when}管家自动重试{what}；等待期间不计入协作时长，可随时停止协作。",
                crate::store::agent_name(agent)
            ),
        );
        (self.notify)(id);
        let started = Instant::now();
        while started.elapsed() < wait {
            if self.native.cancelled(id) {
                return Err("项目协作已停止，保留已产生的改动".into());
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        Ok(deadline + started.elapsed())
    }
    fn coordinate_loop(&self, id: &str, deadline: Instant) -> Result<()> {
        let mut deadline = deadline;
        // 额度等待最多连续这么多次（每次等到重置时间，读不到时按 5、10……30 分钟递增），之后交给管家按失败处理。
        const MAX_QUOTA_WAITS: u32 = 12;
        let mut quota_waits = 0;
        // 撞上限额的任务先交给调度看一次（改派给额度充足的成员，或者 wait 等重置），之后才原地等。
        let mut offered = std::collections::HashSet::new();
        loop {
            self.ensure_active(id, deadline)?;
            if self.store.lock().unwrap().acquire_project(id)? {
                break;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        let mut feedback: Option<String> = None;
        let mut rejected = 0;
        let mut routed_updates = 0;
        loop {
            self.ensure_active(id, deadline)?;
            // Queued independent work proceeds even if another member was stopped.
            let updates = self.store.lock().unwrap().project_user_updates(id)?.len();
            if updates == routed_updates && self.run_coordinated_batch(id, deadline)? {
                feedback = None;
                // 只有不再被额度卡住时才清零，否则重试又失败会一直停在最短间隔。
                if self.quota_blocked(id)?.is_none() {
                    quota_waits = 0;
                }
                continue;
            }
            let mut offer = None;
            if quota_waits < MAX_QUOTA_WAITS {
                if let Some(task) = self.quota_blocked(id)? {
                    if offered.insert(task.id.clone()) {
                        offer = Some(quota_offer(&task));
                    }
                }
            }
            if offer.is_none() && quota_waits < MAX_QUOTA_WAITS {
                if let Some(task) = self.quota_blocked(id)? {
                    deadline = self.wait_for_quota(
                        id,
                        &task.agent_id,
                        &format!("“{}”", task.title),
                        task.error.as_deref().unwrap_or_default(),
                        quota_waits,
                        deadline,
                    )?;
                    quota_waits += 1;
                    // 重试再撞上限额是新的一次，再让调度看。
                    offered.remove(&task.id);
                    self.store.lock().unwrap().connection.execute(
                        "UPDATE project_tasks SET status='queued',error=NULL WHERE id=?1 AND status='failed'",
                        [&task.id],
                    ).map_err(|_| "任务重新排队失败")?;
                    (self.notify)(id);
                    continue;
                }
            }
            let (workflow, manager, sources) = {
                let store = self.store.lock().unwrap();
                (
                    store.workflow(id)?,
                    store.coordination(id)?.ok_or("调度记录缺失")?,
                    store.coordination_sources(id)?,
                )
            };
            let members = self
                .store
                .lock()
                .unwrap()
                .conversation(&workflow.conversation_id)?
                .members;
            // 额外席位（同一成员的另一条会话）也作为独立成员交给管家：base_agent 说明它落在哪位成员上。
            let seats = self.store.lock().unwrap().project_agent_chats(&workflow.conversation_id)?;
            let temporary = self.store.lock().unwrap().temporary_seats(id)?;
            let opened = self.store.lock().unwrap().coordinator_seats(&workflow.conversation_id)?;
            let preference = self.store.lock().unwrap().project_preference(&workflow.conversation_id);
            // 同一位成员的几个会话共用一份模型目录：按 base_agent 只给一次，省得每回合重复几千字。
            let mut models_by_member = serde_json::Map::new();
            // (base_agent, 模型) → 线路（提供商）：用来把各次执行归到所属的资源池。
            let mut providers = std::collections::HashMap::new();
            let catalogs = members
                .iter()
                .filter(|agent| crate::project_store::EXECUTOR_AGENTS.contains(&agent.as_str()))
                .map(|agent| (agent.clone(), None))
                .chain(
                    seats
                        .into_iter()
                        .filter(|chat| {
                            let base = crate::project_store::base_agent(&chat.agent_id);
                            chat.agent_id != base && members.iter().any(|member| member == base)
                        })
                        .map(|chat| (chat.agent_id, chat.label)),
                )
                .map(|(agent, _)| {
                    let runtime = self.snapshot(&agent);
                    // display_name：用户给这位成员 / 这个席位起的名字，原话里可能用它称呼。
                    let (display, context) = {
                        let store = self.store.lock().unwrap();
                        (
                            store.seat_name(&workflow.conversation_id, &agent),
                            store.seat_context(&workflow.conversation_id, &agent),
                        )
                    };
                    let base = crate::project_store::base_agent(&agent);
                    if !models_by_member.contains_key(base) {
                        let models = runtime
                            .models
                            .iter()
                            .map(|model| {
                                let mut value = json!({"id":model.id,"efforts":model.efforts});
                                // 线路名优先取模型 id 里带的（和额度概况的键一致），没有再用目录给的提供商。
                                if let Some(provider) = crate::goat::line_of(&model.id).or_else(|| model.provider_id.clone()) {
                                    value["pool"] = json!(pool_of(base, Some(&provider)));
                                    providers.insert((base.to_owned(), model.id.clone()), provider);
                                }
                                if model.name != model.id {
                                    value["name"] = json!(model.name);
                                }
                                if let Some(mut price) = crate::pricing::price(model) {
                                    if let Some(price) = price.as_object_mut() {
                                        price.remove("name");
                                    }
                                    value["price"] = price;
                                }
                                value
                            })
                            .collect::<Vec<_>>();
                        models_by_member.insert(base.to_owned(), json!(models));
                    }
                    json!({"agent_id":agent,"base_agent":base,"display_name":display,"connection":runtime.connection,"project_settings":self.member_settings(&workflow.conversation_id,&agent).ok(),"context_tokens":context,"opened_by_coordinator":opened.contains(&agent)})
                })
                .collect::<Vec<_>>();
            self.store
                .lock()
                .unwrap()
                .coordination_status(id, "routing")?;
            (self.notify)(id);
            let attempt = self.store.lock().unwrap().begin_coordinated_attempt(
                id,
                None,
                &manager.coordinator.agent,
                manager.coordinator.model.clone(),
                manager.coordinator.effort.clone(),
            )?;
            let root = crate::coordinator_clients::manager_directory(
                &self.store.lock().unwrap().path,
                id,
                &manager.coordinator.agent,
            )?;
            // 说明放在最前、状态在后：几回合挨得近时，说明这段能命中提示缓存。
            let previous_ledger = match manager.ledger {
                None => self.store.lock().unwrap().previous_ledger(id),
                Some(_) => None,
            };
            let quota = crate::quota::overview(&self.codex);
            // 近一年的历史执行汇总成能力画像；读不到就不给，不影响调度。
            let capability = self
                .store
                .lock()
                .unwrap()
                .capability_samples(crate::store::now() - 365 * 24 * 3600 * 1000)
                .map(|samples| crate::capability::profiles(&samples, crate::store::now(), 40))
                .unwrap_or_default();
            if offer.is_some() {
                feedback = offer.take();
            }
            let grants = self.store.lock().unwrap().project_grants(&workflow.conversation_id);
            // 状态分两段：几回合内基本不变的大块（模型目录、价格表、历史画像等）放前面，能跨回合命中提示缓存；
            // 每回合会变的放后面。serde_json 的对象按键名排序，混在一起时变化的键会排到大块前面，整段缓存失效。
            // 只放一次协作里几乎不变的：模型目录、价格表、默认角色、长期授权、倾向。Claude 管家把这段放进系统提示
            // 命中缓存（见 claude_bridge 的 splitReference）；历史画像在成员完成任务后会变，放到每回合状态里。
            let reference = format!(
                "{{\"models\":{},\"pricing\":{},\"default_roles\":{},\"project_grants\":{},\"preference\":{}}}",
                Value::Object(models_by_member),
                crate::pricing::about(),
                json!(crate::project_store::roles_of(&workflow)?),
                json!((!grants.is_empty()).then_some(&grants)),
                json!(preference)
            );
            let state = json!({"ledger":manager.ledger,"previous_ledger":previous_ledger,"quota":quota,"limits":{"new_sessions":{"max":crate::coordinator_store::temporary_limit(&preference),"opened":temporary.len()}},"usage":usage_overview(&workflow),"capability":capability,"resource_pools":resource_pools(&workflow,&quota,&providers),"logical_tasks":logical_usage(&workflow,&manager.routes),"sources":compact_sources(&sources),"tasks":compact_tasks(&workflow),"routes":compact_routes(&manager.routes),"available_agents":catalogs,"routing_feedback":feedback});
            let body = format!("参考资料（JSON，几回合内基本不变）：\n{reference}\n\n本回合状态（JSON）：\n{state}");
            // Claude、Codex 的调度会话把说明放在系统提示里（每回合不变、能命中缓存），这里只给资料和状态；其他线路说明放最前面。
            let input = if matches!(
                crate::project_store::base_agent(&manager.coordinator.agent),
                "claude-win" | "codex-win"
            ) {
                body
            } else {
                format!("{}\n\n{body}", coordinator_policy::MANAGER_INSTRUCTION)
            };
            let result = self.native.run(attempt, &root, &input, deadline)?;
            if result.status != "completed" {
                let error = result.error.unwrap_or_else(|| "管家回合未完成".into());
                if quota_waits < MAX_QUOTA_WAITS && coordinator_policy::quota_exhausted(&error) {
                    deadline = self.wait_for_quota(
                        id,
                        &manager.coordinator.agent,
                        "调度",
                        &error,
                        quota_waits,
                        deadline,
                    )?;
                    quota_waits += 1;
                    continue;
                }
                return Err(error);
            }
            let decision = coordinator_policy::parse(&result.output);
            let applied = decision
                .and_then(|decision| self.apply_coordination(id, &result.id, decision, deadline));
            match applied {
                Ok(true) => return Ok(()),
                Ok(false) => {
                    rejected = 0;
                    feedback = None;
                    routed_updates = updates;
                }
                Err(error) => {
                    self.ensure_active(id, deadline)?;
                    rejected += 1;
                    if rejected >= 3 {
                        return Err(format!("管家未能形成符合用户原话的路由：{error}"));
                    }
                    feedback = Some(error);
                }
            }
        }
    }
    fn apply_coordination(
        &self,
        id: &str,
        attempt: &str,
        decision: Decision,
        deadline: Instant,
    ) -> Result<bool> {
        // 计划账本每回合都存，下一回合原样交还调度。
        if let Some(ledger) = decision.ledger.as_deref().filter(|ledger| !ledger.trim().is_empty()) {
            self.store.lock().unwrap().set_coordination_ledger(id, ledger)?;
        }
        let replacements = decision
            .routes
            .iter()
            .map(|route| (route.source_id.clone(), route.quote.clone()))
            .collect::<Vec<_>>();
        self.store
            .lock()
            .unwrap()
            .withdraw_coordination_tasks(id, &decision.withdraw, &replacements)?;
        // 调度自己拍板的取舍发到群里：不等回答，用户不同意直接在群里说。回合被退回重做时不发，免得重复。
        let decided = (!decision.decisions.is_empty()).then(|| {
            let lines = decision
                .decisions
                .iter()
                .map(|item| match item.reason.as_deref().map(str::trim).filter(|reason| !reason.is_empty()) {
                    Some(reason) => format!("\n· {}：{}（{}）", item.question.trim(), item.choice.trim(), reason),
                    None => format!("\n· {}：{}", item.question.trim(), item.choice.trim()),
                })
                .collect::<String>();
            format!("调度先按下面的选择推进，不同意直接在群里说，会按你说的改。{lines}")
        });
        if decision.action != "delegate" {
            if let Some(text) = &decided {
                self.store.lock().unwrap().add_system_team_message(id, text)?;
            }
        }
        match decision.action.as_str() {
            "delegate" => {
                let workflow = self.store.lock().unwrap().workflow(id)?;
                let sources = self.store.lock().unwrap().coordination_sources(id)?;
                let mut routes = decision.routes;
                let aliases = self.store.lock().unwrap().member_aliases(&workflow.conversation_id);
                for route in &routes {
                    coordinator_policy::check_assignment_with(route, &sources, &aliases)?;
                }
                let opened = self.open_temporary_seats(id, &mut routes)?;
                let applied = self.enqueue_decision(id, &workflow, &sources, routes, decided.as_deref());
                if applied.is_err() {
                    // 这一回合没派成：刚开的会话一起撤掉，不留空壳。
                    let mut store = self.store.lock().unwrap();
                    for seat in &opened {
                        let _ = store.remove_project_seat(&workflow.conversation_id, seat);
                    }
                }
                applied
            }
            "ask_user" => {
                let question = decision.question.ok_or("问题为空")?;
                let db = self.store.lock().unwrap().path.clone();
                // Reuse the existing public question/actual-answer path without exposing the hidden chat.
                self.store
                    .lock()
                    .unwrap()
                    .connection
                    .execute(
                        "UPDATE project_attempts SET status='running' WHERE id=?1",
                        [attempt],
                    )
                    .map_err(|_| "问题状态保存失败")?;
                let idq = crate::project_questions::post(
                    &db,
                    attempt,
                    &question.prompt,
                    question.options,
                )?;
                self.store
                    .lock()
                    .unwrap()
                    .connection
                    .execute(
                        "UPDATE project_attempts SET status='completed' WHERE id=?1",
                        [attempt],
                    )
                    .map_err(|_| "问题状态保存失败")?;
                self.store
                    .lock()
                    .unwrap()
                    .coordination_status(id, "awaiting_user")?;
                (self.notify)(id);
                loop {
                    self.ensure_active(id, deadline)?;
                    let answered: bool = self
                        .store
                        .lock()
                        .unwrap()
                        .connection
                        .query_row(
                            "SELECT status='answered' FROM project_questions WHERE id=?1",
                            [&idq],
                            |row| row.get(0),
                        )
                        .unwrap_or(false);
                    if answered {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Ok(false)
            }
            "finish" => {
                // 分板块收尾：没开始的任务留到下次，剩下的板块发到群里让用户挑。
                let deferred = if decision.next_sections.is_empty() {
                    0
                } else {
                    self.store.lock().unwrap().defer_unstarted_tasks(id)?
                };
                let summary = self
                    .store
                    .lock()
                    .unwrap()
                    .finished_coordination_summary(id, decision.result.as_ref())?;
                if !decision.next_sections.is_empty() {
                    let lines = decision
                        .next_sections
                        .iter()
                        .enumerate()
                        .map(|(index, section)| {
                            let title = coordinator_policy::section_title(&section.title);
                            match section.summary.as_deref().map(str::trim).filter(|text| !text.is_empty()) {
                                Some(text) => format!("\n{}. {}：{}", index + 1, title, text),
                                None => format!("\n{}. {}", index + 1, title),
                            }
                        })
                        .collect::<String>();
                    let kept = if deferred > 0 { format!("（{deferred} 项没开始的任务一起留到下次）") } else { String::new() };
                    self.store.lock().unwrap().add_system_team_message(
                        id,
                        &format!("这次先到这里{kept}。剩下的板块：{lines}\n想做哪个、哪天做，在群里说一声（比如「今天做第 2 块」），会接着这次的账本开始。"),
                    )?;
                }
                // 干过活的成员下次从零上下文开始：隔天续接只会整段重读过期的缓存。
                let _ = self.store.lock().unwrap().reset_worked_sessions(id);
                self.store
                    .lock()
                    .unwrap()
                    .coordination_status(id, "finished")?;
                self.store
                    .lock()
                    .unwrap()
                    .finish_workflow(id, "completed", &summary, None)?;
                (self.notify)(id);
                Ok(true)
            }
            "wait" => {
                // 等撞上限额的成员：循环里按重置时间原地等，不暂停协作。
                if self.quota_blocked(id)?.is_some() {
                    return Ok(false);
                }
                let paused = self.store.lock().unwrap().pause_coordination_wait(id)?;
                (self.notify)(id);
                Ok(paused)
            }
            _ => Err("管家动作无效".into()),
        }
    }
    /// 调度要新开会话的任务：在倾向允许的数量内给它开一个新会话（只能开在项目成员上），任务改派过去。
    fn open_temporary_seats(
        &self,
        id: &str,
        routes: &mut [coordinator_policy::Route],
    ) -> Result<Vec<String>> {
        let fresh = |route: &coordinator_policy::Route| route.fresh_session && route.task_id.is_none();
        let wanted = routes.iter().filter(|route| fresh(route)).count();
        if wanted == 0 {
            return Ok(Vec::new());
        }
        let (room, limit, open) = {
            let store = self.store.lock().unwrap();
            let room = store.workflow(id)?.conversation_id;
            let limit = crate::coordinator_store::temporary_limit(&store.project_preference(&room));
            (room, limit, store.temporary_seats(id)?.len())
        };
        if open + wanted > limit {
            return Err(format!(
                "按当前倾向，一次协作最多新开 {limit} 个会话（已开 {open} 个）；请复用已有会话"
            ));
        }
        let members = self.store.lock().unwrap().conversation(&room)?.members;
        for route in routes.iter().filter(|route| fresh(route)) {
            let base = crate::project_store::base_agent(&route.agent_id);
            if !members.iter().any(|member| member == base) {
                return Err(format!(
                    "新会话只能开在本项目的成员上（available_agents），{} 不是本项目成员",
                    route.agent_id
                ));
            }
        }
        let mut opened = Vec::new();
        for route in routes.iter_mut().filter(|route| fresh(route)) {
            let base = crate::project_store::base_agent(&route.agent_id).to_owned();
            let label = crate::coordinator_store::purpose_label(&route.purpose).to_owned();
            let created = self.store.lock().unwrap().add_temporary_seat(id, &base, &label);
            match created {
                Ok(seat) => {
                    route.agent_id = seat.clone();
                    opened.push(seat);
                }
                Err(error) => {
                    let mut store = self.store.lock().unwrap();
                    for seat in &opened {
                        let _ = store.remove_project_seat(&room, seat);
                    }
                    return Err(format!("新开会话失败：{error}"));
                }
            }
        }
        Ok(opened)
    }
    /// 落实调度的分派：用户点名的模型/强度照办，否则用调度按任务选的；分派消息写明理由。
    fn enqueue_decision(
        &self,
        id: &str,
        workflow: &Workflow,
        sources: &[coordinator_policy::Source],
        routes: Vec<coordinator_policy::Route>,
        decided: Option<&str>,
    ) -> Result<bool> {
        let mut resolved = Vec::new();
        for route in routes {
            self.connect_coord_member(&route.agent_id)?;
            let snapshot = self.snapshot(&route.agent_id);
            let fallback = self.member_settings(&workflow.conversation_id, &route.agent_id)?;
            // 子代理验证的派单人是修改者本人，沿用其项目配置；子代理的模型由它自己选。
            let (model, effort) = if route.subagent {
                (fallback.model.clone(), fallback.effort.clone())
            } else {
                coordinator_policy::overrides(&route, sources, &snapshot.models, &fallback)?
            };
            resolved.push((route, model, effort));
        }
        let ids = self.store.lock().unwrap().enqueue_routes(id, &resolved)?;
        if !self
            .store
            .lock()
            .unwrap()
            .workflow(id)?
            .tasks
            .iter()
            .any(|task| ids.contains(&task.id) && task.status == "queued")
        {
            return Err("这些任务已完成，请引用已有结果继续后续分工或结束，不能重复派发".into());
        }
        // 分派消息写明每项交给谁、用什么模型、为什么，用户一眼看得出调度的取舍。
        let lines = {
            let store = self.store.lock().unwrap();
            resolved
                .iter()
                .map(|(route, model, effort)| {
                    let setup = [model.as_deref(), effort.as_deref()]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join(" · ");
                    format!(
                        "\n· {}{}{}：{}",
                        store.seat_name(&workflow.conversation_id, &route.agent_id),
                        if setup.is_empty() { String::new() } else { format!("（{setup}）") },
                        if route.fresh_session {
                            "〔新开会话〕"
                        } else if route.clear_context {
                            "〔清空上下文〕"
                        } else {
                            ""
                        },
                        route
                            .reason
                            .as_deref()
                            .unwrap_or(crate::coordinator_store::purpose_label(&route.purpose))
                    )
                })
                .collect::<String>()
        };
        if let Some(text) = decided {
            self.store.lock().unwrap().add_system_team_message(id, text)?;
        }
        self.store
            .lock()
            .unwrap()
            .add_system_team_message(id, &format!("调度已分派{}项工作。{lines}", ids.len()))?;
        (self.notify)(id);
        Ok(false)
    }
    /// 现在能开工的任务：依赖已完成并合并、会话空闲（每个会话同时只做一项），最多 room 项。
    /// busy_tasks / busy_agents 是正在跑的任务和会话。
    fn runnable_tasks(
        &self,
        id: &str,
        busy_tasks: &HashSet<String>,
        busy_agents: &HashSet<String>,
        room: usize,
    ) -> Result<Vec<Task>> {
        let workflow = self.store.lock().unwrap().workflow(id)?;
        let metadata = self.store.lock().unwrap().coordination_routes(id)?;
        let gate = failed_verification(&workflow.tasks, &metadata);
        let mut agents = busy_agents.clone();
        let mut tasks = workflow
            .tasks
            .iter()
            .filter(|task| {
                // 验证关卡：最新验证未通过时，事先排队的收尾先不跑，回合交给管家安排返修；
                // 管家看过该验证结果后明确引用它再派收尾，才放行。
                !gate.as_ref().is_some_and(|failed| {
                    metadata.iter().any(|route| {
                        route.task_id == task.id
                            && route.purpose == "closeout"
                            && !route.context_ids.contains(failed)
                    })
                })
            })
            .filter(|task| {
                // 前置任务还在等验证、没并入主线时，后续任务拿不到它的文件，先等合并；只有验证它的任务能开工。
                let verifies = |held: &str| {
                    metadata.iter().any(|route| {
                        route.task_id == task.id
                            && route.purpose == "verification"
                            && route.context_ids.iter().any(|context| context == held)
                    })
                };
                task.status == "queued"
                    && task.depends_on.iter().all(|at| {
                        workflow.tasks.get(*at as usize).is_some_and(|before| {
                            before.status == "completed"
                                && (!held(&metadata, &before.id) || verifies(&before.id))
                        })
                    })
            })
            .filter(|task| !busy_tasks.contains(&task.id) && agents.insert(task.agent_id.clone()))
            .take(room)
            .cloned()
            .collect::<Vec<_>>();
        // Closing files/commits/push must operate on the merged project, after other queued work.
        if tasks.iter().any(|task| {
            metadata
                .iter()
                .any(|route| route.task_id == task.id && route.purpose == "closeout")
        }) {
            if tasks.iter().any(|task| {
                metadata
                    .iter()
                    .any(|route| route.task_id == task.id && route.purpose != "closeout")
            }) {
                tasks.retain(|task| {
                    metadata
                        .iter()
                        .any(|route| route.task_id == task.id && route.purpose != "closeout")
                });
            } else if !busy_tasks.is_empty()
                || workflow.tasks.iter().any(|task| {
                    !matches!(task.status.as_str(), "completed" | "skipped")
                        && !tasks.iter().any(|candidate| candidate.id == task.id)
                })
                || metadata.iter().any(|route| held(&metadata, &route.task_id))
            {
                // 还有任务在跑或在等验证后合并：收尾要在合并后的项目上做，先不开。
                return Ok(Vec::new());
            } else {
                tasks.truncate(1);
            }
        }
        Ok(tasks)
    }

    /// 一项任务结束后：合并（或按调度要求先保留等验证）、验证通过就合并被验分支、发布交付、标记完成。
    fn finish_batch_task(
        &self,
        id: &str,
        task: &Task,
        result: crate::project_store::Attempt,
        deadline: Instant,
    ) -> Result<()> {
        let workflow = self.store.lock().unwrap().workflow(id)?;
        let metadata = self.store.lock().unwrap().coordination_routes(id)?;
        if result.status == "completed" {
            let route = metadata.iter().find(|route| route.task_id == task.id);
            if route.is_some_and(|route| route.hold_merge && route.purpose != "verification") {
                // 调度要求合并前验证：分支和工作树留着，等验证通过再并入。
                self.store.lock().unwrap().set_merge_state(&task.id, Some("held"))?;
                self.store.lock().unwrap().add_system_team_message(
                    id,
                    &format!("「{}」已完成，按调度安排验证通过后再合并，任务分支先保留。", task.title),
                )?;
            } else {
                self.merge_task(id, &workflow.project_id, &task.id, deadline)?;
            }
            // 验证在被验任务的分支上做：结论写明通过才合并；未通过或没写结论都先不合并，交调度安排返修或复验。
            if let Some(route) = route.filter(|route| route.purpose == "verification") {
                let passed = crate::capability::verdict(&result.output) == Some(true);
                for held_task in route.context_ids.iter().filter(|context| held(&metadata, context)) {
                    let title = workflow
                        .tasks
                        .iter()
                        .find(|task| &task.id == held_task)
                        .map_or(held_task.as_str(), |task| task.title.as_str());
                    if passed {
                        self.merge_task(id, &workflow.project_id, held_task, deadline)?;
                        self.store.lock().unwrap().add_system_team_message(
                            id,
                            &format!("「{title}」验证通过，改动已合并。"),
                        )?;
                    } else {
                        self.store.lock().unwrap().add_system_team_message(
                            id,
                            &format!("「{title}」验证未通过或没写结论，暂不合并；任务分支保留，可用同一任务返修后再验证。"),
                        )?;
                    }
                }
            }
            self.store.lock().unwrap().publish_coordinated_reply(
                id,
                &result,
                &result.output,
            )?;
        }
        self.store
            .lock()
            .unwrap()
            .finish_coordinated_task(&task.id, &result)?;
        Ok(())
    }

    /// 滚动执行：互不依赖、会话不同的任务一起跑（最多 MAX_PARALLEL 项）；哪项做完就当场处理它的结果，
    /// 并立刻补上因此能开工的任务，不等同批里最慢的那项。都跑完、没有能开工的了才交回调度。
    fn run_coordinated_batch(&self, id: &str, deadline: Instant) -> Result<bool> {
        const MAX_PARALLEL: usize = 4;
        let first = self.runnable_tasks(id, &HashSet::new(), &HashSet::new(), MAX_PARALLEL)?;
        if first.is_empty() {
            return Ok(false);
        }
        self.store
            .lock()
            .unwrap()
            .coordination_status(id, "waiting_members")?;
        (self.notify)(id);
        std::thread::scope(|scope| -> Result<()> {
            let (sender, receiver) = std::sync::mpsc::channel::<(Task, Result<crate::project_store::Attempt>)>();
            let mut busy: Vec<Task> = Vec::new();
            let start = |task: Task, busy: &mut Vec<Task>| {
                busy.push(task.clone());
                let sender = sender.clone();
                scope.spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        self.run_coordinated_task(id, &task, deadline)
                    }))
                    .unwrap_or_else(|_| Err("成员运行线程异常".into()));
                    let _ = sender.send((task, result));
                });
            };
            for task in first {
                start(task, &mut busy);
            }
            let mut failure: Option<String> = None;
            while !busy.is_empty() {
                let Ok((task, result)) = receiver.recv() else {
                    break;
                };
                busy.retain(|running| running.id != task.id);
                if failure.is_some() {
                    continue;
                }
                // 出错就不再开新任务，等在跑的都结束后把错误交回去。
                let outcome = result
                    .and_then(|result| self.finish_batch_task(id, &task, result, deadline))
                    .and_then(|_| self.ensure_active(id, deadline))
                    .and_then(|_| {
                        let busy_tasks = busy.iter().map(|task| task.id.clone()).collect();
                        let busy_agents = busy.iter().map(|task| task.agent_id.clone()).collect();
                        self.runnable_tasks(id, &busy_tasks, &busy_agents, MAX_PARALLEL - busy.len())
                    });
                match outcome {
                    Ok(more) => {
                        for task in more {
                            start(task, &mut busy);
                        }
                    }
                    Err(error) => failure = Some(error),
                }
            }
            failure.map_or(Ok(()), Err)
        })?;
        Ok(true)
    }
    /// 把任务分支并入主线并清理工作树；合并冲突交管家处理，核实确实已并入才继续。
    fn merge_task(&self, id: &str, project: &str, task_id: &str, deadline: Instant) -> Result<()> {
        let current = self.store.lock().unwrap().task(task_id)?;
        let (Some(path), Some(branch)) = (&current.worktree, &current.branch) else {
            return Ok(());
        };
        let project = self.store.lock().unwrap().project(project)?;
        let root = Path::new(&project.root);
        if let Err(error) = crate::project_worktree::merge(root, branch) {
            let Some(problem) = crate::project_worktree::midway_state(root)? else {
                return Err(format!("任务改动合并失败：{error}"));
            };
            self.resolve_git_state(
                id,
                root,
                &format!("合并任务分支 {branch} 时出现冲突；{problem}"),
                deadline,
            )?;
            if !crate::project_worktree::merged(root, branch) {
                return Err(format!(
                    "冲突处理后任务分支 {branch} 仍未并入，已保留现场，请检查后继续协作"
                ));
            }
        }
        self.store.lock().unwrap().set_merge_state(task_id, Some("merged"))?;
        if crate::project_worktree::remove(root, Path::new(path)).is_err() {
            self.store.lock().unwrap().add_system_team_message(
                id,
                "任务改动已合并，临时工作树清理未完成；已有结果保留，不影响后续工作。",
            )?;
        }
        Ok(())
    }
    fn run_coordinated_task(
        &self,
        id: &str,
        task: &Task,
        deadline: Instant,
    ) -> Result<crate::project_store::Attempt> {
        let (workflow, project, route, sources, worktree_root) = {
            let store = self.store.lock().unwrap();
            let job = store.workflow(id)?;
            let project = store.project(&job.project_id)?;
            let route = store
                .coordination_routes(id)?
                .into_iter()
                .find(|route| route.task_id == task.id)
                .ok_or("路由记录缺失")?;
            (
                job,
                project,
                route,
                store.coordination_sources(id)?,
                store
                    .path
                    .parent()
                    .ok_or("数据目录不可用")?
                    .join("worktrees"),
            )
        };
        let root = Path::new(&project.root);
        // 验证一项等待合并的任务：直接在它的工作树里验，不另开分支。
        let held_tree = (route.purpose == "verification")
            .then(|| {
                let routes = self.store.lock().unwrap().coordination_routes(id).ok()?;
                route.context_ids.iter().rev().find_map(|context| {
                    let held_task = workflow.tasks.iter().find(|task| &task.id == context && held(&routes, context))?;
                    Some((held_task.worktree.clone()?, held_task.branch.clone()?))
                })
            })
            .flatten();
        let cwd = if route.purpose == "closeout" {
            root.to_path_buf()
        } else if let Some((path, branch)) = &held_tree {
            crate::project_worktree::reuse(root, &worktree_root, Path::new(path), branch)?
        } else if let (Some(path), Some(branch)) = (&task.worktree, &task.branch) {
            if Path::new(path).exists() {
                crate::project_worktree::reuse(root, &worktree_root, Path::new(path), branch)?
            } else {
                crate::project_worktree::attach(root, &worktree_root, &task.id, branch)?
            }
        } else {
            self.checkpoint_source(id, root, deadline)?;
            crate::project_worktree::create(
                root,
                &worktree_root,
                &task.id,
                &format!("hub/{}", task.id),
            )?
        };
        let branch = (route.purpose != "closeout" && held_tree.is_none()).then(|| {
            task.branch
                .clone()
                .unwrap_or_else(|| format!("hub/{}", task.id))
        });
        self.store.lock().unwrap().set_task_config(
            &task.id,
            task.model.clone(),
            task.effort.clone(),
            branch.as_ref().map(|_| cwd.to_string_lossy().to_string()),
            branch.clone(),
        )?;
        self.connect_coord_member(&task.agent_id)?;
        let fallback = self.member_settings(&workflow.conversation_id, &task.agent_id)?;
        let snapshot = self.snapshot(&task.agent_id);
        let model = task
            .model
            .clone()
            .or(fallback.model)
            .or(snapshot.default_model.clone());
        let effort = task.effort.clone().or(fallback.effort).or_else(|| {
            snapshot
                .models
                .iter()
                .find(|option| Some(&option.id) == model.as_ref())
                .and_then(|option| option.default_effort.clone())
        });
        crate::models::validate_selection(
            &snapshot.models,
            model.as_deref(),
            effort.as_deref(),
            snapshot.default_model.as_deref(),
        )?;
        let attempt = loop {
            self.ensure_active(id, deadline)?;
            let result = self.store.lock().unwrap().begin_coordinated_attempt(
                id,
                Some(task),
                &task.agent_id,
                model.clone(),
                effort.clone(),
            );
            match result {
                Ok(value) => break value,
                Err(error) if error == "成员仍有项目任务或私聊运行" => {
                    std::thread::sleep(Duration::from_millis(150))
                }
                Err(error) => return Err(error),
            }
        };
        let contexts = route
            .context_ids
            .iter()
            .filter_map(|source_id| sources.iter().find(|source| &source.id == source_id))
            .map(|source| json!({"source_id":source.id,"kind":source.kind,"text":source.text}))
            .collect::<Vec<Value>>();
        let mut prompt = coordinator_policy::member_prompt(
            &task.agent_id,
            &route.purpose,
            &workflow.request,
            &route.quote,
            contexts,
            route.instructions.as_deref(),
            route.allow_subagents,
        );
        prompt["user_updates"] = json!(self.store.lock().unwrap().project_user_updates(id)?);
        let grants = self.store.lock().unwrap().project_grants(&workflow.conversation_id);
        if !grants.is_empty() {
            prompt["project_grants"] = json!(grants);
        }
        if branch.is_some() {
            // 工作树在任务结束后会删掉：被 .gitignore 忽略的产物不会合并，要留的写到项目根目录。
            prompt["workspace"] = json!(format!(
                "本任务在临时工作树 {} 里做；结束时只合并纳入 Git 的改动，被 .gitignore 忽略的文件不合并，随后工作树会删掉。日志、报告这类不入库、又要留给后面任务或用户看的产物，写到项目根目录 {} 下的同一相对路径，不要只写在工作树里。",
                cwd.display(),
                root.display()
            ));
        }
        if route.subagent {
            let (cheap, _) = coordinator_policy::subagent_model(
                &snapshot.models,
                &crate::project_store::RoleChoice {
                    agent: task.agent_id.clone(),
                    model: model.clone(),
                    effort: None,
                },
            );
            prompt["task_scope"] = json!(coordinator_policy::subagent_scope(
                &task.agent_id,
                cheap.as_deref()
            ));
        }
        // 有工作树的任务应只改工作树；记下根目录原有改动，任务后对比，发现越界就公开提醒。
        let before = branch
            .as_ref()
            .and_then(|_| crate::project_worktree::root_changes(root).ok());
        let result = self
            .native
            .run(attempt, &cwd, &prompt.to_string(), deadline)?;
        if let Some(before) = before {
            if let Ok(after) = crate::project_worktree::root_changes(root) {
                let stray = crate::project_worktree::changed_since(&before, &after);
                if !stray.is_empty() {
                    let shown = stray.iter().take(8).cloned().collect::<Vec<_>>().join("、");
                    let more = if stray.len() > 8 {
                        format!(" 等 {} 项", stray.len())
                    } else {
                        String::new()
                    };
                    let _ = self.store.lock().unwrap().add_system_team_message(
                        id,
                        &format!(
                            "注意：{} 在本任务期间直接改动了项目根目录（{shown}{more}），这些改动不在任务分支里，不会随任务合并；请确认后保留、提交或移除。",
                            crate::store::agent_name(&task.agent_id)
                        ),
                    );
                }
            }
        }
        if result.status == "completed" {
            let mut files = crate::project_worktree::coordinated_files(&cwd)?;
            // 提交前查敏感内容：高置信度的文件不提交（也就不会合并），低置信度的提醒。
            let findings = crate::secret_scan::scan(&cwd, &files);
            let (high, low): (Vec<_>, Vec<_>) = findings.into_iter().partition(|finding| finding.high);
            if !high.is_empty() {
                files.retain(|file| !high.iter().any(|finding| &finding.file == file));
                let blocked: Vec<String> = high.iter().map(|finding| finding.file.clone()).collect();
                crate::project_worktree::unstage(&cwd, &blocked)?;
                let _ = self.store.lock().unwrap().add_system_team_message(
                    id,
                    &format!(
                        "「{}」的改动里有疑似密钥：{}。这些文件没有提交、不会合并，留在任务工作树里；确认安全的可把路径写进 .git/info/{}。",
                        task.title,
                        crate::secret_scan::describe(&high),
                        crate::secret_scan::ALLOW_FILE
                    ),
                );
            }
            if !low.is_empty() {
                let _ = self.store.lock().unwrap().add_system_team_message(
                    id,
                    &format!("提醒：「{}」的改动里 {} 看起来像写死的密码或密钥，请确认。", task.title, crate::secret_scan::describe(&low)),
                );
            }
            crate::project_worktree::commit(
                &cwd,
                &format!("tongxi: {} task {}", task.agent_id, task.id),
                &files,
            )?;
        }
        (self.notify)(id);
        Ok(result)
    }
}

#[tauri::command]
pub fn set_project_coordinator(
    runtime: tauri::State<'_, Arc<Runtime>>,
    conversation_id: String,
    coordinator: RoleChoice,
) -> Result<crate::store::ConversationDetail> {
    if runtime.snapshot(&coordinator.agent).connection == "connected" {
        let snapshot = runtime.snapshot(&coordinator.agent);
        crate::models::validate_selection(
            &snapshot.models,
            coordinator.model.as_deref(),
            coordinator.effort.as_deref(),
            snapshot.default_model.as_deref(),
        )?;
    }
    let mut store = runtime.store.lock().unwrap();
    store.set_project_coordinator(&conversation_id, &coordinator)?;
    store.detail(&conversation_id)
}

/// 套餐额度：Claude、Codex 的订阅窗口，以及 DSH、Hermes 走 Command Code（GOAT）线路的额度，进度页用。
#[tauri::command]
pub async fn project_quota(runtime: tauri::State<'_, Arc<Runtime>>) -> Result<Value> {
    let codex = runtime.codex.clone();
    tauri::async_runtime::spawn_blocking(move || crate::quota::overview(&codex))
        .await
        .map_err(|_| "额度查询失败".to_string())
}

/// 历史表现：近一年各「成员+模型+强度+类别」组合的能力与成本画像，进度页的对比表用。
#[tauri::command]
pub fn project_capability(runtime: tauri::State<'_, Arc<Runtime>>) -> Result<Vec<Value>> {
    let now = crate::store::now();
    let samples = runtime
        .store
        .lock()
        .unwrap()
        .capability_samples(now - 365 * 24 * 3600 * 1000)?;
    Ok(crate::capability::profiles(&samples, now, 60))
}

/// 调度倾向：save（省额度）/ balanced（平衡）/ quality（质量优先）。影响调度怎么挑模型、一次协作能新开几个会话。
#[tauri::command]
pub fn set_project_preference(
    runtime: tauri::State<'_, Arc<Runtime>>,
    conversation_id: String,
    preference: String,
) -> Result<crate::store::ConversationDetail> {
    let mut store = runtime.store.lock().unwrap();
    store.set_project_preference(&conversation_id, &preference)?;
    store.detail(&conversation_id)
}

/// 项目长期授权：用户写一段话（如「允许提交并推送 main，不 force push」），调度和成员每次协作都当作用户原话授权。
#[tauri::command]
pub fn set_project_grants(
    runtime: tauri::State<'_, Arc<Runtime>>,
    conversation_id: String,
    grants: String,
) -> Result<crate::store::ConversationDetail> {
    let mut store = runtime.store.lock().unwrap();
    store.set_project_grants(&conversation_id, &grants)?;
    store.detail(&conversation_id)
}

#[cfg(test)]
mod tests {
    use crate::coordinator_policy::Source;

    #[test]
    fn earlier_member_reports_keep_their_delivery_summary() {
        let source = |text: String| Source { id: "m".into(), kind: "context".into(), text };
        let report = format!("{}\n\n交付摘要：已部署。遗留：OpenViking 要另行排查。", "过程".repeat(800));
        let text = super::compact_sources(&[source(report)])[0]["text"].as_str().unwrap().to_owned();
        assert!(text.starts_with("交付摘要：") && text.contains("OpenViking"));
        let chat = super::compact_sources(&[source("闲聊".repeat(400))])[0]["text"].as_str().unwrap().to_owned();
        assert_eq!(chat.chars().count(), 602);
    }

    /// 验收场景：DSH 首次实现失败 → Codex 改派接手 → Claude 验证未通过 → Codex 返修 → 复核；调度另计一次。
    #[test]
    fn logical_task_collects_failed_attempts_repairs_and_verification() {
        use crate::project_store::{Attempt, Task, Usage, Workflow};
        let task = |id: &str, position: u32| Task {
            id: id.into(), workflow_id: "w".into(), position, title: format!("任务{id}"), agent_id: "codex-win".into(),
            instructions: String::new(), files: vec![], depends_on: vec![], status: "completed".into(), output: String::new(),
            error: None, model: None, effort: None, worktree: None, branch: None,
        };
        let attempt = |task: Option<&str>, status: &str, usage: Option<Usage>, activity: Option<&str>| Attempt {
            id: format!("a{}", rand_id()), created_at: 0, workflow_id: "w".into(), task_id: task.map(String::from),
            agent_id: "codex-win".into(), stage: "implement".into(), activity: activity.map(String::from), status: status.into(),
            native_thread_id: None, native_turn_id: None, model: None, reasoning_effort: None, thought: String::new(),
            tool_events: vec![], output: String::new(), error: None, usage,
        };
        fn rand_id() -> u64 { use std::sync::atomic::{AtomicU64, Ordering}; static N: AtomicU64 = AtomicU64::new(0); N.fetch_add(1, Ordering::Relaxed) }
        let used = |n: u64| Some(Usage { input: n, ..Default::default() });
        let route = |task: &str, purpose: &str, quote: &str, context: &[&str]| -> crate::coordinator_store::TaskRoute {
            serde_json::from_value(serde_json::json!({"task_id":task,"purpose":purpose,"source_id":"u1","quote":quote,"context_ids":context,"subagent":false})).unwrap()
        };
        let workflow = Workflow {
            id: "w".into(), project_id: "p".into(), conversation_id: "c".into(), user_message_id: "u1".into(), request: String::new(),
            status: "running".into(), plan: None, roles: None, coordination: None, summary: String::new(), error: None,
            created_at: 0, updated_at: 0,
            tasks: vec![task("impl", 1), task("verify", 2), task("repair", 3), task("recheck", 4), task("docs", 5)],
            attempts: vec![
                attempt(Some("impl"), "failed", used(100), None),
                attempt(Some("impl"), "completed", used(300), None),
                attempt(Some("verify"), "completed", used(50), None),
                attempt(Some("repair"), "completed", used(200), None),
                attempt(Some("recheck"), "completed", used(40), None),
                attempt(Some("docs"), "completed", used(70), None),
                attempt(None, "completed", used(1000), Some(super::coordinator_policy::MANAGER_ACTIVITY)),
            ],
            changes: vec![], agent_messages: vec![], team_messages: vec![],
        };
        // 文档任务锚定同一原文却没引用别的任务结果，不并进来。
        let routes = vec![
            route("impl", "implementation", "做登录", &[]),
            route("verify", "verification", "验证登录", &["impl"]),
            route("repair", "implementation", "做登录", &["verify"]),
            route("recheck", "verification", "验证登录", &["repair"]),
            route("docs", "implementation", "做登录", &[]),
        ];
        let groups = super::logical_tasks(&workflow, &routes);
        assert!(["impl", "verify", "repair", "recheck"].iter().all(|id| groups[*id] == "impl"));
        assert_eq!(groups["docs"], "docs");
        let logical = super::logical_usage(&workflow, &routes);
        assert_eq!(logical.len(), 1);
        let item = &logical[0];
        assert_eq!((item["tokens"].as_u64(), item["attempts"].as_u64(), item["failed_attempts"].as_u64()), (Some(690), Some(5), Some(1)));
        assert_eq!((item["by_purpose"]["implementation"].as_u64(), item["by_purpose"]["verification"].as_u64()), (Some(600), Some(90)));
        // 调度只在协作总账里算一次。
        let overview = super::usage_overview(&workflow);
        assert_eq!((overview["member_tokens"].as_u64(), overview["coordinator_tokens"].as_u64()), (Some(760), Some(1000)));
        // 有一次只报上下文占用：逻辑任务合计变为未知，不当 0。
        let mut partial = workflow.clone();
        partial.attempts[2].usage = Some(Usage { input: 9000, context_only: true, ..Default::default() });
        let logical = super::logical_usage(&partial, &routes);
        assert!(logical[0]["tokens"].is_null() && logical[0]["by_purpose"]["verification"].is_null());
    }

    pub(crate) fn sample_attempt(agent: &str, model: &str, status: &str, usage: Option<crate::project_store::Usage>) -> crate::project_store::Attempt {
        crate::project_store::Attempt {
            id: format!("{agent}-{model}-{status}"), created_at: 0, workflow_id: "w".into(), task_id: None, agent_id: agent.into(),
            stage: "implement".into(), activity: None, status: status.into(), native_thread_id: None, native_turn_id: None,
            model: Some(model.into()), reasoning_effort: None, thought: String::new(), tool_events: vec![], output: String::new(),
            error: None, usage,
        }
    }

    pub(crate) fn sample_workflow(attempts: Vec<crate::project_store::Attempt>) -> crate::project_store::Workflow {
        crate::project_store::Workflow {
            id: "w".into(), project_id: "p".into(), conversation_id: "c".into(), user_message_id: "u1".into(), request: String::new(),
            status: "running".into(), plan: None, roles: None, coordination: None, summary: String::new(), error: None,
            created_at: 0, updated_at: 0, tasks: vec![], attempts, changes: vec![], agent_messages: vec![], team_messages: vec![],
        }
    }

    #[test]
    fn resource_pools_split_subscriptions_and_provider_lines_without_inventing_costs() {
        use crate::project_store::Usage;
        let usage = |n: u64, cost: Option<f64>, kind: Option<&str>| Some(Usage { input: n, cost_usd: cost, cost_kind: kind.map(String::from), ..Default::default() });
        let workflow = sample_workflow(vec![
            sample_attempt("claude-win", "sonnet", "completed", usage(100, Some(0.5), Some("api_estimate"))),
            sample_attempt("dsh-win", "flash", "completed", usage(200, Some(0.02), Some("provider"))),
            sample_attempt("dsh-win-2", "flash", "failed", Some(Usage { input: 50, context_only: true, ..Default::default() })),
            sample_attempt("hermes-win", "flash", "completed", None),
        ]);
        let providers = [(("dsh-win".to_owned(), "flash".to_owned()), "goat".to_owned())].into_iter().collect();
        let quota = serde_json::json!({"claude-win":{"five_hour":{"used_percent":40}}});
        let pools = super::resource_pools(&workflow, &quota, &providers);
        assert_eq!(pools.len(), 3);
        assert_eq!((pools[0]["pool"].as_str(), pools[0]["kind"].as_str()), (Some("claude-subscription"), Some("subscription")));
        assert_eq!(pools[0]["quota"]["five_hour"]["used_percent"], 40);
        assert_eq!(pools[0]["api_estimate_usd"], 0.5);
        assert!(pools[0].get("provider_cost_usd").is_none());
        // 额外席位与本体同一条线路，归同一池；只报上下文占用的那次记未知。
        assert_eq!((pools[1]["pool"].as_str(), pools[1]["tokens"].as_u64(), pools[1]["unknown_attempts"].as_u64()), (Some("dsh-win:goat"), Some(200), Some(1)));
        assert_eq!(pools[1]["provider_cost_usd"], 0.02);
        // Hermes 线路未知、也不报用量：池类型与额度都是 unknown，不当零成本。
        assert_eq!((pools[2]["kind"].as_str(), pools[2]["quota"].as_str(), pools[2]["unknown_attempts"].as_u64()), (Some("unknown"), Some("unknown"), Some(1)));
    }
}
