//! Real file and state transitions in isolated fixtures, never user projects.
use crate::project_store::{self, *};
use crate::project_tools::Broker;
use crate::store::Store;
use serde_json::json;
use std::path::PathBuf;
use uuid::Uuid;

#[test]
fn supplementary_user_assignment_updates_roles_during_planning_and_overrides_the_proposal() {
    let mut f = Fixture::new();
    let workflow = f.queued();
    assert!(f.store.acquire_project(&workflow.id).unwrap());
    let mut roles = Roles::defaults();
    roles.implement.agent = "hermes-win".into();
    roles.implement.model = Some("old-hermes-route".into());
    f.store
        .set_workflow_roles(&workflow.id, Some(&serde_json::to_string(&roles).unwrap()))
        .unwrap();
    let mut planner = f
        .store
        .begin_attempt(&workflow.id, None, "codex-win", "plan", None, None)
        .unwrap();
    let posted = f
        .store
        .post_user_team_message(&f.room, "由codex出方案，dsh执行，Hermes做好验收和文件整理")
        .unwrap();
    assert_eq!(posted.workflow_id.as_deref(), Some(workflow.id.as_str()));
    let updated = f.store.workflow(&workflow.id).unwrap();
    let roles = roles_of(&updated).unwrap();
    assert_eq!(roles.implement.agent, "dsh-win");
    assert!(roles.implement.model.is_none());
    let mut proposed = plan(&["main.py"]);
    proposed.tasks[0].agent_id = "hermes-win".into();
    planner.output = serde_json::to_string(&proposed).unwrap();
    complete(&mut f.store, &planner);
    let planned = f.store.save_plan(&workflow.id, &proposed).unwrap();
    let chosen = f
        .store
        .project_explicit_executor(&planned)
        .unwrap()
        .unwrap();
    assert_eq!(chosen, "dsh-win");
    f.store
        .set_task_agent(&planned.tasks[0].id, &chosen)
        .unwrap();
    f.store.begin_execution(&workflow.id).unwrap();
    let actual = f
        .store
        .begin_attempt(
            &workflow.id,
            Some(&planned.tasks[0].id),
            &chosen,
            "implement",
            None,
            None,
        )
        .unwrap();
    assert_eq!(actual.agent_id, "dsh-win");
    assert_eq!(f.store.attempts(&workflow.id).unwrap().len(), 2);
}

#[test]
fn user_answers_can_correct_assignment_without_importing_other_project_messages() {
    let mut f = Fixture::new();
    let job = f.queued();
    f.store.acquire_project(&job.id).unwrap();
    let planner = f
        .store
        .begin_attempt(&job.id, None, "codex-win", "plan", None, None)
        .unwrap();
    let id = crate::project_questions::post(&f.store.path, &planner.id, "请描述改进目标", vec![])
        .unwrap();
    f.store
        .answer_project_question(
            &f.room,
            &id,
            &Uuid::new_v4().to_string(),
            "DSH执行，修复main.py，Hermes验收",
        )
        .unwrap();
    let updates = f.store.project_user_updates(&job.id).unwrap();
    assert_eq!(updates.len(), 1);
    assert!(updates[0].id.starts_with("answer:"));
    assert_eq!(
        f.store
            .project_explicit_executor(&f.store.workflow(&job.id).unwrap())
            .unwrap()
            .as_deref(),
        Some("dsh-win")
    );
    f.store
        .post_user_team_message(&f.room, "实现交给Hermes")
        .unwrap();
    assert_eq!(
        f.store
            .project_explicit_executor(&f.store.workflow(&job.id).unwrap())
            .unwrap()
            .as_deref(),
        Some("hermes-win")
    );
    assert!(f
        .store
        .project_user_updates("another-workflow")
        .unwrap()
        .is_empty());
}

#[test]
fn role_directives_bind_each_marker_to_its_agent_in_a_single_sentence() {
    let mut roles = Roles::defaults();
    crate::project_directives::apply("Hermes出方案 Codex实现 DSH验收", &mut roles);
    assert_eq!(roles.plan.agent, "hermes-win");
    assert_eq!(roles.implement.agent, "codex-win");
    assert_eq!(roles.review.agent, "dsh-win");
    crate::project_directives::apply("规划交给Codex，实现交给DSH，验收由Hermes完成", &mut roles);
    assert_eq!(roles.plan.agent, "codex-win");
    assert_eq!(roles.implement.agent, "dsh-win");
    assert_eq!(roles.review.agent, "hermes-win");
    let before = roles.clone();
    crate::project_directives::apply("Hermes的实现现在是什么状态？Codex验收做完了？", &mut roles);
    assert_eq!(roles, before);
}

#[test]
fn members_can_discuss_direction_before_planner_without_becoming_the_plan() {
    let mut f = Fixture::new();
    let workflow = f.queued();
    assert!(f.store.acquire_project(&workflow.id).unwrap());
    assert!(f
        .store
        .begin_attempt(&workflow.id, None, "albion-wsl", "discussion", None, None)
        .is_err());
    for agent in ["dsh-win", "hermes-win"] {
        let mut attempt = f
            .store
            .begin_attempt(&workflow.id, None, agent, "discussion", None, None)
            .unwrap();
        assert_eq!(attempt.activity.as_deref(), Some("discussion"));
        attempt.output = "保留已完成内容，调整未完成任务。".into();
        complete(&mut f.store, &attempt);
    }
    assert!(f
        .store
        .save_plan(&workflow.id, &plan(&["double.py"]))
        .is_err());
    let planned = f.plan_with(&workflow, &plan(&["double.py"]));
    assert_eq!(planned.tasks.len(), 1);
    assert_eq!(planned.attempts.len(), 3);
}

#[test]
fn direction_change_preserves_completed_branch_but_never_merges_interrupted_work() {
    let mut f = Fixture::new();
    crate::project_worktree::initialize_safe_repository(&f.root).unwrap();
    let mut workflow = f.planned(&["identity.py"]);
    let parent = f.base.join("worktrees");
    let finished =
        crate::project_worktree::create(&f.root, &parent, "finished", "hub/finished").unwrap();
    std::fs::write(
        finished.join("identity.py"),
        "def identity(value): return value\n",
    )
    .unwrap();
    crate::project_worktree::commit(&finished, "completed task", &["identity.py".into()]).unwrap();
    let partial =
        crate::project_worktree::create(&f.root, &parent, "partial", "hub/partial").unwrap();
    std::fs::write(partial.join("cube.py"), "partial code\n").unwrap();
    crate::project_worktree::commit(&partial, "partial task", &["cube.py".into()]).unwrap();
    workflow.tasks[0].status = "completed".into();
    workflow.tasks[0].branch = Some("hub/finished".into());
    let mut other = workflow.tasks[0].clone();
    other.status = "interrupted".into();
    other.branch = Some("hub/partial".into());
    workflow.tasks.push(other);
    crate::projects::preserve_completed_work(&f.root, &workflow).unwrap();
    crate::projects::preserve_completed_work(&f.root, &workflow).unwrap();
    assert_eq!(
        std::fs::read_to_string(f.root.join("identity.py"))
            .unwrap()
            .replace("\r\n", "\n"),
        "def identity(value): return value\n"
    );
    assert!(!f.root.join("cube.py").exists());
    assert!(partial.join("cube.py").exists());
    crate::project_worktree::remove(&f.root, &finished).unwrap();
    crate::project_worktree::remove(&f.root, &partial).unwrap();
}

#[test]
fn user_question_is_public_and_answered_atomically_during_live_workflow() {
    let mut f = Fixture::new();
    let attempt = f.implement(&["main.py"]);
    let id = crate::project_questions::post(
        &f.store.path,
        &attempt.id,
        "选择输出格式？",
        vec!["JSON".into(), "文本".into()],
    )
    .unwrap();
    assert_eq!(f.store.team_messages(&f.room).unwrap()[0].id, id);
    assert_eq!(
        f.store.project_questions(&f.room).unwrap()[0].status,
        "pending"
    );
    let wrong = f
        .store
        .create("其他群", "group", &["codex-win".into(), "dsh-win".into()])
        .unwrap();
    assert!(f
        .store
        .answer_project_question(&wrong.id, &id, &Uuid::new_v4().to_string(), "文本")
        .is_err());
    let message = Uuid::new_v4().to_string();
    f.store
        .answer_project_question(&f.room, &id, &message, "JSON")
        .unwrap();
    let question = &f.store.project_questions(&f.room).unwrap()[0];
    assert_eq!(question.answer.as_deref(), Some("JSON"));
    assert_eq!(question.status, "answered");
    assert!(f
        .store
        .detail(&f.room)
        .unwrap()
        .messages
        .iter()
        .any(|row| row.id == message && row.status == "delivered"));
    let before = f.store.detail(&f.room).unwrap().messages.len();
    assert!(f
        .store
        .answer_project_question(&f.room, &id, &Uuid::new_v4().to_string(), "再次回答")
        .is_err());
    assert_eq!(f.store.detail(&f.room).unwrap().messages.len(), before);
}

