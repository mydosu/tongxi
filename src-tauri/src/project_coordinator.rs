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

/// 软预算的运行状态：最近一回合的参考资料（拨款请求复用它，命中调度系统提示的缓存）、
/// 被叫停的任务（task id → 错误说明）、拨款请求一次只发一个。
#[derive(Default)]
pub(crate) struct BudgetState {
    reference: std::sync::Mutex<Option<String>>,
    stopped: std::sync::Mutex<std::collections::HashMap<String, String>>,
    asking: std::sync::Mutex<()>,
}

/// 一次执行到现在的累计用量：Codex 实时报精确值，Claude 由桥接按每次模型调用累加（先记在上下文占位里），
/// DSH 读它自己的会话日志；Hermes 回合中途报不出，返回 None。
fn live_spent(attempt: &crate::project_store::Attempt) -> Option<u64> {
    let base = crate::project_store::base_agent(&attempt.agent_id);
    if base == "dsh-win" {
        let session = attempt.native_thread_id.as_deref()?;
        return crate::dsh::session_usage(session, attempt.created_at)?.spent();
    }
    let usage = attempt.usage.as_ref()?;
    if usage.context_only {
        return (base == "claude-win").then_some(usage.input + usage.output);
    }
    usage.spent()
}

/// 成员疑似原地打转：同一句话反复说（免费、弱模型常见），或工具调用接连失败。只看 since 之后的新内容
/// （上次问过调度之后的），返回给调度看的证据。
fn looping(attempt: &crate::project_store::Attempt, since: (usize, usize)) -> Option<String> {
    let output = attempt.output.get(since.0..).unwrap_or_default();
    let lines: Vec<&str> = output.lines().map(str::trim).filter(|line| line.chars().count() >= 12).collect();
    let recent = &lines[lines.len().saturating_sub(30)..];
    if let Some((line, count)) = recent
        .iter()
        .map(|line| (*line, recent.iter().filter(|other| *other == line).count()))
        .max_by_key(|(_, count)| *count)
        .filter(|(_, count)| *count >= 4)
    {
        return Some(format!("最近的回复里同一句话出现了 {count} 次：「{}」", line.chars().take(80).collect::<String>()));
    }
    let tools = attempt.tool_events.get(since.1..).unwrap_or_default();
    let last: Vec<_> = tools.iter().rev().take(4).collect();
    if last.len() == 4 && last.iter().all(|event| event.status == "failed") {
        return Some(format!("最近 4 次工具调用全部失败（{}）", last[0].title.chars().take(60).collect::<String>()));
    }
    None
}

/// 12345678 → 12.3M，给群消息看的。
fn token_label(value: u64) -> String {
    match value {
        v if v >= 1_000_000 => format!("{:.1}M", v as f64 / 1e6),
        v if v >= 1_000 => format!("{:.0}k", v as f64 / 1e3),
        v => v.to_string(),
    }
}

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
            json!({"task_id":route.task_id,"purpose":route.purpose,"source_id":route.source_id,"quote":clip(&route.quote,200),"context_ids":route.context_ids,"subagent":route.subagent,"merge":route.merge_state,"budget_tokens":route.budget_tokens,"advisor_calls":route.advisor_calls})
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

/// 各资源池在本次协作里的 token 消耗与额度状态（只统计 token，不折成金额）。
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
        // 按时间顺序覆盖：留下的是这条线路最近一次的结果。最近一次报错（余额不足、认证失败、服务报错）就标出来，
        // 调度别再往这里派；后来成功过就清掉。
        match (attempt.status.as_str(), attempt.error.as_deref()) {
            ("failed", Some(error)) => pool["last_error"] = json!(error.chars().take(160).collect::<String>()),
            ("completed", _) => {
                pool.as_object_mut().unwrap().remove("last_error");
            }
            _ => {}
        }
        match attempt.usage.as_ref().and_then(|usage| usage.spent()) {
            Some(spent) => pool["tokens"] = json!(pool["tokens"].as_u64().unwrap_or(0) + spent),
            None => pool["unknown_attempts"] = json!(pool["unknown_attempts"].as_u64().unwrap_or(0) + 1),
        }
    }
    pools.into_iter().map(|(_, pool)| pool).collect()
}

/// 给订阅额度的每个窗口补上同席在这个窗口里用掉的 token（window_tokens），调度据此估算一项任务会吃掉多少额度。
/// 窗口起点按「窗口长度 − 距重置」倒推；成员在同席以外的使用也算在 used_percent 里，估算偏保守。
fn window_tokens(quota: &mut Value, tokens_since: impl Fn(&str, i64) -> Option<u64>) {
    let now = crate::store::now();
    for base in ["claude-win", "codex-win"] {
        for (window, hours) in [("five_hour", 5.0), ("weekly", 168.0)] {
            let Some(entry) = quota.get_mut(base).and_then(|member| member.get_mut(window)) else {
                continue;
            };
            let Some(left) = entry["resets_in_hours"].as_f64() else { continue };
            let since = now - ((hours - left).max(0.0) * 3_600_000.0) as i64;
            if let Some(tokens) = tokens_since(base, since) {
                entry["window_tokens"] = json!(tokens);
                // 按已用比例粗估窗口还能跑多少 token；用得太少时比例不可靠，不给。
                let used = entry["used_percent"].as_f64().unwrap_or(0.0);
                if (5.0..100.0).contains(&used) && tokens > 0 {
                    entry["estimated_tokens_left"] = json!((tokens as f64 * (100.0 - used) / used) as u64);
                }
            }
        }
    }
}

