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
        match agent {
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
            .find(|session| session.agent_id == agent)
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
            if quota_waits < MAX_QUOTA_WAITS {
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
            let catalogs=members.iter().filter(|agent|crate::project_store::EXECUTOR_AGENTS.contains(&agent.as_str())).map(|agent|{let runtime=self.snapshot(agent);json!({"agent_id":agent,"connection":runtime.connection,"models":runtime.models,"project_settings":self.member_settings(&workflow.conversation_id,agent).ok()})}).collect::<Vec<_>>();
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
            let input = json!({"instruction":coordinator_policy::MANAGER_INSTRUCTION,"sources":sources,"tasks":workflow.tasks,"routes":manager.routes,"available_agents":catalogs,"default_roles":crate::project_store::roles_of(&workflow)?,"routing_feedback":feedback});
            let result = self
                .native
                .run(attempt, &root, &input.to_string(), deadline)?;
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
        self.store
            .lock()
            .unwrap()
            .withdraw_coordination_tasks(id, &decision.withdraw)?;
        match decision.action.as_str() {
            "delegate" => {
                let workflow = self.store.lock().unwrap().workflow(id)?;
                let sources = self.store.lock().unwrap().coordination_sources(id)?;
                let mut resolved = Vec::new();
                for route in decision.routes {
                    coordinator_policy::check_assignment(&route, &sources)?;
                    self.connect_coord_member(&route.agent_id)?;
                    let snapshot = self.snapshot(&route.agent_id);
                    let fallback =
                        self.member_settings(&workflow.conversation_id, &route.agent_id)?;
                    // 子代理验证的派单人是修改者本人，沿用其项目配置；子代理的模型由它自己选。
                    let (model, effort) = if route.subagent {
                        (fallback.model.clone(), fallback.effort.clone())
                    } else {
                        coordinator_policy::overrides(
                            &route,
                            &sources,
                            &snapshot.models,
                            &fallback,
                        )?
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
                    return Err(
                        "这些任务已完成，请引用已有结果继续后续分工或结束，不能重复派发".into(),
                    );
                }
                self.store.lock().unwrap().add_system_team_message(
                    id,
                    &format!("管家已按原文分派{}项工作。", ids.len()),
                )?;
                (self.notify)(id);
                Ok(false)
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
                let summary = self
                    .store
                    .lock()
                    .unwrap()
                    .finished_coordination_summary(id, decision.result.as_ref())?;
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
                let paused = self.store.lock().unwrap().pause_coordination_wait(id)?;
                (self.notify)(id);
                Ok(paused)
            }
            _ => Err("管家动作无效".into()),
        }
    }
    fn run_coordinated_batch(&self, id: &str, deadline: Instant) -> Result<bool> {
        let workflow = self.store.lock().unwrap().workflow(id)?;
        let metadata = self.store.lock().unwrap().coordination_routes(id)?;
        let gate = failed_verification(&workflow.tasks, &metadata);
        let mut agents = HashSet::new();
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
                task.status == "queued"
                    && task.depends_on.iter().all(|at| {
                        workflow
                            .tasks
                            .get(*at as usize)
                            .is_some_and(|task| task.status == "completed")
                    })
            })
            .filter(|task| agents.insert(task.agent_id.clone()))
            .take(2)
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
            } else if workflow.tasks.iter().any(|task| {
                !matches!(task.status.as_str(), "completed" | "skipped")
                    && !tasks.iter().any(|candidate| candidate.id == task.id)
            }) {
                return Ok(false);
            } else {
                tasks.truncate(1);
            }
        }
        if tasks.is_empty() {
            return Ok(false);
        }
        self.store
            .lock()
            .unwrap()
            .coordination_status(id, "waiting_members")?;
        (self.notify)(id);
        let results = std::thread::scope(|scope| {
            tasks
                .iter()
                .map(|task| {
                    let task = task.clone();
                    scope.spawn(move || self.run_coordinated_task(id, &task, deadline))
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .unwrap_or_else(|_| Err("成员运行线程异常".into()))
                })
                .collect::<Vec<_>>()
        });
        for (task, result) in tasks.iter().zip(results) {
            let result = result?;
            if result.status == "completed" {
                let current = self.store.lock().unwrap().task(&task.id)?;
                if let (Some(path), Some(branch)) = (&current.worktree, &current.branch) {
                    let project = self.store.lock().unwrap().project(&workflow.project_id)?;
                    let root = Path::new(&project.root);
                    if let Err(error) = crate::project_worktree::merge(root, branch) {
                        // 合并冲突：管家汇报并处理，核实分支确实已并入后再继续。
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
                    if crate::project_worktree::remove(Path::new(&project.root), Path::new(path))
                        .is_err()
                    {
                        self.store.lock().unwrap().add_system_team_message(
                            id,
                            "任务改动已合并，临时工作树清理未完成；已有结果保留，不影响后续工作。",
                        )?;
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
        }
        Ok(true)
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
        let cwd = if route.purpose == "closeout" {
            root.to_path_buf()
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
        let branch = (route.purpose != "closeout").then(|| {
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
        );
        prompt["user_updates"] = json!(self.store.lock().unwrap().project_user_updates(id)?);
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
                let stray = after.difference(&before).cloned().collect::<Vec<_>>();
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
            let files = crate::project_worktree::coordinated_files(&cwd)?;
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