#[test]
fn waiting_questions_exclude_overlapping_human_waits_and_disappear_with_history() {
    let mut f = Fixture::new();
    let attempt = f.implement(&["main.py"]);
    let a = crate::project_questions::post(&f.store.path, &attempt.id, "问题一", vec![]).unwrap();
    let b = crate::project_questions::post(&f.store.path, &attempt.id, "问题二", vec![]).unwrap();
    f.store.connection.execute("UPDATE project_questions SET created_at=1000,answered_at=5000,status='answered' WHERE id=?1", [&a]).unwrap();
    f.store.connection.execute("UPDATE project_questions SET created_at=3000,answered_at=7000,status='cancelled' WHERE id=?1", [&b]).unwrap();
    assert_eq!(
        f.store
            .project_wait_duration(&attempt.workflow_id)
            .as_millis(),
        6000
    );
    f.store
        .connection
        .execute(
            "UPDATE workflows SET status='failed' WHERE id=?1",
            [&attempt.workflow_id],
        )
        .unwrap();
    assert!(
        crate::project_questions::post(&f.store.path, &attempt.id, "旧任务问题", vec![]).is_err()
    );
    f.store
        .remove_conversation_item(&f.room, "workflow", &attempt.workflow_id)
        .unwrap();
    assert!(f.store.project_questions(&f.room).unwrap().is_empty());
    assert!(f.store.team_messages(&f.room).unwrap().is_empty());
}

#[test]
fn a_conflicting_answer_message_id_rolls_back_question_transition() {
    let mut f = Fixture::new();
    let attempt = f.implement(&["main.py"]);
    let id =
        crate::project_questions::post(&f.store.path, &attempt.id, "需要确认", vec![]).unwrap();
    let message = f
        .store
        .workflow(&attempt.workflow_id)
        .unwrap()
        .user_message_id;
    assert!(f
        .store
        .answer_project_question(&f.room, &id, &message, "回答")
        .is_err());
    assert_eq!(
        f.store.project_questions(&f.room).unwrap()[0].status,
        "pending"
    );
}

#[test]
fn large_source_review_uses_paths_and_native_tools_instead_of_a_snapshot_limit() {
    let mut f = Fixture::new();
    let workflow = f.planned(&["large.py"]);
    std::fs::write(
        f.root.join("large.py"),
        format!("{}\ndef result(): return 42", "# filler\n".repeat(32_000)),
    )
    .unwrap();
    let project = f.store.project(&f.project).unwrap();
    let prompt = crate::projects::review_prompt(&workflow, &project, "hermes-win", None);
    assert_eq!(prompt["source_paths"], json!(["large.py"]));
    assert!(prompt.get("sources").is_none());
    assert!(!prompt.to_string().contains("# filler"));
    assert!(prompt.to_string().len() < 10_000);
    let recovery = crate::projects::review_prompt(&workflow, &project, "hermes-win", Some(0));
    assert_eq!(recovery["review_mode"], "context_recovery");
    assert_eq!(recovery["task_results"].as_array().unwrap().len(), 1);
}

#[test]
fn deleting_history_removes_plan_tasks_attempts_and_messages_without_touching_source() {
    let mut f = Fixture::new();
    let workflow = f.planned(&["main.py"]);
    std::fs::write(f.root.join("main.py"), "unchanged source").unwrap();
    let attempt = f
        .store
        .begin_attempt(
            &workflow.id,
            Some(&workflow.tasks[0].id),
            "dsh-win",
            "implement",
            None,
            None,
        )
        .unwrap();
    assert!(f
        .store
        .remove_conversation_item(&f.room, "workflow", &workflow.id)
        .is_err());
    f.store
        .connection
        .execute(
            "UPDATE workflows SET status='failed' WHERE id=?1",
            [&workflow.id],
        )
        .unwrap();
    let removed = f
        .store
        .remove_conversation_item(&f.room, "workflow", &workflow.id)
        .unwrap();
    assert_eq!(removed.workflow_id.as_deref(), Some(workflow.id.as_str()));
    assert!(f.store.workflow(&workflow.id).is_err());
    assert!(f.store.attempt(&attempt.id).is_err());
    assert!(f.store.detail(&f.room).unwrap().messages.is_empty());
    assert_eq!(
        std::fs::read_to_string(f.root.join("main.py")).unwrap(),
        "unchanged source"
    );
}

struct Fixture {
    store: Store,
    base: PathBuf,
    root: PathBuf,
    room: String,
    project: String,
}
impl Fixture {
    fn new() -> Self {
        let base = std::env::temp_dir().join(format!("agent-hub-project-test-{}", Uuid::new_v4()));
        let root = base.join("project");
        std::fs::create_dir_all(&root).unwrap();
        let mut store = Store::open(&base.join("data/hub.db")).unwrap();
        let room = store
            .create(
                "项目群",
                "group",
                &["codex-win".into(), "hermes-win".into(), "dsh-win".into()],
            )
            .unwrap()
            .id;
        let project = store
            .register_project("隔离项目", root.to_str().unwrap())
            .unwrap()
            .id;
        store.bind_project(&room, Some(&project)).unwrap();
        Self {
            store,
            base,
            root,
            room,
            project,
        }
    }
    fn queued(&mut self) -> Workflow {
        self.store
            .start_workflow(&self.room, &Uuid::new_v4().to_string(), "实现隔离测试需求")
            .unwrap()
            .0
    }
    fn planned(&mut self, files: &[&str]) -> Workflow {
        let workflow = self.queued();
        assert!(self.store.acquire_project(&workflow.id).unwrap());
        let planned = self.plan_with(&workflow, &plan(files));
        self.store.begin_execution(&planned.id).unwrap()
    }
    fn plan_with(&mut self, workflow: &Workflow, proposed: &Plan) -> Workflow {
        let mut proposed = proposed.clone();
        let current = self.store.workflow(&workflow.id).unwrap();
        let executor = project_store::roles_of(&current).unwrap().implement.agent;
        project_store::bind_plan_to_executor(&mut proposed, &executor).unwrap();
        let mut codex = self
            .store
            .begin_attempt(&workflow.id, None, "codex-win", "plan", None, None)
            .unwrap();
        codex.output = serde_json::to_string(&proposed).unwrap();
        complete(&mut self.store, &codex);
        self.store.save_plan(&workflow.id, &proposed).unwrap()
    }
    fn implement(&mut self, files: &[&str]) -> Attempt {
        let workflow = self.planned(files);
        self.store
            .begin_attempt(
                &workflow.id,
                Some(&workflow.tasks[0].id),
                "dsh-win",
                "implement",
                Some("model".into()),
                Some("low".into()),
            )
            .unwrap()
    }
    fn broker(&self, attempt: &Attempt) -> Broker {
        Broker::open(&self.store.path, &attempt.id).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        // Close SQLite before removal; no computed path can escape this fixture.
        let replacement = Store::initialize(
            rusqlite::Connection::open_in_memory().unwrap(),
            PathBuf::from(":memory:"),
        )
        .unwrap();
        let store = std::mem::replace(&mut self.store, replacement);
        drop(store);
        assert!(self
            .base
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("agent-hub-project-test-"));
        assert_eq!(self.base.parent().unwrap(), std::env::temp_dir());
        std::fs::remove_dir_all(&self.base).unwrap();
    }
}
fn plan(files: &[&str]) -> Plan {
    Plan {
        summary: "执行一项明确的改动".into(),
        tasks: vec![PlannedTask {
            title: "实现".into(),
            agent_id: "dsh-win".into(),
            instructions: "按需求修改授权文件".into(),
            files: files.iter().map(|s| s.to_string()).collect(),
            depends_on: vec![],
            execution: None,
        }],
    }
}
fn complete(store: &mut Store, attempt: &Attempt) {
    let mut attempt = attempt.clone();
    attempt.status = "completed".into();
    store.checkpoint_attempt(&attempt).unwrap();
}

#[test]
fn project_plan_rejects_untrusted_roles_protected_files_and_forward_dependencies() {
    let good = plan(&["src/main.py"]);
    assert_eq!(
        parse_plan(&serde_json::to_string(&good).unwrap()).unwrap(),
        good
    );
    assert!(parse_plan(&format!(
        "```json\n{}\n```",
        serde_json::to_string(&good).unwrap()
    ))
    .is_ok());
    for agent in ["albion-wsl", "unknown"] {
        let mut p = good.clone();
        p.tasks[0].agent_id = agent.into();
        assert!(parse_plan(&serde_json::to_string(&p).unwrap()).is_err());
    }
    let mut hermes_task = good.clone();
    hermes_task.tasks[0].agent_id = "hermes-win".into();
    assert!(parse_plan(&serde_json::to_string(&hermes_task).unwrap()).is_ok());
    for path in [
        "../escape.py",
        "C:/outside.txt",
        ".env",
        ".env.local",
        "dir/auth.json",
        ".git/config",
        "NUL.txt",
        "a:stream",
        "folder/../file",
        "folder/file.",
        "/outside",
        "*.py",
    ] {
        assert!(protected_relative(path).is_err(), "{path}");
    }
    let mut bad = good.clone();
    bad.tasks[0].depends_on = vec![0];
    assert!(parse_plan(&serde_json::to_string(&bad).unwrap()).is_err());
    let mut bad = good;
    bad.tasks[0].files.clear();
    assert!(parse_plan(&serde_json::to_string(&bad).unwrap()).is_err());
    assert!(parse_plan("{\"summary\":\"a\",\"tasks\":[],\"run_shell\":true}").is_err());
}

#[test]
fn project_group_can_answer_in_the_same_workspace_without_fabricating_an_implementation_task() {
    let mut f = Fixture::new();
    let workflow = f.queued();
    assert!(f.store.acquire_project(&workflow.id).unwrap());
    let reply = Plan {
        summary: "这个问题的答案是：讨论和实施现在共用同一个项目群。".into(),
        tasks: vec![],
    };
    assert_eq!(
        parse_plan(&serde_json::to_string(&reply).unwrap()).unwrap(),
        reply
    );
    let mut planning = f
        .store
        .begin_attempt(&workflow.id, None, "codex-win", "plan", None, None)
        .unwrap();
    planning.output = serde_json::to_string(&reply).unwrap();
    complete(&mut f.store, &planning);
    let saved = f.store.save_plan(&workflow.id, &reply).unwrap();
    assert!(saved.tasks.is_empty());
    let finished = f
        .store
        .finish_workflow(&workflow.id, "completed", &reply.summary, None)
        .unwrap();
    assert_eq!(finished.status, "completed");
    assert_eq!(finished.summary, reply.summary);
    assert!(finished.tasks.is_empty());
}