/// 问答面板给调度看的部分：没答的（问了多久）和新答的。
fn inquiry_state(inquiries: &[crate::project_inquiries::Inquiry]) -> Value {
    let now = crate::store::now();
    let open: Vec<Value> = inquiries
        .iter()
        .filter(|item| item.status == "open")
        .map(|item| json!({"key":item.key,"question":item.question,"default":item.default_choice,"reversible":item.reversible,"asked_hours_ago":((now - item.created_at) as f64 / 3_600_000.0).round()}))
        .collect();
    let answers: Vec<Value> = inquiries
        .iter()
        .filter(|item| item.status == "answered" && !item.seen)
        .map(|item| json!({"key":item.key,"question":item.question,"answer":item.answer,"source_id":format!("inquiry:{}", item.id)}))
        .collect();
    json!({"open":open,"new_answers":answers})
}

/// 本次协作到目前为止的用量：成员干活与调度自己分开算。
fn usage_overview(workflow: &Workflow) -> Value {
    let (mut members, mut coordinator, mut unknown) = (0u64, 0u64, 0u64);
    // 失败执行的消耗按原因分；kept 是任务后来做完了的那部分（工作树留着、被接手，不全算白花）。
    let mut failed = serde_json::Map::new();
    let mut kept = 0u64;
    for attempt in &workflow.attempts {
        let Some(spent) = attempt.usage.as_ref().and_then(|usage| usage.spent()) else {
            if attempt.status != "starting" {
                unknown += 1;
            }
            continue;
        };
        if let Some(cause) = crate::capability::failure_cause(&attempt.status, attempt.error.as_deref()) {
            let entry = failed.entry(cause).or_insert(json!(0));
            *entry = json!(entry.as_u64().unwrap_or(0) + spent);
            let done = attempt.task_id.as_ref().is_some_and(|task| {
                workflow.tasks.iter().any(|item| &item.id == task && item.status == "completed")
            });
            if done {
                kept += spent;
            }
        }
        if attempt.activity.as_deref() == Some(coordinator_policy::MANAGER_ACTIVITY) {
            coordinator += spent;
        } else {
            members += spent;
        }
    }
    // unknown_attempts：用量未知的执行次数（成员没报、或只报了上下文占用），总数因此偏小。
    let mut value = json!({"member_tokens":members,"coordinator_tokens":coordinator,"unknown_attempts":unknown});
    if !failed.is_empty() {
        value["failed_tokens"] = json!({"by_cause":failed,"task_later_completed":kept});
    }
    value
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
            None if crate::quota_reset::already_reset(error, now) => (
                Duration::from_secs(60),
                "额度已经重置，1 分钟后".to_owned(),
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
    /// 调度主动暂停、到点自动继续：公开说明后等 hours 小时（可随时停止；用户在群里发话就提前结束），
    /// 等待不计入协作时长，返回顺延后的截止时间。
    fn rest(&self, id: &str, reason: &str, hours: f64, deadline: Instant) -> Result<Instant> {
        let (offset, now) = crate::quota_reset::local_offset();
        let at = now + (hours * 3600.0) as i64;
        self.store.lock().unwrap().add_system_team_message(
            id,
            &format!(
                "调度主动暂停协作：{}。预计 {} 自动继续；等待期间不计入协作时长，在群里发话会提前叫醒，也可随时停止协作。",
                reason.trim().trim_end_matches('。'),
                crate::quota_reset::local_label(at, offset)
            ),
        )?;
        self.store.lock().unwrap().coordination_status(id, "resting")?;
        (self.notify)(id);
        let room = self.store.lock().unwrap().workflow(id)?.conversation_id;
        let heard = |store: &crate::store::Store| -> Result<(usize, usize)> { Ok((store.project_user_updates(id)?.len(), store.answered_inquiries(&room))) };
        let updates = heard(&self.store.lock().unwrap())?;
        let (started, wait) = (Instant::now(), Duration::from_secs_f64(hours * 3600.0));
        let mut tick = 0u32;
        while started.elapsed() < wait {
            if self.native.cancelled(id) {
                return Err("项目协作已停止，保留已产生的改动".into());
            }
            tick += 1;
            if tick % 5 == 0 && heard(&self.store.lock().unwrap())? != updates {
                break;
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
            let mut quota = crate::quota::overview(&self.codex);
            window_tokens(&mut quota, |base, since| self.store.lock().unwrap().tokens_since(base, since).ok());
            // 近一年的历史执行汇总成能力画像；读不到就不给，不影响调度。
            let samples = self
                .store
                .lock()
                .unwrap()
                .capability_samples(crate::store::now() - 365 * 24 * 3600 * 1000)
                .unwrap_or_default();
            let capability = crate::capability::profiles(&samples, crate::store::now(), 40);
            let session_reuse = crate::capability::session_reuse(&samples);
            if offer.is_some() {
                feedback = offer.take();
            }
            let grants = self.store.lock().unwrap().project_grants(&workflow.conversation_id);
            // 状态分两段：几回合内基本不变的大块（模型目录、价格表、历史画像等）放前面，能跨回合命中提示缓存；
            // 每回合会变的放后面。serde_json 的对象按键名排序，混在一起时变化的键会排到大块前面，整段缓存失效。
            // 只放一次协作里几乎不变的：模型目录、价格表、默认角色、长期授权、倾向。Claude 管家把这段放进系统提示
            // 命中缓存（见 claude_bridge 的 splitReference）；历史画像在成员完成任务后会变，放到每回合状态里。
            // 长期计划放最后：它比模型目录、价格表变得勤，变了也只让后半段缓存失效。
            let plan_root = self.store.lock().unwrap().project(&workflow.project_id)?.root;
            let plan = crate::project_plan::read(Path::new(&plan_root));
            let reference = format!(
                "{{\"coordinator\":{},\"models\":{},\"pricing\":{},\"default_roles\":{},\"project_grants\":{},\"preference\":{},\"plan\":{}}}",
                json!(crate::project_store::base_agent(&manager.coordinator.agent)),
                Value::Object(models_by_member),
                crate::pricing::about(),
                json!(crate::project_store::roles_of(&workflow)?),
                json!((!grants.is_empty()).then_some(&grants)),
                json!(preference),
                json!(plan.map(|text| json!({"path":".tongxi/计划.md","text":text})))
            );
            let inquiries = self.store.lock().unwrap().project_inquiries(&workflow.conversation_id).unwrap_or_default();
            // 这回合交给调度的新回答；回合进行中才答的不在里面，留给下回合。
            let shown: Vec<String> = inquiries.iter().filter(|item| item.status == "answered" && !item.seen).map(|item| item.id.clone()).collect();
            let state = json!({"ledger":manager.ledger,"previous_ledger":previous_ledger,"quota":quota,"limits":{"new_sessions":{"max":crate::coordinator_store::temporary_limit(&preference),"opened":temporary.len()}},"usage":usage_overview(&workflow),"capability":capability,"session_reuse":session_reuse,"resource_pools":resource_pools(&workflow,&quota,&providers),"logical_tasks":logical_usage(&workflow,&manager.routes),"sources":compact_sources(&sources),"tasks":compact_tasks(&workflow),"routes":compact_routes(&manager.routes),"available_agents":catalogs,"routing_feedback":feedback,"inquiries":inquiry_state(&inquiries)});
            *self.budget.reference.lock().unwrap() = Some(reference.clone());
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
            // 主动暂停并估了恢复时间：就在这里等（不计入协作时长），到点或用户在群里发话就开下一回合。
            if let Ok(coordinator_policy::Decision { action, pause: Some(pause), ledger, .. }) = &decision {
                if let (true, Some(hours)) = (action == "pause", pause.resume_in_hours) {
                    if let Some(ledger) = ledger.as_deref().filter(|ledger| !ledger.trim().is_empty()) {
                        self.store.lock().unwrap().set_coordination_ledger(id, ledger)?;
                    }
                    if let Ok(decision) = &decision {
                        self.save_plan(id, decision.plan.as_deref());
                    }
                    deadline = self.rest(id, &pause.reason, hours, deadline)?;
                    rejected = 0;
                    feedback = None;
                    continue;
                }
            }
            let applied = decision
                .and_then(|decision| self.apply_coordination(id, &result.id, decision, deadline));
            if applied.is_ok() {
                let _ = self.store.lock().unwrap().mark_inquiries_seen(&shown);
            }
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
        self.save_plan(id, decision.plan.as_deref());
        let replacements = decision
            .routes
            .iter()
            .map(|route| (route.source_id.clone(), route.quote.clone()))
            .collect::<Vec<_>>();
        self.store
            .lock()
            .unwrap()
            .withdraw_coordination_tasks(id, &decision.withdraw, &replacements)?;
        // 调度自己拍板的取舍和要确认的事都挂到问答面板，群里只留一行提醒。
        let asked = self.post_inquiries(id, &decision)?;
        let decided = (asked > 0).then(|| format!("调度在问答面板挂了 {asked} 个新问题，有空时去答；能回退的先按调度的默认做法推进。"));
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
                // 除了这件事已经没活可做：问题挂到问答面板（不可逆、先搁着），协作停下等回答。
                let question = decision.question.ok_or("问题为空")?;
                let _ = attempt;
                let room = self.store.lock().unwrap().workflow(id)?.conversation_id;
                let key = format!("ask:{}", question.prompt.trim().chars().take(50).collect::<String>());
                let ask = crate::project_inquiries::Ask {
                    key: key.clone(),
                    question: question.prompt.trim().chars().take(400).collect(),
                    background: None,
                    options: question.options.into_iter().take(6).collect(),
                    default: None,
                    reversible: false,
                };
                self.store.lock().unwrap().ask_inquiry(&room, Some(id), "coordinator", &ask)?;
                self.store.lock().unwrap().add_system_team_message(id, "调度有一件事要你确认才能继续，已挂到问答面板。")?;
                self.store.lock().unwrap().coordination_status(id, "awaiting_user")?;
                (self.notify)(id);
                loop {
                    self.ensure_active(id, deadline)?;
                    let open = self
                        .store
                        .lock()
                        .unwrap()
                        .project_inquiries(&room)?
                        .into_iter()
                        .any(|inquiry| inquiry.key == key && inquiry.status == "open");
                    if !open {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(500));
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
            "pause" => {
                // 没给恢复时间的暂停：停下协作，已完成的保留，用户补充额度后点「继续」或在群里说。
                let reason = decision.pause.as_ref().map_or("", |pause| pause.reason.trim());
                self.store.lock().unwrap().add_system_team_message(
                    id,
                    &format!("调度主动暂停协作：{}。额度恢复或补充后，在群里说一声或点「继续」就接着做。", reason.trim_end_matches('。')),
                )?;
                self.store.lock().unwrap().coordination_status(id, "paused")?;
                self.store.lock().unwrap().finish_workflow(
                    id,
                    "interrupted",
                    "",
                    Some("调度因额度不足主动暂停，已完成的结果都保留"),
                )?;
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
            // 方案全文存进项目的 .tongxi/方案存档/，长期计划按路径引用，以后的协作也找得到。
            if route.is_some_and(|route| route.purpose == "solution") && !result.output.trim().is_empty() {
                let project = self.store.lock().unwrap().project(&workflow.project_id)?;
                if let Ok(path) = crate::project_plan::archive(Path::new(&project.root), &task.title, &result.output, crate::store::now()) {
                    let _ = self.store.lock().unwrap().add_system_team_message(id, &format!("「{}」的方案全文已存档：{path}", task.title));
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

    /// 软预算：任务跑着时每 15 秒看一次用量，超了预算不打断成员（打断后续做要整段按全价重读），
    /// 而是单独问调度一次拨款：追加、收尾（插话让它做完手头一步、提交、交报告）或叫停。
    /// 问不到（调度撞限额等）就默认追加一半；同一项最多问 3 次，之后默认收尾。
    fn watch_budget(&self, id: &str, task: &Task, done: &std::sync::atomic::AtomicBool, deadline: Instant) {
        let (budget, advice) = {
            let store = self.store.lock().unwrap();
            (store.task_budget(&task.id), store.task_advisor_calls(&task.id) > 0)
        };
        // 没写预算也要盯：成员原地打转时同样问调度（Hermes 回合中途报不出用量，只能靠这个）。
        let mut budget = budget.unwrap_or(u64::MAX);
        let mut asked = 0u32;
        let mut loop_mark = (0usize, 0usize);
        // 已经让它收尾过：它要是不停、接着干，收尾阶段也有上限，超了再问调度（这次可以叫停）。
        let mut wrapped = false;
        loop {
            // 每秒看一眼有没有请教，每 15 秒看一次用量。
            for _ in 0..15 {
                if done.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                if advice {
                    self.serve_advice(id, task, deadline);
                }
                std::thread::sleep(Duration::from_secs(1));
            }
            let Some(attempt) = self.store.lock().unwrap().running_attempt(&task.id) else { continue };
            let live = live_spent(&attempt);
            let over = budget != u64::MAX && live.is_some_and(|spent| spent > budget);
            let stuck = if over { None } else { looping(&attempt, loop_mark) };
            if !over && stuck.is_none() {
                continue;
            }
            let spent = live.unwrap_or(0);
            let can_wrap_up = matches!(crate::project_store::base_agent(&attempt.agent_id), "codex-win" | "claude-win");
            let grant = if asked >= 3 {
                coordinator_policy::Grant {
                    action: if wrapped { "stop" } else if can_wrap_up { "wrap_up" } else { "continue" }.into(),
                    extra_tokens: None,
                    reason: Some(if wrapped { "已经拨过三次，让它收尾也没停" } else { "已经拨过三次" }.into()),
                }
            } else {
                self.ask_grant(id, task, &attempt, (budget, spent, asked), stuck.as_deref(), (can_wrap_up, wrapped), deadline)
                    .unwrap_or_else(|error| coordinator_policy::Grant {
                        action: "continue".into(),
                        extra_tokens: None,
                        reason: Some(format!("没问到调度（{error}），先追加一半")),
                    })
            };
            asked += 1;
            let reason = grant
                .reason
                .as_deref()
                .map(str::trim)
                .filter(|reason| !reason.is_empty())
                .map(|reason| format!("：{reason}"))
                .unwrap_or_default();
            let action = if grant.action == "wrap_up" && !can_wrap_up { "continue" } else { grant.action.as_str() };
            let (budget_label, spent_label) = (if budget == u64::MAX { "未设".to_owned() } else { token_label(budget) }, token_label(spent));
            let why = match &stuck {
                Some(evidence) => format!("疑似原地打转（{evidence}）"),
                None => format!("用量超出预算（{spent_label} / {budget_label}）"),
            };
            loop_mark = (attempt.output.len(), attempt.tool_events.len());
            let note = match action {
                "wrap_up" => {
                    let text = match &stuck {
                        Some(_) => "同席提醒：你最近像是在原地打转（同样的话反复说或工具接连失败）。请停下来，保留已做的改动，交一份进度报告：做了什么、卡在哪、还剩什么。剩下的由调度另行安排。".to_owned(),
                        None => format!("同席提醒：这项任务的用量已超过调度给的预算（预算 {budget_label}，已用 {spent_label}）。请把手头这一步做完、跑和改动相关的测试、提交，然后停下交一份进度报告：做了什么、还剩什么。剩下的由调度另行安排。"),
                    };
                    let _ = self.native.steer_note(&attempt, &text);
                    // 收尾阶段的上限：原预算的三分之一，至少够十次工具调用。
                    let per_call = attempt.usage.as_ref().map_or(0, |usage| usage.context);
                    budget = if budget == u64::MAX { u64::MAX } else { spent + (budget / 3).max(per_call * 10) };
                    wrapped = true;
                    format!("「{}」{why}，调度让它收尾交报告{reason}。", task.title)
                }
                "stop" => {
                    self.budget.stopped.lock().unwrap().insert(
                        task.id.clone(),
                        match &stuck {
                            Some(evidence) => format!("成员原地打转，调度叫停（{evidence}）{reason}；改动留在工作树里未合并，可换更强的模型接手或改说明再派"),
                            None => format!("超出预算（预算 {budget_label}，已用 {spent_label}），调度叫停{reason}；改动留在工作树里未合并，可用同一任务续做、换人接手或撤下"),
                        },
                    );
                    self.native.cancel_attempt(&attempt.id);
                    format!("「{}」{why}，调度叫停{reason}。", task.title)
                }
                _ => {
                    if stuck.is_some() {
                        format!("「{}」{why}，调度让它接着做{reason}。", task.title)
                    } else {
                        // 每次工具调用大约把上下文重读一遍：追加量至少够它再跑 15 次，不然一两分钟后又得来问。
                        let per_call = attempt.usage.as_ref().map_or(0, |usage| usage.context);
                        let extra = grant.extra_tokens.filter(|extra| *extra > 0).unwrap_or(budget / 2).max(budget / 5).max(per_call * 15);
                        budget = spent + extra;
                        format!("「{}」{why}，调度追加 {}{reason}。", task.title, token_label(extra))
                    }
                }
            };
            let _ = self.store.lock().unwrap().add_system_team_message(id, &note);
            (self.notify)(id);
            if action == "stop" {
                return;
            }
        }
    }

    /// 把调度的 decisions（先按选择推进的取舍，可回退）和 inquiries 挂到问答面板，撤下要撤的；返回新挂出的个数。
    fn post_inquiries(&self, id: &str, decision: &Decision) -> Result<usize> {
        let room = self.store.lock().unwrap().workflow(id)?.conversation_id;
        let decided = decision.decisions.iter().map(|item| crate::project_inquiries::Ask {
            key: format!("decision:{}", item.question.trim().chars().take(50).collect::<String>()),
            question: item.question.trim().chars().take(400).collect(),
            background: item.reason.clone(),
            options: vec![],
            default: Some(item.choice.trim().chars().take(300).collect()),
            reversible: true,
        });
        let mut asked = 0;
        let store = self.store.lock().unwrap();
        for ask in decided.chain(decision.inquiries.iter().cloned()) {
            if store.ask_inquiry(&room, Some(id), "coordinator", &ask)? {
                asked += 1;
            }
        }
        for key in &decision.withdraw_inquiries {
            store.withdraw_inquiry(&room, key)?;
        }
        Ok(asked)
    }

    /// 调度交了新版长期计划就写进项目的 .tongxi/计划.md；写不进去只记一条群消息，不影响调度。
    fn save_plan(&self, id: &str, plan: Option<&str>) {
        let Some(plan) = plan.filter(|plan| !plan.trim().is_empty()) else { return };
        let workflow = self.store.lock().unwrap().workflow(id);
        let project = workflow.and_then(|workflow| self.store.lock().unwrap().project(&workflow.project_id));
        if let Err(error) = project.and_then(|project| crate::project_plan::write(Path::new(&project.root), plan)) {
            let _ = self.store.lock().unwrap().add_system_team_message(id, &format!("长期计划没能写进文件：{error}"));
        }
    }

    /// 回答这项任务排着的请教：交给调度（复用缓存的系统提示），回答写回去，成员那边的工具调用就返回了。
    /// 调度建议换强模型时，回答里让成员保留改动、写「需要升档」交回。
    fn serve_advice(&self, id: &str, task: &Task, deadline: Instant) {
        let Some((request, question, context)) = self.store.lock().unwrap().pending_advice(&task.id) else { return };
        let attempt = self.store.lock().unwrap().running_attempt(&task.id);
        let answered = attempt.ok_or_else(|| "任务已结束".to_owned()).and_then(|attempt| {
            let clip = |text: &str, limit: usize| text.chars().take(limit).collect::<String>();
            let request = json!({"task_id":task.id,"title":task.title,"agent_id":attempt.agent_id,"model":attempt.model,"effort":attempt.reasoning_effort,
                "instructions":clip(&task.instructions, 2000),"question":question,"context":clip(&context, 12_000)});
            let output = self.ask_coordinator(id, json!({"advice_request":request}), deadline)?;
            coordinator_policy::parse_advice(&output)
        });
        let mut store = self.store.lock().unwrap();
        match answered {
            Ok(advice) => {
                let answer = if advice.escalate {
                    format!("{}\n\n顾问判断这项超出当前模型的能力，建议换更强的模型接手：请保留已做的改动，不要再硬做，在交付摘要里单独一行写「需要升档：卡在哪、试过什么」，然后结束本轮。", advice.answer.trim())
                } else {
                    advice.answer.trim().to_owned()
                };
                let _ = store.answer_advice(&request, "answered", &answer);
                let _ = store.add_system_team_message(id, &format!("「{}」向调度请教了一次{}。", task.title, if advice.escalate { "，调度建议换强模型接手" } else { "" }));
            }
            Err(error) => {
                let _ = store.answer_advice(&request, "failed", &format!("顾问没能回答（{error}）：按自己的判断继续"));
            }
        }
        drop(store);
        (self.notify)(id);
    }

    /// 单独问调度一件事（拨款、请教）：复用最近一回合的参考资料，调度的系统提示能命中缓存。返回调度的原始回复。
    fn ask_coordinator(&self, id: &str, state: Value, deadline: Instant) -> Result<String> {
        let _turn = self.budget.asking.lock().unwrap();
        let manager = self.store.lock().unwrap().coordination(id)?.ok_or("调度记录缺失")?;
        let mut state = state;
        state["ledger"] = json!(manager.ledger);
        let reference = self.budget.reference.lock().unwrap().clone().unwrap_or_else(|| "{}".into());
        let body = format!("参考资料（JSON，几回合内基本不变）：\n{reference}\n\n本回合状态（JSON）：\n{state}");
        let input = if matches!(crate::project_store::base_agent(&manager.coordinator.agent), "claude-win" | "codex-win") {
            body
        } else {
            format!("{}\n\n{body}", coordinator_policy::MANAGER_INSTRUCTION)
        };
        let record = self.store.lock().unwrap().begin_coordinated_attempt(
            id,
            None,
            &manager.coordinator.agent,
            manager.coordinator.model.clone(),
            manager.coordinator.effort.clone(),
        )?;
        let root = crate::coordinator_clients::manager_directory(&self.store.lock().unwrap().path, id, &manager.coordinator.agent)?;
        let result = self.native.run(record, &root, &input, deadline)?;
        if result.status != "completed" {
            return Err(result.error.unwrap_or_else(|| "调度回合未完成".into()));
        }
        Ok(result.output)
    }

    /// 单独问调度一次拨款。复用最近一回合的参考资料，调度的系统提示能命中缓存，一次只花几千 token。
    /// usage：（当前预算, 已用, 已问过几次）。
    fn ask_grant(
        &self,
        id: &str,
        task: &Task,
        attempt: &crate::project_store::Attempt,
        usage: (u64, u64, u32),
        stuck: Option<&str>,
        (can_wrap_up, wrapped): (bool, bool),
        deadline: Instant,
    ) -> Result<coordinator_policy::Grant> {
        let (budget, spent, asked) = usage;
        let tail = |text: &str, limit: usize| {
            let chars: Vec<char> = text.chars().collect();
            chars[chars.len().saturating_sub(limit)..].iter().collect::<String>()
        };
        let tools: Vec<&str> = attempt.tool_events.iter().rev().take(12).map(|event| event.title.as_str()).collect();
        let minutes = (crate::store::now() - attempt.created_at).max(0) / 60_000;
        let request = json!({"task_id":task.id,"title":task.title,"agent_id":attempt.agent_id,"model":attempt.model,"effort":attempt.reasoning_effort,
            "trigger":if stuck.is_some() { "looping" } else { "budget" },"loop_evidence":stuck,
            "budget_tokens":(budget != u64::MAX).then_some(budget),"spent_tokens":(spent > 0).then_some(spent),"grants_so_far":asked,"running_minutes":minutes,
            "context_tokens":attempt.usage.as_ref().map(|usage| usage.context),"tokens_per_call":attempt.usage.as_ref().map(|usage| usage.context),"can_wrap_up":can_wrap_up,"already_told_to_wrap_up":wrapped,
            "instructions":tail(&task.instructions, 1500),"recent_output":tail(&attempt.output, 1500),"recent_tools":tools});
        let output = self.ask_coordinator(id, json!({"grant_request":request,"quota":crate::quota::overview(&self.codex)}), deadline)?;
        coordinator_policy::parse_grant(&output)
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
                let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let (watched, watching) = (task.clone(), done.clone());
                scope.spawn(move || {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        self.watch_budget(id, &watched, &watching, deadline)
                    }));
                });
                scope.spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        self.run_coordinated_task(id, &task, deadline)
                    }))
                    .unwrap_or_else(|_| Err("成员运行线程异常".into()));
                    done.store(true, std::sync::atomic::Ordering::SeqCst);
                    // 软预算叫停的：不管成员怎么结束的，都记成超出预算的失败，改动留在工作树里不合并。
                    let stopped = self.budget.stopped.lock().unwrap().remove(&task.id);
                    // 成员交代做不了、要换强模型：同样不合并，改动留在工作树里给接手的人。
                    let stopped = stopped.or_else(|| match &result {
                        Ok(attempt) if attempt.status == "completed" => coordinator_policy::escalation(&attempt.output)
                            .map(|reason| format!("成员请求升档：{}", reason.chars().take(200).collect::<String>())),
                        _ => None,
                    });
                    let result = match (result, stopped) {
                        (Ok(mut attempt), Some(error)) => {
                            attempt.status = "failed".into();
                            attempt.error = Some(error);
                            let _ = self.store.lock().unwrap().checkpoint_attempt(&attempt);
                            Ok(attempt)
                        }
                        (result, _) => result,
                    };
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
        if route.advisor_calls > 0 {
            prompt["advisor"] = json!(format!(
                "本任务开放请教：遇到拿不准的关键决定（设计取舍、定位不到的疑难问题、关键处怎么做）时，用 hub_ask_advisor 向调度请教，最多 {} 次。写清问题和你试过什么，context 里附上相关代码片段或报错；等回答回来再接着做。小事别用。",
                route.advisor_calls
            ));
        }
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
    fn repeated_lines_or_failing_tools_count_as_looping_only_after_the_mark() {
        let mut attempt = sample_attempt("hermes-win", "flash:free", "running", None);
        attempt.output = (0..5).map(|_| "Now I have the full picture. Let me implement the fix.").collect::<Vec<_>>().join("\n\n");
        assert!(super::looping(&attempt, (0, 0)).is_some_and(|evidence| evidence.contains("5 次")));
        // 问过调度之后，只看新内容。
        assert!(super::looping(&attempt, (attempt.output.len(), 0)).is_none());
        attempt.output = "读文件。\n改了 a.rs，跑测试。".into();
        assert!(super::looping(&attempt, (0, 0)).is_none());
        let event = |status: &str| crate::store::RunToolEvent { event_id: "e".into(), kind: "edit".into(), title: "hub_edit: notify.py".into(), input: None, output: None, status: status.into(), created_at: 0 };
        attempt.tool_events = vec![event("completed"), event("failed"), event("failed"), event("failed"), event("failed")];
        assert!(super::looping(&attempt, (0, 0)).is_some_and(|evidence| evidence.contains("工具调用全部失败")));
        assert!(super::looping(&attempt, (0, 3)).is_none());
    }

    #[test]
    fn live_spending_comes_from_upstream_usage_not_from_members() {
        use crate::project_store::Usage;
        // Codex：实时报的精确累计。
        let codex = sample_attempt("codex-win-2", "sol", "running", Some(Usage { input: 900, output: 100, ..Default::default() }));
        assert_eq!(super::live_spent(&codex), Some(1000));
        // Claude：桥接按每次模型调用累加，先放在上下文占位里。
        let claude = sample_attempt("claude-win", "opus", "running", Some(Usage { input: 5000, context: 300, context_only: true, ..Default::default() }));
        assert_eq!(super::live_spent(&claude), Some(5000));
        // Hermes 只报上下文占用：回合中途不知道累计，不拿它当用量。
        let hermes = sample_attempt("hermes-win", "flash", "running", Some(Usage { input: 80_000, context_only: true, ..Default::default() }));
        assert_eq!(super::live_spent(&hermes), None);
        assert_eq!(super::token_label(2_345_678), "2.3M");
    }

    #[test]
    fn failed_attempts_are_split_by_cause_and_later_completed_tasks_are_marked_kept() {
        use crate::project_store::Usage;
        let used = |n: u64| Some(Usage { input: n, ..Default::default() });
        let mut quota = sample_attempt("codex-win", "sol", "failed", used(700));
        quota.error = Some("Codex 项目运行未正常完成（额度限制（额度预计 10月21日 01:12 重置））".into());
        quota.task_id = Some("t1".into());
        let mut key = sample_attempt("hermes-win", "flash", "failed", used(20));
        key.error = Some("项目成员的模型服务拒绝请求（Key 无效或余额不足），这条线路暂时不能用，未完成本阶段".into());
        let stopped = sample_attempt("dsh-win", "flash", "interrupted", used(5));
        let mut workflow = sample_workflow(vec![quota, key, stopped, sample_attempt("dsh-win", "flash", "completed", used(900))]);
        workflow.tasks = vec![crate::project_store::Task {
            id: "t1".into(), workflow_id: "w".into(), position: 0, title: "接手做完".into(), agent_id: "dsh-win".into(),
            instructions: String::new(), files: vec![], depends_on: vec![], status: "completed".into(), output: String::new(),
            error: None, model: None, effort: None, worktree: None, branch: None,
        }];
        let overview = super::usage_overview(&workflow);
        let failed = &overview["failed_tokens"];
        assert_eq!((failed["by_cause"]["quota"].as_u64(), failed["by_cause"]["provider"].as_u64(), failed["by_cause"]["interrupted"].as_u64()), (Some(700), Some(20), Some(5)));
        assert_eq!(failed["task_later_completed"], 700);
        assert_eq!(crate::capability::failure_cause("completed", None), None);
    }

    #[test]
    fn quota_windows_report_tokens_spent_since_window_start() {
        let mut quota = serde_json::json!({"codex-win":{"five_hour":{"used_percent":100,"resets_in_hours":4.5},"weekly":{"used_percent":49}}});
        let asked = std::cell::RefCell::new(Vec::new());
        super::window_tokens(&mut quota, |base, since| {
            asked.borrow_mut().push((base.to_owned(), crate::store::now() - since));
            Some(16_500_000)
        });
        assert_eq!(quota["codex-win"]["five_hour"]["window_tokens"], 16_500_000);
        // 已用满的窗口不给剩余估算。
        assert!(quota["codex-win"]["five_hour"].get("estimated_tokens_left").is_none());
        let mut half = serde_json::json!({"claude-win":{"five_hour":{"used_percent":40,"resets_in_hours":2.0}}});
        super::window_tokens(&mut half, |_, _| Some(4_000_000));
        assert_eq!(half["claude-win"]["five_hour"]["estimated_tokens_left"], 6_000_000);
        // 没给距重置时间的窗口推不出起点，不填。
        assert!(quota["codex-win"]["weekly"].get("window_tokens").is_none());
        let (base, back) = asked.borrow()[0].clone();
        assert_eq!(base, "codex-win");
        assert!((1_799_000..1_801_000).contains(&back), "{back}");
    }

    #[test]
    fn resource_pools_split_subscriptions_and_provider_lines_without_inventing_costs() {
        use crate::project_store::Usage;
        let usage = |n: u64, cost: Option<f64>, kind: Option<&str>| Some(Usage { input: n, cost_usd: cost, cost_kind: kind.map(String::from), ..Default::default() });
        let mut failed = sample_attempt("dsh-win-2", "flash", "failed", Some(Usage { input: 50, context_only: true, ..Default::default() }));
        failed.error = Some("原生成员执行失败（协议代码 -32603）".into());
        let workflow = sample_workflow(vec![
            sample_attempt("claude-win", "sonnet", "completed", usage(100, Some(0.5), Some("api_estimate"))),
            sample_attempt("dsh-win", "flash", "completed", usage(200, Some(0.02), Some("provider"))),
            failed,
            sample_attempt("hermes-win", "flash", "completed", None),
        ]);
        let providers = [(("dsh-win".to_owned(), "flash".to_owned()), "goat".to_owned())].into_iter().collect();
        let quota = serde_json::json!({"claude-win":{"five_hour":{"used_percent":40}}});
        let pools = super::resource_pools(&workflow, &quota, &providers);
        assert_eq!(pools.len(), 3);
        assert_eq!((pools[0]["pool"].as_str(), pools[0]["kind"].as_str()), (Some("claude-subscription"), Some("subscription")));
        assert_eq!(pools[0]["quota"]["five_hour"]["used_percent"], 40);
        // 只统计 token，不折成金额。
        assert!(pools[0].get("api_estimate_usd").is_none() && pools[1].get("provider_cost_usd").is_none());
        // 额外席位与本体同一条线路，归同一池；只报上下文占用的那次记未知。
        assert_eq!((pools[1]["pool"].as_str(), pools[1]["tokens"].as_u64(), pools[1]["unknown_attempts"].as_u64()), (Some("dsh-win:goat"), Some(200), Some(1)));
        // 这条线路最近一次报错：标出来，调度别再往这里派。
        assert!(pools[1]["last_error"].as_str().is_some_and(|error| error.contains("-32603")));
        assert!(pools[0].get("last_error").is_none());
        // Hermes 线路未知、也不报用量：池类型与额度都是 unknown，不当零成本。
        assert_eq!((pools[2]["kind"].as_str(), pools[2]["quota"].as_str(), pools[2]["unknown_attempts"].as_u64()), (Some("unknown"), Some("unknown"), Some(1)));
    }
}