#[test]
fn project_plan_normalizes_obvious_one_based_dependencies_and_keeps_zero_based_ones() {
    let first = plan(&["src/first.rs"]).tasks.remove(0);
    let mut second = plan(&["src/second.rs"]).tasks.remove(0);
    second.title = "第二项".into();
    let mut value = serde_json::to_value(Plan {
        summary: "两项串行任务".into(),
        tasks: vec![first.clone(), second.clone()],
    })
    .unwrap();

    value["tasks"][1]["depends_on"] = serde_json::json!([1]);
    let normalized = parse_plan(&value.to_string()).unwrap();
    assert_eq!(normalized.tasks[1].depends_on, [0]);

    value["tasks"][1]["depends_on"] = serde_json::json!([0]);
    let canonical = parse_plan(&value.to_string()).unwrap();
    assert_eq!(canonical.tasks[1].depends_on, [0]);

    value["tasks"][1]["depends_on"] = serde_json::json!([2]);
    assert!(parse_plan(&value.to_string()).is_err());
}

#[test]
fn failed_planning_can_resume_from_saved_plan_output_without_another_plan_attempt() {
    let mut f = Fixture::new();
    let workflow = f.queued();
    assert!(f.store.acquire_project(&workflow.id).unwrap());
    let proposed = plan(&["src/retry.rs"]);
    let mut planning = f
        .store
        .begin_attempt(&workflow.id, None, "codex-win", "plan", None, None)
        .unwrap();
    planning.output = serde_json::to_string(&proposed).unwrap();
    complete(&mut f.store, &planning);
    f.store
        .finish_workflow(&workflow.id, "failed", "", Some("plan validation"))
        .unwrap();

    let resumed = f.store.resume_workflow(&workflow.id, &proposed).unwrap();
    assert_eq!(resumed.status, "planning");
    assert_eq!(resumed.plan, Some(proposed));
    assert_eq!(resumed.tasks.len(), 1);
    assert_eq!(
        resumed
            .attempts
            .iter()
            .filter(|attempt| attempt.stage == "plan")
            .count(),
        1,
        "resume must reuse the original planner attempt"
    );
    assert!(f.store.acquire_project(&workflow.id).is_err());
}

#[test]
fn cancelling_idle_plan_finishes_immediately_and_preserves_resume_path() {
    let mut f = Fixture::new();
    let workflow = f.queued();
    assert!(f.store.acquire_project(&workflow.id).unwrap());
    let proposed = plan(&["src/retry.rs"]);
    let planned = f.plan_with(&workflow, &proposed);
    let cancelled = f.store.cancel_workflow(&workflow.id).unwrap();
    assert_eq!(cancelled.status, "interrupted");
    assert_eq!(cancelled.tasks[0].status, "queued");

    let resumed = f.store.resume_workflow(&workflow.id, &proposed).unwrap();
    assert_eq!(resumed.status, "planning");
    assert_eq!(resumed.tasks[0].status, "queued");
    assert_eq!(resumed.plan, planned.plan);
}

#[test]
fn failed_execution_can_reopen_its_plan_and_reset_tasks_for_confirmation() {
    let mut f = Fixture::new();
    let workflow = f.queued();
    assert!(f.store.acquire_project(&workflow.id).unwrap());
    let planned = f.plan_with(&workflow, &plan(&["src/retry.rs"]));
    let running = f.store.begin_execution(&planned.id).unwrap();
    f.store
        .set_task_config(
            &running.tasks[0].id,
            Some("ds-v4.1".into()),
            Some("low".into()),
            None,
            None,
        )
        .unwrap();
    let mut attempt = f
        .store
        .begin_attempt(
            &running.id,
            Some(&running.tasks[0].id),
            "dsh-win",
            "implement",
            Some("ds-v4.1".into()),
            Some("low".into()),
        )
        .unwrap();
    attempt.status = "failed".into();
    attempt.error = Some("temporary failure".into());
    f.store.checkpoint_attempt(&attempt).unwrap();
    f.store
        .finish_workflow(&workflow.id, "failed", "", Some("implementation"))
        .unwrap();

    let resumed = f
        .store
        .resume_workflow(&workflow.id, &plan(&["src/retry.rs"]))
        .unwrap();
    assert_eq!(resumed.status, "planning");
    assert_eq!(resumed.tasks[0].status, "queued");
    assert_eq!(resumed.tasks[0].model.as_deref(), Some("ds-v4.1"));
    assert_eq!(resumed.tasks[0].effort.as_deref(), Some("low"));
    assert!(resumed.tasks[0].error.is_none());
    assert_eq!(resumed.attempts.len(), 2, "retry retains prior evidence");
}

#[test]
fn paused_workflow_role_defaults_do_not_overwrite_task_level_agent_assignments() {
    let mut f = Fixture::new();
    let workflow = f.queued();
    assert!(f.store.acquire_project(&workflow.id).unwrap());
    let proposed = plan(&["src/retry.rs"]);
    f.plan_with(&workflow, &proposed);
    let task_id = f.store.workflow(&workflow.id).unwrap().tasks[0].id.clone();
    f.store
        .set_task_config(
            &task_id,
            Some("ds-v4.1".into()),
            Some("off".into()),
            None,
            None,
        )
        .unwrap();
    f.store
        .finish_workflow(&workflow.id, "failed", "", Some("paused for settings"))
        .unwrap();

    let roles = Roles {
        plan: RoleChoice {
            agent: "hermes-win".into(),
            model: Some("hermes-plan-model".into()),
            effort: Some("low".into()),
        },
        implement: RoleChoice {
            agent: "codex-win".into(),
            model: Some("gpt-6-luna".into()),
            effort: Some("high".into()),
        },
        review: RoleChoice {
            agent: "dsh-win".into(),
            model: Some("ds-v4.1".into()),
            effort: Some("medium".into()),
        },
    };
    let updated = f
        .store
        .update_paused_workflow_roles(&workflow.id, &serde_json::to_string(&roles).unwrap())
        .unwrap();
    assert_eq!(updated.status, "failed");
    let resumed_plan = updated.plan.clone().unwrap();
    assert_eq!(resumed_plan.tasks[0].agent_id, "dsh-win");
    assert_eq!(updated.tasks[0].agent_id, "dsh-win");
    assert_eq!(updated.tasks[0].model.as_deref(), Some("ds-v4.1"));
    assert_eq!(updated.tasks[0].effort.as_deref(), Some("off"));
    assert_eq!(Roles::parse(updated.roles.as_deref()).unwrap(), roles);
    assert_eq!(
        updated
            .attempts
            .iter()
            .filter(|attempt| attempt.stage == "plan")
            .count(),
        1
    );
    let role_json = updated.roles.as_deref().unwrap().to_owned();
    f.store
        .resume_workflow(&workflow.id, &resumed_plan)
        .unwrap();
    assert!(f
        .store
        .update_paused_workflow_roles(&workflow.id, &role_json)
        .is_err());
}

#[test]
fn retry_preserves_completed_tasks_and_only_requeues_unfinished_work() {
    let mut f = Fixture::new();
    let workflow = f.queued();
    assert!(f.store.acquire_project(&workflow.id).unwrap());
    let first = plan(&["src/first.rs"]).tasks.remove(0);
    let mut second = plan(&["src/second.rs"]).tasks.remove(0);
    second.title = "第二项".into();
    second.depends_on = vec![0];
    let proposed = Plan {
        summary: "先完成第一项，再继续第二项".into(),
        tasks: vec![first, second],
    };
    let planned = f.plan_with(&workflow, &proposed);
    let running = f.store.begin_execution(&planned.id).unwrap();
    f.store
        .set_task_config(
            &running.tasks[0].id,
            None,
            None,
            None,
            Some("hub/completed-task".into()),
        )
        .unwrap();
    let completed = f
        .store
        .begin_attempt(
            &running.id,
            Some(&running.tasks[0].id),
            "dsh-win",
            "implement",
            None,
            None,
        )
        .unwrap();
    complete(&mut f.store, &completed);
    let mut failed = f
        .store
        .begin_attempt(
            &running.id,
            Some(&running.tasks[1].id),
            "dsh-win",
            "implement",
            None,
            None,
        )
        .unwrap();
    failed.status = "failed".into();
    f.store.checkpoint_attempt(&failed).unwrap();
    f.store
        .finish_workflow(&workflow.id, "failed", "", Some("second task failed"))
        .unwrap();

    let resumed = f.store.resume_workflow(&workflow.id, &proposed).unwrap();
    assert_eq!(resumed.tasks[0].status, "completed");
    assert_eq!(
        resumed.tasks[0].branch.as_deref(),
        Some("hub/completed-task")
    );
    assert_eq!(resumed.tasks[1].status, "queued");
    assert_eq!(
        resumed.attempts.len(),
        3,
        "all earlier attempts remain visible"
    );
}

#[test]
fn plan_implementation_tasks_follow_the_selected_executor_and_drop_other_agents_model_hints() {
    let mut codex_task = plan(&["src/complex.rs"]).tasks.remove(0);
    codex_task.agent_id = "codex-win".into();
    codex_task.execution = Some(ExecutionChoice {
        model: "gpt-6-luna".into(),
        reasoning_effort: Some("low".into()),
        rationale: "复杂任务".into(),
    });
    let mut dsh_task = plan(&["src/simple.rs"]).tasks.remove(0);
    dsh_task.title = "简单实现".into();
    dsh_task.execution = Some(ExecutionChoice {
        model: "ds-v4.1".into(),
        reasoning_effort: Some("off".into()),
        rationale: "实现任务".into(),
    });
    let mut hermes_task = plan(&["src/reviewed.rs"]).tasks.remove(0);
    hermes_task.agent_id = "hermes-win".into();
    hermes_task.title = "独立实现".into();
    hermes_task.execution = Some(ExecutionChoice {
        model: "hermes-model".into(),
        reasoning_effort: Some("high".into()),
        rationale: "独立任务".into(),
    });
    let mut proposed = Plan {
        summary: "按任务难度分工".into(),
        tasks: vec![codex_task, dsh_task, hermes_task],
    };
    project_store::bind_plan_to_executor(&mut proposed, "dsh-win").unwrap();
    assert!(proposed.tasks.iter().all(|task| task.agent_id == "dsh-win"));
    assert_eq!(proposed.tasks[0].execution, None);
    assert!(proposed.tasks[1].execution.is_some());
    assert_eq!(proposed.tasks[2].execution, None);
    let available = vec!["codex-win".into(), "hermes-win".into(), "dsh-win".into()];
    assert!(crate::projects::validate_plan_agents(&proposed, &available).is_ok());
    assert!(crate::projects::validate_plan_agents(&proposed, &["codex-win".into()]).is_err());
}

#[test]
fn selected_stage_executor_replaces_a_planners_alternate_task_assignment() {
    let mut f = Fixture::new();
    let workflow = f.queued();
    let roles = Roles {
        plan: RoleChoice {
            agent: "codex-win".into(),
            model: None,
            effort: None,
        },
        implement: RoleChoice {
            agent: "codex-win".into(),
            model: None,
            effort: None,
        },
        review: RoleChoice {
            agent: "hermes-win".into(),
            model: None,
            effort: None,
        },
    };
    f.store
        .set_workflow_roles(&workflow.id, Some(&serde_json::to_string(&roles).unwrap()))
        .unwrap();
    f.store.acquire_project(&workflow.id).unwrap();
    let mut proposed = plan(&["src/task.rs"]);
    proposed.tasks[0].agent_id = "hermes-win".into();
    let planned = f.plan_with(&workflow, &proposed);
    let raw_roles = serde_json::to_string(&roles).unwrap();
    assert_eq!(planned.roles.as_deref(), Some(raw_roles.as_str()));
    let running = f.store.begin_execution(&planned.id).unwrap();
    let task = &running.tasks[0];
    assert_eq!(task.agent_id, "codex-win");
    let attempt = f
        .store
        .begin_attempt(
            &running.id,
            Some(&task.id),
            "codex-win",
            "implement",
            None,
            None,
        )
        .unwrap();
    let mut broker = f.broker(&attempt);
    assert!(broker
        .allowed_tool_specs()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "hub_write"));
    assert_eq!(
        broker
            .call(
                "hub_write",
                json!({"path":"src/task.rs","content":"codex-stage","expected_sha256":null})
            )
            .unwrap()["saved"],
        true
    );
}

#[test]
fn hermes_planning_gets_read_only_project_tools() {
    let mut f = Fixture::new();
    let workflow = f.queued();
    let roles = Roles {
        plan: RoleChoice {
            agent: "hermes-win".into(),
            model: None,
            effort: None,
        },
        implement: RoleChoice {
            agent: "dsh-win".into(),
            model: None,
            effort: None,
        },
        review: RoleChoice {
            agent: "codex-win".into(),
            model: None,
            effort: None,
        },
    };
    f.store
        .set_workflow_roles(&workflow.id, Some(&serde_json::to_string(&roles).unwrap()))
        .unwrap();
    f.store.acquire_project(&workflow.id).unwrap();
    let attempt = f
        .store
        .begin_attempt(&workflow.id, None, "hermes-win", "plan", None, None)
        .unwrap();
    let mut broker = f.broker(&attempt);
    let tools = broker.allowed_tool_specs().unwrap();
    assert!(tools.iter().any(|tool| tool["name"] == "hub_list"));
    assert!(tools.iter().any(|tool| tool["name"] == "hub_read"));
    assert!(!tools.iter().any(|tool| tool["name"] == "hub_write"));
    assert!(broker
        .call(
            "hub_write",
            json!({"path":"test.py","content":"changed","expected_sha256":null})
        )
        .is_err());
}

#[test]
fn project_plan_execution_choice_is_optional_and_persisted_in_plan_json() {
    let base_plan = plan(&["src/main.py"]);
    let mut legacy = serde_json::to_value(&base_plan).unwrap();
    legacy["tasks"][0]
        .as_object_mut()
        .unwrap()
        .remove("execution");
    assert_eq!(
        parse_plan(&legacy.to_string()).unwrap(),
        base_plan,
        "historical plans without execution remain readable"
    );

    let mut selected = base_plan.clone();
    let choice = ExecutionChoice {
        model: "codex-main".into(),
        reasoning_effort: Some("high".into()),
        rationale: "此任务需要跨模块推理".into(),
    };
    selected.tasks[0].execution = Some(choice);
    let selected_json = serde_json::to_string(&selected).unwrap();
    assert_eq!(
        serde_json::from_str::<Plan>(&selected_json).unwrap(),
        selected,
        "historical plans carrying execution remain readable"
    );
    assert_eq!(
        parse_plan(&selected_json).unwrap(),
        selected,
        "execution 现在是规划给出的预填建议，不再被 parse_plan 拒收"
    );

    let mut f = Fixture::new();
    let workflow = f.queued();
    f.store.acquire_project(&workflow.id).unwrap();
    let stored = f.plan_with(&workflow, &base_plan);
    assert_eq!(stored.plan.unwrap().tasks[0].execution, None);
}

#[test]
fn project_planning_runs_once_and_waits_for_confirmation() {
    let mut f = Fixture::new();
    let workflow = f.queued();
    f.store.acquire_project(&workflow.id).unwrap();
    // 规划角色缺省是 Codex：其他成员不能抢规划。
    assert!(f
        .store
        .begin_attempt(&workflow.id, None, "hermes-win", "plan", None, None)
        .is_err());

    let proposed = plan(&["src/main.py"]);
    let mut codex = f
        .store
        .begin_attempt(&workflow.id, None, "codex-win", "plan", None, None)
        .unwrap();
    codex.output = serde_json::to_string(&proposed).unwrap();
    // 规划未完成前不能再次规划。
    assert!(f
        .store
        .begin_attempt(&workflow.id, None, "codex-win", "plan", None, None)
        .is_err());
    complete(&mut f.store, &codex);

    let stored = f.store.save_plan(&workflow.id, &proposed).unwrap();
    // 保存方案后仍是待确认：状态不变、任务已落库。
    assert_eq!(stored.status, "planning");
    assert_eq!(stored.tasks.len(), 1);
    // 确认前不能开始实现。
    assert!(f
        .store
        .begin_attempt(
            &workflow.id,
            Some(&stored.tasks[0].id),
            "dsh-win",
            "implement",
            None,
            None
        )
        .is_err());
    let running = f.store.begin_execution(&workflow.id).unwrap();
    assert_eq!(running.status, "running");
}

#[test]
fn project_failed_plan_cannot_be_saved() {
    let mut f = Fixture::new();
    let workflow = f.queued();
    f.store.acquire_project(&workflow.id).unwrap();
    let mut codex = f
        .store
        .begin_attempt(&workflow.id, None, "codex-win", "plan", None, None)
        .unwrap();
    codex.output = serde_json::to_string(&plan(&["src/main.py"])).unwrap();
    codex.status = "failed".into();
    f.store.checkpoint_attempt(&codex).unwrap();
    assert!(f
        .store
        .save_plan(&workflow.id, &plan(&["src/main.py"]))
        .is_err());
}

#[test]
fn project_codex_plan_tools_are_read_only_scoped_and_cancelled_reads_stop() {
    let mut f = Fixture::new();
    std::fs::write(f.root.join("ordinary.txt"), "visible project input").unwrap();
    let workflow = f.queued();
    f.store.acquire_project(&workflow.id).unwrap();
    let attempt = f
        .store
        .begin_attempt(&workflow.id, None, "codex-win", "plan", None, None)
        .unwrap();
    let mut broker = f.broker(&attempt);
    let listing = broker.call("hub_list", json!({})).unwrap();
    assert_eq!(listing["writable"], false);
    assert!(listing["files"]
        .as_array()
        .unwrap()
        .iter()
        .any(|path| path == "ordinary.txt"));
    let read = broker
        .call("hub_read", json!({"path":"ordinary.txt"}))
        .unwrap();
    assert_eq!(read["content"], "visible project input");
    assert!(broker
        .call(
            "hub_write",
            json!({"path":"ordinary.txt","content":"changed","expected_sha256":read["sha256"]})
        )
        .is_err());
    assert!(broker
        .call(
            "hub_edit",
            json!({"path":"ordinary.txt","old_text":"visible","new_text":"hidden","expected_sha256":read["sha256"]})
        )
        .is_err());
    assert!(broker
        .call(
            "hub_delete",
            json!({"path":"ordinary.txt","expected_sha256":read["sha256"]})
        )
        .is_err());
    assert!(broker.call("hub_read", json!({"path":".env"})).is_err());
    assert!(broker
        .call("hub_read", json!({"path":"../outside.txt"}))
        .is_err());

    f.store.cancel_workflow(&workflow.id).unwrap();
    assert!(broker
        .call("hub_read", json!({"path":"ordinary.txt"}))
        .is_err());
    assert_eq!(
        std::fs::read_to_string(f.root.join("ordinary.txt")).unwrap(),
        "visible project input"
    );
}

#[test]
fn hermes_planner_can_read_project_files_but_cannot_write() {
    let mut f = Fixture::new();
    std::fs::write(f.root.join("ordinary.txt"), "visible project input").unwrap();
    let workflow = f.queued();
    f.store.acquire_project(&workflow.id).unwrap();
    // Hermes 规划器拿到只读项目工具，写操作仍由 Broker 拒绝。
    let roles = r#"{"plan":{"agent":"hermes-win","model":null,"effort":null},"implement":{"agent":"dsh-win","model":null,"effort":null},"review":{"agent":"hermes-win","model":null,"effort":null}}"#;
    f.store
        .set_workflow_roles(&workflow.id, Some(roles))
        .unwrap();
    let hermes = f
        .store
        .begin_attempt(&workflow.id, None, "hermes-win", "plan", None, None)
        .unwrap();
    let mut broker = f.broker(&hermes);
    let tools = broker.allowed_tool_specs().unwrap();
    assert!(tools.iter().any(|tool| tool["name"] == "hub_list"));
    assert!(tools.iter().any(|tool| tool["name"] == "hub_read"));
    assert!(!tools.iter().any(|tool| tool["name"] == "hub_write"));
    assert!(broker.call("hub_list", json!({})).is_ok());
    assert_eq!(
        broker
            .call("hub_read", json!({"path":"ordinary.txt"}))
            .unwrap()["content"],
        "visible project input"
    );
    assert!(broker
        .call(
            "hub_write",
            json!({"path":"ordinary.txt","content":"changed","expected_sha256":null})
        )
        .is_err());
    assert_eq!(
        std::fs::read_to_string(f.root.join("ordinary.txt")).unwrap(),
        "visible project input"
    );
}

#[test]
fn project_save_plan_requires_exact_completed_plan_and_no_active_attempt() {
    let mut f = Fixture::new();
    let workflow = f.queued();
    f.store.acquire_project(&workflow.id).unwrap();
    let proposed = plan(&["src/main.py"]);
    let mut codex = f
        .store
        .begin_attempt(&workflow.id, None, "codex-win", "plan", None, None)
        .unwrap();
    codex.output = serde_json::to_string(&proposed).unwrap();
    complete(&mut f.store, &codex);

    let mut changed = proposed.clone();
    changed.summary.push_str(" 扩大要求");
    assert!(f.store.save_plan(&workflow.id, &changed).is_err());
    let mut changed = proposed.clone();
    changed.tasks[0].instructions.push_str(" 改写要求");
    assert!(f.store.save_plan(&workflow.id, &changed).is_err());

    f.store
        .connection
        .execute(
            "UPDATE project_attempts SET status='running' WHERE id=?1",
            [&codex.id],
        )
        .unwrap();
    assert!(f.store.save_plan(&workflow.id, &proposed).is_err());
}

#[test]
fn project_registration_is_idempotent_and_excludes_app_data() {
    let mut f = Fixture::new();
    let p = f.store.project(&f.project).unwrap();
    assert_eq!(f.store.register_project(&p.name, &p.root).unwrap().id, p.id);
    assert!(f.store.register_project("另一个名称", &p.root).is_err());
    assert!(f
        .store
        .register_project("应用数据", f.base.join("data").to_str().unwrap())
        .is_err());
    assert!(f
        .store
        .register_project("不存在", f.base.join("missing").to_str().unwrap())
        .is_err());
}

#[test]
fn project_workflow_is_idempotent_and_guards_room_changes() {
    let mut f = Fixture::new();
    let w = f.queued();
    assert!(
        !f.store
            .start_workflow(&f.room, &w.user_message_id, &w.request)
            .unwrap()
            .1
    );
    assert!(f
        .store
        .start_workflow(&f.room, &w.user_message_id, "漂移需求")
        .is_err());
    assert!(f.store.bind_project(&f.room, None).is_err());
    assert!(f.store.archive(&f.room, true).is_err());
    assert!(f.store.delete(&f.room).is_err());
    assert!(f
        .store
        .save_message(&f.room, &Uuid::new_v4().to_string(), "另一条消息")
        .is_err());
    assert!(f
        .store
        .start_discussion(
            &f.room,
            &Uuid::new_v4().to_string(),
            "讨论",
            &["codex-win".into(), "hermes-win".into()],
            1
        )
        .is_err());
}

#[test]
fn project_lease_blocks_same_and_nested_roots_but_allows_another_project() {
    let mut f = Fixture::new();
    let one = f.queued();
    assert!(f.store.acquire_project(&one.id).unwrap());
    let room = f
        .store
        .create("第二群", "group", &["codex-win".into(), "dsh-win".into()])
        .unwrap()
        .id;
    f.store.bind_project(&room, Some(&f.project)).unwrap();
    let same = f
        .store
        .start_workflow(&room, &Uuid::new_v4().to_string(), "第二需求")
        .unwrap()
        .0;
    assert!(!f.store.acquire_project(&same.id).unwrap());
    let nested = f.root.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    let child = f
        .store
        .register_project("子项目", nested.to_str().unwrap())
        .unwrap();
    f.store
        .finish_workflow(&same.id, "interrupted", "", None)
        .unwrap();
    f.store.bind_project(&room, Some(&child.id)).unwrap();
    let next = f
        .store
        .start_workflow(&room, &Uuid::new_v4().to_string(), "子目录需求")
        .unwrap()
        .0;
    assert!(!f.store.acquire_project(&next.id).unwrap());
    f.store
        .finish_workflow(&next.id, "interrupted", "", None)
        .unwrap();
    let other = f.base.join("project-other");
    std::fs::create_dir_all(&other).unwrap();
    let p = f
        .store
        .register_project("另项目", other.to_str().unwrap())
        .unwrap();
    f.store.bind_project(&room, Some(&p.id)).unwrap();
    let next = f
        .store
        .start_workflow(&room, &Uuid::new_v4().to_string(), "其他项目需求")
        .unwrap()
        .0;
    assert!(f.store.acquire_project(&next.id).unwrap());
    f.store
        .finish_workflow(&one.id, "interrupted", "", None)
        .unwrap();
}

#[test]
fn project_task_dependencies_and_agent_busy_are_enforced() {
    let mut f = Fixture::new();
    let w = f.queued();
    f.store.acquire_project(&w.id).unwrap();
    let mut p = plan(&["a.py"]);
    let mut second = p.tasks[0].clone();
    second.agent_id = "dsh-win".into();
    second.files = vec!["b.py".into()];
    second.depends_on = vec![0];
    p.tasks.push(second);
    let w = f.plan_with(&w, &p);
    let w = f.store.begin_execution(&w.id).unwrap();
    assert!(f
        .store
        .begin_attempt(
            &w.id,
            Some(&w.tasks[1].id),
            "dsh-win",
            "implement",
            None,
            None
        )
        .is_err());
    let a = f
        .store
        .begin_attempt(
            &w.id,
            Some(&w.tasks[0].id),
            "dsh-win",
            "implement",
            None,
            None,
        )
        .unwrap();
    let direct = f
        .store
        .create("独立私聊", "direct", &["dsh-win".into()])
        .unwrap();
    assert!(f
        .store
        .begin_run(&direct.id, &Uuid::new_v4().to_string(), "不能同时运行")
        .is_err());
    assert!(f
        .store
        .finish_workflow(&w.id, "interrupted", "", None)
        .is_err());
    complete(&mut f.store, &a);
    assert!(f
        .store
        .begin_attempt(
            &w.id,
            Some(&w.tasks[1].id),
            "dsh-win",
            "implement",
            None,
            None
        )
        .is_ok());
}

#[test]
fn cancelling_one_agent_marks_only_its_attempt_and_keeps_other_tasks_running() {
    let mut f = Fixture::new();
    let workflow = f.queued();
    assert!(f.store.acquire_project(&workflow.id).unwrap());
    let mut plan_attempt = f
        .store
        .begin_attempt(&workflow.id, None, "codex-win", "plan", None, None)
        .unwrap();
    let proposed = Plan {
        summary: "两个互不依赖的模块".into(),
        tasks: vec![
            PlannedTask {
                title: "DSH模块".into(),
                agent_id: "dsh-win".into(),
                instructions: "只处理 a.py".into(),
                files: vec!["a.py".into()],
                depends_on: vec![],
                execution: None,
            },
            PlannedTask {
                title: "Codex模块".into(),
                agent_id: "codex-win".into(),
                instructions: "只处理 b.py".into(),
                files: vec!["b.py".into()],
                depends_on: vec![],
                execution: None,
            },
            PlannedTask {
                title: "等待 DSH 的模块".into(),
                agent_id: "hermes-win".into(),
                instructions: "依赖 DSH 模块".into(),
                files: vec!["c.py".into()],
                depends_on: vec![0],
                execution: None,
            },
        ],
    };
    plan_attempt.output = serde_json::to_string(&proposed).unwrap();
    complete(&mut f.store, &plan_attempt);
    let planned = f.store.save_plan(&workflow.id, &proposed).unwrap();
    let running = f.store.begin_execution(&planned.id).unwrap();
    let dsh = f
        .store
        .begin_attempt(
            &running.id,
            Some(&running.tasks[0].id),
            "dsh-win",
            "implement",
            None,
            None,
        )
        .unwrap();
    let codex = f
        .store
        .begin_attempt(
            &running.id,
            Some(&running.tasks[1].id),
            "codex-win",
            "implement",
            None,
            None,
        )
        .unwrap();
    let cancelled = f
        .store
        .cancel_agent_attempts(&running.id, "dsh-win")
        .unwrap();
    assert_eq!(cancelled, vec![dsh.id.clone()]);
    assert_eq!(f.store.attempt(&dsh.id).unwrap().status, "cancelling");
    assert_eq!(f.store.attempt(&codex.id).unwrap().status, "starting");
    let mut stopped = dsh;
    stopped.status = "interrupted".into();
    stopped.error = Some("用户单独中断".into());
    f.store.checkpoint_attempt(&stopped).unwrap();
    complete(&mut f.store, &codex);
    f.store
        .finish_workflow(&running.id, "interrupted", "", Some("依赖任务等待恢复"))
        .unwrap();
    let saved = f.store.workflow(&running.id).unwrap();
    assert_eq!(saved.status, "interrupted");
    assert_eq!(saved.tasks[0].status, "interrupted");
    assert_eq!(saved.tasks[1].status, "completed");
    assert_eq!(saved.tasks[2].status, "queued");
    f.store.recover_projects().unwrap();
    assert_eq!(
        f.store.workflow(&running.id).unwrap().tasks[2].status,
        "queued"
    );
    let notice = f
        .store
        .add_system_team_message(
            &running.id,
            "用户单独中断了 DSH；Codex 的独立任务继续运行。 ",
        )
        .unwrap();
    assert_eq!(notice.kind, "system");
    let again = f
        .store
        .add_system_team_message(&running.id, "用户单独中断了 DSH；Codex 的独立任务继续运行。")
        .unwrap();
    assert_eq!(again.id, notice.id);
    let other = f
        .store
        .add_system_team_message(&running.id, "另一条状态")
        .unwrap();
    assert_ne!(other.id, notice.id);
    let back = f
        .store
        .add_system_team_message(&running.id, "用户单独中断了 DSH；Codex 的独立任务继续运行。")
        .unwrap();
    assert_ne!(back.id, notice.id);
}

#[test]
fn project_completion_requires_functional_review_approval_only() {
    let mut f = Fixture::new();
    let implementation = f.implement(&["a.py"]);
    complete(&mut f.store, &implementation);
    let mut review = f
        .store
        .begin_attempt(
            &implementation.workflow_id,
            None,
            "hermes-win",
            "review",
            None,
            None,
        )
        .unwrap();
    assert!(f
        .store
        .finish_workflow(&implementation.workflow_id, "completed", "", None)
        .is_err());
    review.status = "completed".into();
    review.output = r#"{"approved":false,"summary":"需要修复","issues":["仍有问题"]}"#.into();
    f.store.checkpoint_attempt(&review).unwrap();
    assert!(f
        .store
        .finish_workflow(&implementation.workflow_id, "completed", "", None)
        .is_err());
    f.store
        .connection
        .execute(
            "UPDATE project_attempts SET output=?1 WHERE id=?2",
            rusqlite::params![
                r#"{"approved":true,"summary":"验收通过","issues":[]}"#,
                review.id
            ],
        )
        .unwrap();
    assert_eq!(
        f.store
            .finish_workflow(&implementation.workflow_id, "completed", "通过", None)
            .unwrap()
            .status,
        "completed"
    );
}

#[test]
fn project_pre_prompt_failure_does_not_claim_delivery() {
    let mut f = Fixture::new();
    let w = f.queued();
    f.store.acquire_project(&w.id).unwrap();
    let mut a = f
        .store
        .begin_attempt(&w.id, None, "codex-win", "plan", None, None)
        .unwrap();
    assert_eq!(
        f.store.detail(&f.room).unwrap().messages[0].status,
        "pending"
    );
    a.status = "failed".into();
    f.store.checkpoint_attempt(&a).unwrap();
    f.store
        .finish_workflow(&w.id, "failed", "", Some("握手失败"))
        .unwrap();
    assert_eq!(
        f.store.detail(&f.room).unwrap().messages[0].status,
        "failed"
    );
}

#[test]
fn project_native_admission_records_delivery_and_freezes_attempt_identity() {
    let mut f = Fixture::new();
    let mut a = f.implement(&["a.py"]);
    a.native_thread_id = Some("native-thread".into());
    a.native_turn_id = Some("native-turn".into());
    a.status = "running".into();
    f.store.checkpoint_attempt(&a).unwrap();
    assert_eq!(
        f.store.detail(&f.room).unwrap().messages[0].status,
        "delivered"
    );
    a.agent_id = "codex-win".into();
    assert!(f.store.checkpoint_attempt(&a).is_err());
}

#[test]
fn project_recovery_interrupts_instead_of_replaying_and_releases_lease() {
    let mut f = Fixture::new();
    let a = f.implement(&["a.py"]);
    let mut b = f.broker(&a);
    f.store.recover_projects().unwrap();
    assert_eq!(
        f.store.workflow(&a.workflow_id).unwrap().status,
        "interrupted"
    );
    assert_eq!(f.store.attempt(&a.id).unwrap().status, "interrupted");
    assert!(b
        .call(
            "hub_write",
            json!({"path":"a.py","content":"old","expected_sha256":null})
        )
        .is_err());
    let w = f.queued();
    assert!(f.store.acquire_project(&w.id).unwrap());
}

/// 退出重启后「继续协作」要复用原方案：已完成的成果保留，未完成的重新排队，租约重新占住。
#[test]
fn project_continue_after_recovery_requeues_unfinished_work_and_regains_the_lease() {
    let mut f = Fixture::new();
    let workflow = f.queued();
    assert!(f.store.acquire_project(&workflow.id).unwrap());
    let first = plan(&["src/first.rs"]).tasks.remove(0);
    let mut second = plan(&["src/second.rs"]).tasks.remove(0);
    second.title = "第二项".into();
    second.depends_on = vec![0];
    let proposed = Plan {
        summary: "先完成第一项，再继续第二项".into(),
        tasks: vec![first, second],
    };
    let planned = f.plan_with(&workflow, &proposed);
    let running = f.store.begin_execution(&planned.id).unwrap();
    f.store
        .set_task_config(
            &running.tasks[0].id,
            None,
            None,
            Some("C:\\worktrees\\crashed-0".into()),
            Some("hub/completed-task".into()),
        )
        .unwrap();
    let completed = f
        .store
        .begin_attempt(
            &running.id,
            Some(&running.tasks[0].id),
            "dsh-win",
            "implement",
            None,
            None,
        )
        .unwrap();
    complete(&mut f.store, &completed);
    let live = f
        .store
        .begin_attempt(
            &running.id,
            Some(&running.tasks[1].id),
            "dsh-win",
            "implement",
            None,
            None,
        )
        .unwrap();

    f.store.recover_projects().unwrap();
    let interrupted = f.store.workflow(&workflow.id).unwrap();
    assert_eq!(interrupted.status, "interrupted");
    assert_eq!(interrupted.tasks[0].status, "completed");
    assert_eq!(interrupted.tasks[1].status, "interrupted");
    assert_eq!(f.store.attempt(&live.id).unwrap().status, "interrupted");
    assert_eq!(
        f.store.detail(&f.room).unwrap().messages[0].status,
        "failed",
        "中断后这条需求不能留在待投递状态"
    );

    let resumed = f.store.resume_workflow(&workflow.id, &proposed).unwrap();
    assert_eq!(resumed.status, "planning");
    assert_eq!(resumed.plan, Some(proposed));
    assert_eq!(resumed.tasks[0].status, "completed");
    assert_eq!(
        resumed.tasks[0].branch.as_deref(),
        Some("hub/completed-task"),
        "已完成任务继续沿用原分支"
    );
    assert_eq!(resumed.tasks[1].status, "queued");
    assert_eq!(resumed.tasks[1].branch, None);
    assert_eq!(resumed.tasks[1].worktree, None);
    assert_eq!(
        f.store
            .attempts(&workflow.id)
            .unwrap()
            .iter()
            .filter(|attempt| attempt.stage == "plan")
            .count(),
        1,
        "继续协作复用原规划，不重新调用规划模型"
    );
    // 继续成功后重新占住项目租约：同一个项目在别的群聊里不能再开新协作。
    let other_room = f
        .store
        .create(
            "项目群二",
            "group",
            &["codex-win".into(), "hermes-win".into(), "dsh-win".into()],
        )
        .unwrap()
        .id;
    f.store.bind_project(&other_room, Some(&f.project)).unwrap();
    let another = f
        .store
        .start_workflow(&other_room, &Uuid::new_v4().to_string(), "另一个需求")
        .unwrap()
        .0;
    assert!(!f.store.acquire_project(&another.id).unwrap());
}

/// 退出恰好发生在「纯答复方案已保存」与「协作收尾」之间时，继续协作要落到与首次规划相同的终态。
#[test]
fn project_continue_after_recovery_finishes_a_reply_only_plan_instead_of_parking_it() {
    let mut f = Fixture::new();
    let workflow = f.queued();
    assert!(f.store.acquire_project(&workflow.id).unwrap());
    let reply = Plan {
        summary: "这个问题的答案是：讨论和实施现在共用同一个项目群。".into(),
        tasks: vec![],
    };
    let mut planning = f
        .store
        .begin_attempt(&workflow.id, None, "codex-win", "plan", None, None)
        .unwrap();
    planning.output = serde_json::to_string(&reply).unwrap();
    complete(&mut f.store, &planning);
    f.store.save_plan(&workflow.id, &reply).unwrap();
    // 这一段只回放重启后的恢复：方案已保存、任务表为空、消息仍待投递、租约还没释放。
    f.store.recover_projects().unwrap();
    assert_eq!(
        f.store.workflow(&workflow.id).unwrap().status,
        "interrupted"
    );

    let resumed = f.store.resume_workflow(&workflow.id, &reply).unwrap();
    assert_eq!(
        resumed.status, "completed",
        "纯答复方案没有可确认的任务，继续协作必须直接收尾"
    );
    assert_eq!(resumed.summary, reply.summary);
    assert!(resumed.tasks.is_empty());
    assert_eq!(
        f.store.detail(&f.room).unwrap().messages[0].status,
        "delivered",
        "收尾后这条需求要记为已投递"
    );
    // 收尾后不再占租约，同一个项目可以正常开下一轮协作。
    let next = f.queued();
    assert!(f.store.acquire_project(&next.id).unwrap());
}

#[test]
fn project_attempt_thought_is_persisted_as_streaming_progress() {
    let mut f = Fixture::new();
    let mut attempt = f.implement(&["thought.py"]);
    attempt.thought = "先检查任务文件，再验证调用路径。".into();
    f.store.checkpoint_attempt(&attempt).unwrap();
    let saved = f.store.attempt(&attempt.id).unwrap();
    assert_eq!(saved.thought, attempt.thought);
}

#[test]
fn project_broker_writes_edits_deletes_with_hashes_and_preserves_first_backup() {
    let mut f = Fixture::new();
    std::fs::write(f.root.join("a.py"), "before").unwrap();
    let a = f.implement(&["a.py", "src/new.py"]);
    let mut b = f.broker(&a);
    let read = b.call("hub_read", json!({"path":"a.py"})).unwrap();
    assert_eq!(read["content"], "before");
    assert!(b
        .call(
            "hub_write",
            json!({"path":"a.py","content":"bad","expected_sha256":null})
        )
        .is_err());
    let written = b
        .call(
            "hub_write",
            json!({"path":"a.py","content":"after","expected_sha256":read["sha256"]}),
        )
        .unwrap();
    assert!(b.call("hub_edit",json!({"path":"a.py","old_text":"after","new_text":"changed","expected_sha256":read["sha256"]})).is_err());
    let edited=b.call("hub_edit",json!({"path":"a.py","old_text":"after","new_text":"changed","expected_sha256":written["sha256"]})).unwrap();
    assert_eq!(
        std::fs::read_to_string(f.root.join("a.py")).unwrap(),
        "changed"
    );
    b.call(
        "hub_delete",
        json!({"path":"a.py","expected_sha256":edited["sha256"]}),
    )
    .unwrap();
    assert!(!f.root.join("a.py").exists());
    b.call(
        "hub_write",
        json!({"path":"src/new.py","content":"new","expected_sha256":null}),
    )
    .unwrap();
    let backups = std::fs::read_dir(f.base.join("data/project-backups").join(&a.id))
        .unwrap()
        .map(|v| v.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(
        backups
            .iter()
            .filter(|p| p.extension().is_some_and(|e| e == "bin"))
            .count(),
        1
    );
    let original = backups
        .iter()
        .find(|p| p.extension().is_some_and(|e| e == "bin"))
        .unwrap();
    assert_eq!(std::fs::read_to_string(original).unwrap(), "before");
    let before_hash: String = f
        .store
        .connection
        .query_row(
            "SELECT before_hash FROM project_changes WHERE attempt_id=?1 AND path='a.py'",
            [&a.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(before_hash, read["sha256"].as_str().unwrap());
}

#[test]
fn project_broker_rejects_out_of_scope_paths_unknown_tools_and_cancelled_requests() {
    let mut f = Fixture::new();
    let a = f.implement(&["a.py"]);
    let mut b = f.broker(&a);
    for path in ["b.py", "../escape.py", ".env", ".git/config", "a.py:other"] {
        assert!(b
            .call(
                "hub_write",
                json!({"path":path,"content":"bad","expected_sha256":null})
            )
            .is_err());
    }
    assert!(b.call("shell", json!({"command":"whoami"})).is_err());
    assert!(b.call("hub_list", json!({"extra":true})).is_err());
    f.store.cancel_workflow(&a.workflow_id).unwrap();
    assert!(b
        .call(
            "hub_write",
            json!({"path":"a.py","content":"stale","expected_sha256":null})
        )
        .is_err());
    assert!(!f.root.join("a.py").exists());
    assert!(f
        .store
        .finish_workflow(&a.workflow_id, "interrupted", "", None)
        .is_err());
    let mut a = a;
    a.status = "interrupted".into();
    f.store.checkpoint_attempt(&a).unwrap();
    assert!(f
        .store
        .finish_workflow(&a.workflow_id, "interrupted", "", None)
        .is_ok());
}

#[test]
fn project_broker_rejects_hard_links_that_could_write_outside_the_project() {
    let mut f = Fixture::new();
    let outside = f.base.join("outside.txt");
    std::fs::write(&outside, "unchanged").unwrap();
    std::fs::hard_link(&outside, f.root.join("a.py")).unwrap();
    let a = f.implement(&["a.py"]);
    let mut b = f.broker(&a);
    assert!(b.call("hub_read", json!({"path":"a.py"})).is_err());
    assert!(b
        .call(
            "hub_write",
            json!({"path":"a.py","content":"bad","expected_sha256":null})
        )
        .is_err());
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "unchanged");
}

#[test]
fn project_broker_allows_authorized_test_files_and_blocks_large_files_and_binary_reads() {
    let mut f = Fixture::new();
    std::fs::write(f.root.join("binary.dat"), [0xff, 0xfe]).unwrap();
    std::fs::write(f.root.join("large.txt"), vec![b'a'; 1_048_577]).unwrap();
    let a = f.implement(&["test.py", "binary.dat", "large.txt", "new.txt"]);
    let mut b = f.broker(&a);
    let written = b
        .call(
            "hub_write",
            json!({"path":"test.py","content":"assert True","expected_sha256":null}),
        )
        .unwrap();
    assert!(!written["isError"].as_bool().unwrap_or(false));
    assert_eq!(
        std::fs::read_to_string(f.root.join("test.py")).unwrap(),
        "assert True"
    );
    assert!(b.call("hub_read", json!({"path":"binary.dat"})).is_err());
    assert!(b.call("hub_read", json!({"path":"large.txt"})).is_err());
    assert!(b
        .call(
            "hub_write",
            json!({"path":"new.txt","content":"a".repeat(1_048_577),"expected_sha256":null})
        )
        .is_err());
    assert!(!f.root.join("new.txt").exists());
}

#[test]
fn project_helper_open_does_not_recover_live_workflow_and_review_is_read_only() {
    let mut f = Fixture::new();
    std::fs::write(f.root.join("a.py"), "source").unwrap();
    let a = f.implement(&["a.py"]);
    let b = f.broker(&a);
    drop(b);
    assert_eq!(f.store.attempt(&a.id).unwrap().status, "starting");
    assert_eq!(f.store.workflow(&a.workflow_id).unwrap().status, "running");
    complete(&mut f.store, &a);
    let review = f
        .store
        .begin_attempt(&a.workflow_id, None, "hermes-win", "review", None, None)
        .unwrap();
    let mut b = f.broker(&review);
    let read = b.call("hub_read", json!({"path":"a.py"})).unwrap();
    assert!(b
        .call(
            "hub_write",
            json!({"path":"a.py","content":"bad","expected_sha256":read["sha256"]})
        )
        .is_err());
    assert_eq!(
        std::fs::read_to_string(f.root.join("a.py")).unwrap(),
        "source"
    );
}

#[test]
fn project_repair_is_bounded_to_one_attempt() {
    let mut f = Fixture::new();
    let a = f.implement(&["a.py"]);
    complete(&mut f.store, &a);
    let mut review = f
        .store
        .begin_attempt(&a.workflow_id, None, "hermes-win", "review", None, None)
        .unwrap();
    review.status = "completed".into();
    review.output = r#"{"approved":false,"summary":"需要修复","issues":["问题"]}"#.into();
    f.store.checkpoint_attempt(&review).unwrap();
    let failed = f
        .store
        .begin_attempt(&a.workflow_id, None, "dsh-win", "repair", None, None)
        .unwrap();
    let mut failed = failed;
    failed.status = "failed".into();
    failed.error = Some("上游额度限制".into());
    f.store.checkpoint_attempt(&failed).unwrap();
    f.store
        .finish_workflow(&a.workflow_id, "failed", "", Some("上游额度限制"))
        .unwrap();
    let plan = f.store.workflow(&a.workflow_id).unwrap().plan.unwrap();
    f.store.resume_workflow(&a.workflow_id, &plan).unwrap();
    f.store.begin_execution(&a.workflow_id).unwrap();
    let mut retry_review = f
        .store
        .begin_attempt(&a.workflow_id, None, "hermes-win", "review", None, None)
        .unwrap();
    retry_review.status = "completed".into();
    retry_review.output = r#"{"approved":false,"summary":"仍需修复","issues":["问题"]}"#.into();
    f.store.checkpoint_attempt(&retry_review).unwrap();
    let repair = f
        .store
        .begin_attempt(&a.workflow_id, None, "dsh-win", "repair", None, None)
        .unwrap();
    complete(&mut f.store, &repair);
    assert!(f
        .store
        .begin_attempt(&a.workflow_id, None, "dsh-win", "repair", None, None)
        .is_err());
}

#[test]
fn project_plan_and_broker_normalize_case_and_separator_aliases() {
    let mut p = plan(&["src/a.py", "src\\a.py"]);
    assert!(parse_plan(&serde_json::to_string(&p).unwrap()).is_err());
    if cfg!(windows) {
        p.tasks[0].files = vec!["A.py".into(), "a.PY".into()];
        assert!(parse_plan(&serde_json::to_string(&p).unwrap()).is_err());
    }
    let mut f = Fixture::new();
    let a = f.implement(&["src/a.py"]);
    let mut b = f.broker(&a);
    let first = b
        .call(
            "hub_write",
            json!({"path":"src\\a.py","content":"first","expected_sha256":null}),
        )
        .unwrap();
    let path = if cfg!(windows) {
        "SRC/A.PY"
    } else {
        "src/a.py"
    };
    b.call(
        "hub_write",
        json!({"path":path,"content":"second","expected_sha256":first["sha256"]}),
    )
    .unwrap();
    let count: i64 = f
        .store
        .connection
        .query_row(
            "SELECT count(*) FROM project_changes WHERE attempt_id=?1",
            [&a.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    let directory = f.base.join("data/project-backups").join(&a.id);
    assert_eq!(
        std::fs::read_dir(directory)
            .unwrap()
            .filter(|p| p
                .as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|e| e == "json"))
            .count(),
        1
    );
}

#[test]
fn structured_plan_and_review_accept_one_fence_with_brief_prose() {
    let p = plan(&["calc.py"]);
    let body = serde_json::to_string(&p).unwrap();
    assert_eq!(
        parse_plan(&format!(
            "以下是方案：\n```json\r\n{body}\n```\n请由框架运行检查。"
        ))
        .unwrap(),
        p
    );
    assert_eq!(
        parse_plan(&format!(
            "我已检查项目文件，下面是结构化方案：\n{body}\n方案范围仅供确认。"
        ))
        .unwrap(),
        p
    );
    let body = r#"{"approved":true,"summary":"实际检查通过","issues":[]}"#;
    assert!(
        parse_review(&format!("```json\n{body}\n```\n已对照需求和源码完成复核。"))
            .unwrap()
            .approved
    );
    assert!(
        parse_review(&format!("我会只读核对源码并给出 JSON：\n{body}"))
            .unwrap()
            .approved
    );
}

#[test]
fn structured_outputs_reject_ambiguous_incomplete_or_untyped_documents() {
    let body = serde_json::to_string(&plan(&["calc.py"])).unwrap();
    for raw in [
        format!("```json\n{body}"),
        format!("```json\n{body}\n```\n```json\n{body}\n```"),
        format!("{{}}\n```json\n{body}\n```"),
        format!("```python\n{body}\n```"),
        format!("```json\n{body} {{}}\n```"),
        format!("```json\n{body}\n```{}", "x".repeat(2001)),
        format!("说明：{body} 另一个对象：{{}}"),
        format!("说明：{{}}\n{body}"),
    ] {
        assert!(parse_plan(&raw).is_err());
        assert!(parse_review(&raw).is_err());
    }
    assert!(parse_review(
        "```json\n{\"approved\":true,\"summary\":\"a\",\"issues\":[],\"unknown\":0}\n```"
    )
    .is_err());
}

#[test]
fn summary_opt_in_default_live_completed_revoke() {
    let mut f = Fixture::new();
    let job = f.queued();
    assert!(
        f.store.shared_workflows().unwrap().is_empty(),
        "摘要默认不开启"
    );
    assert!(crate::services::dev_digest(&f.store).is_empty());
    f.store.set_project_summary(&f.project, true).unwrap();
    let shared = f.store.shared_workflows().unwrap();
    assert_eq!(shared.len(), 1);
    assert_eq!(shared[0].id, job.id);
    // 隔离 fixture 直接落合法终态：真实 finish_workflow 的实现/验收保护保持不变。
    f.store
        .connection
        .execute(
            "UPDATE workflows SET status='completed',summary=?1 WHERE id=?2",
            rusqlite::params!["DSH完成隔离成果", job.id],
        )
        .unwrap();
    assert_eq!(f.store.shared_workflows().unwrap().len(), 1);
    assert!(crate::services::dev_digest(&f.store).contains("DSH完成隔离成果"));
    let private = f
        .store
        .create("独立私聊", "direct", &["dsh-win".into()])
        .unwrap();
    f.store
        .save_message(&private.id, &Uuid::new_v4().to_string(), "secret marker")
        .unwrap();
    assert!(!crate::services::dev_digest(&f.store).contains("secret marker"));
    f.store.set_project_summary(&f.project, false).unwrap();
    assert!(f.store.shared_workflows().unwrap().is_empty());
    assert!(crate::services::dev_digest(&f.store).is_empty());
    assert!(f.store.set_project_summary("unknown-id", true).is_err());
}

#[test]
fn maintenance_blocks_new_work_and_queued_prevents_maintenance() {
    let mut f = Fixture::new();
    f.store.begin_service_maintenance("dsh-win").unwrap();
    assert!(f.store.guard_service_maintenance("dsh-win").is_err());
    assert!(f
        .store
        .start_workflow(&f.room, &Uuid::new_v4().to_string(), "维护期间需求")
        .is_err());
    assert!(f
        .store
        .start_discussion(
            &f.room,
            &Uuid::new_v4().to_string(),
            "隔离讨论",
            &["hermes-win".into(), "dsh-win".into()],
            1
        )
        .is_err());
    let direct = f
        .store
        .create("维护期间私聊", "direct", &["dsh-win".into()])
        .unwrap();
    f.store
        .validate_private_target(&direct.id, "dsh-win")
        .unwrap();
    assert!(f
        .store
        .begin_agent_run(
            &direct.id,
            &Uuid::new_v4().to_string(),
            "维护期间私聊",
            "dsh-win"
        )
        .is_err());
    let counts: (i64, i64, i64) = f
        .store
        .connection
        .query_row(
            "SELECT (SELECT count(*) FROM runs), (SELECT count(*) FROM discussions), (SELECT count(*) FROM workflows)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        counts,
        (0, 0, 0),
        "被维护拒绝的请求不应留下 runs/discussions/workflows"
    );
    f.store.end_service_maintenance("dsh-win").unwrap();
    assert!(f.store.guard_service_maintenance("dsh-win").is_ok());
    let _job = f.queued();
    assert!(f.store.begin_service_maintenance("dsh-win").is_err());
    assert!(f.store.begin_service_maintenance("albion-wsl").is_ok());
    assert!(f.store.end_service_maintenance("albion-wsl").is_ok());
}

#[test]
fn summary_sql_failure_is_closed() {
    let mut f = Fixture::new();
    f.store
        .connection
        .execute_batch("DROP TABLE workflows")
        .unwrap();
    assert!(f.store.shared_workflows().is_err());
    assert!(crate::services::dev_digest(&f.store).is_empty());
    assert!(f.store.begin_service_maintenance("dsh-win").is_err());
    assert!(f.store.guard_service_maintenance("dsh-win").is_ok());
}
#[test]
fn review_accepts_bare_keys_without_changing_strings_or_inventing_approval() {
    let review = crate::project_store::parse_review(r#"Acceptance complete. {approved: true, summary: "源码和3条断言通过；文字 approved: false 保持原样。", issues: []}"#).unwrap();
    assert!(review.approved);
    assert!(review.summary.contains("approved: false"));
    assert!(
        crate::project_store::parse_review(r#"{approved: "true", summary: "ok", issues: []}"#)
            .is_err()
    );
    assert!(crate::project_store::parse_review(
        r#"{approved: true, summary: "ok", issues: [], other: 1}"#
    )
    .is_err());
    assert!(crate::project_store::parse_review("Everything seems approved: true").is_err());
}

#[test]
fn checkpoint_blocks_real_secrets_honors_allowlist_and_leaves_recovery_hint() {
    let f = Fixture::new();
    crate::project_worktree::initialize_safe_repository(&f.root).unwrap();
    std::fs::write(f.root.join("notes.md"), "plain notes\n").unwrap();
    // 假令牌拼出来，源码里不留完整字面量。
    let token = format!("ghp_{}", "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8");
    std::fs::write(f.root.join("config.py"), format!("TOKEN = '{token}'\n")).unwrap();
    let error = crate::project_worktree::checkpoint(&f.root).unwrap_err();
    assert!(error.contains("config.py:1（GitHub 令牌）"), "{error}");
    // 没有任何东西被提交：两个文件都还是未跟踪。
    let changes = crate::project_worktree::root_changes(&f.root).unwrap();
    assert!(changes.contains_key("notes.md") && changes.contains_key("config.py"));
    // 用户确认安全后写进本地例外清单，就能照常提交，说明里带撤回方法。
    let git_dir = crate::project_worktree::git_dir(&f.root).unwrap();
    std::fs::write(git_dir.join("info").join(crate::secret_scan::ALLOW_FILE), "# 测试用假令牌\nconfig.py\n").unwrap();
    let summary = crate::project_worktree::checkpoint(&f.root).unwrap().unwrap();
    assert!(summary.contains("git reset --soft"), "{summary}");
    // 已跟踪文件只看新增的行：旧内容里的令牌不会每次都拦。
    std::fs::write(git_dir.join("info").join(crate::secret_scan::ALLOW_FILE), "").unwrap();
    std::fs::write(f.root.join("config.py"), format!("TOKEN = '{token}'\nDEBUG = True\n")).unwrap();
    assert!(crate::project_worktree::checkpoint(&f.root).unwrap().is_some());
}

#[test]
fn root_change_fingerprint_sees_a_second_edit_to_an_already_dirty_file() {
    let f = Fixture::new();
    crate::project_worktree::initialize_safe_repository(&f.root).unwrap();
    std::fs::write(f.root.join("draft.txt"), "one\n").unwrap();
    let before = crate::project_worktree::root_changes(&f.root).unwrap();
    std::fs::write(f.root.join("draft.txt"), "one\ntwo\n").unwrap();
    let after = crate::project_worktree::root_changes(&f.root).unwrap();
    assert_eq!(crate::project_worktree::changed_since(&before, &after), vec!["draft.txt".to_owned()]);
    assert!(crate::project_worktree::changed_since(&after, &after).is_empty());
}
