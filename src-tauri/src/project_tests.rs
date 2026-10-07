//! Real file and state transitions in isolated fixtures, never user projects.
use crate::project_store::*;
use crate::project_tools::Broker;
use crate::store::Store;
use serde_json::json;
use std::path::PathBuf;
use uuid::Uuid;

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
        std::fs::write(root.join("check.py"), "# fixed verification script").unwrap();
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
            .register_project(
                "隔离项目",
                root.to_str().unwrap(),
                &[CheckCommand {
                    name: "实际检查".into(),
                    program: "python".into(),
                    args: vec!["check.py".into()],
                    timeout_seconds: 10,
                }],
            )
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
        let mut codex = self
            .store
            .begin_attempt(&workflow.id, None, "codex-win", "plan", None, None)
            .unwrap();
        codex.output = serde_json::to_string(proposed).unwrap();
        complete(&mut self.store, &codex);
        self.store.save_plan(&workflow.id, proposed).unwrap()
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
fn plan_tasks_can_be_split_between_connected_codex_hermes_and_dsh() {
    let mut codex_task = plan(&["src/complex.rs"]).tasks.remove(0);
    codex_task.agent_id = "codex-win".into();
    let mut dsh_task = plan(&["src/simple.rs"]).tasks.remove(0);
    dsh_task.title = "简单实现".into();
    let mut hermes_task = plan(&["src/reviewed.rs"]).tasks.remove(0);
    hermes_task.agent_id = "hermes-win".into();
    hermes_task.title = "独立实现".into();
    let plan = Plan {
        summary: "按任务难度分工".into(),
        tasks: vec![codex_task, dsh_task, hermes_task],
    };
    let available = vec!["codex-win".into(), "hermes-win".into(), "dsh-win".into()];
    assert!(crate::projects::validate_plan_agents(&plan, &available).is_ok());
    assert!(crate::projects::validate_plan_agents(&plan, &["codex-win".into()]).is_err());
}

#[test]
fn hermes_can_execute_a_task_when_codex_is_the_default_executor() {
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
    assert_eq!(task.agent_id, "hermes-win");
    let attempt = f
        .store
        .begin_attempt(
            &running.id,
            Some(&task.id),
            "hermes-win",
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
                json!({"path":"src/task.rs","content":"hermes","expected_sha256":null})
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
            json!({"path":"check.py","content":"changed","expected_sha256":null})
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
    assert_eq!(
        f.store
            .register_project(&p.name, &p.root, &p.checks)
            .unwrap()
            .id,
        p.id
    );
    assert!(f
        .store
        .register_project("另一个名称", &p.root, &p.checks)
        .is_err());
    assert!(f
        .store
        .register_project("应用数据", f.base.join("data").to_str().unwrap(), &[])
        .is_err());
    assert!(f
        .store
        .register_project("不存在", f.base.join("missing").to_str().unwrap(), &[])
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
    let checks = f.store.project(&f.project).unwrap().checks;
    let child = f
        .store
        .register_project("子项目", nested.to_str().unwrap(), &checks)
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
        .register_project("另项目", other.to_str().unwrap(), &checks)
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
fn project_completion_requires_matching_real_check_evidence() {
    let mut f = Fixture::new();
    let a = f.implement(&["a.py"]);
    complete(&mut f.store, &a);
    let mut verify = f
        .store
        .begin_attempt(&a.workflow_id, None, "hermes-win", "verify", None, None)
        .unwrap();
    verify.status = "completed".into();
    assert!(f.store.checkpoint_attempt(&verify).is_err());
    verify.checks = vec![CheckResult {
        name: "实际检查".into(),
        program: "python".into(),
        args: vec!["different.py".into()],
        exit_code: Some(0),
        timed_out: false,
        duration_ms: 1,
        output: "passed".into(),
    }];
    assert!(f.store.checkpoint_attempt(&verify).is_err());
    verify.checks[0].args = vec!["check.py".into()];
    verify.checks[0].timed_out = true;
    assert!(f.store.checkpoint_attempt(&verify).is_err());
    verify.checks[0].timed_out = false;
    verify.checks[0].exit_code = Some(1);
    assert!(f.store.checkpoint_attempt(&verify).is_err());
    verify.checks[0].exit_code = Some(0);
    f.store.checkpoint_attempt(&verify).unwrap();
    let review = f
        .store
        .begin_attempt(&a.workflow_id, None, "hermes-win", "review", None, None)
        .unwrap();
    assert!(f
        .store
        .finish_workflow(&a.workflow_id, "completed", "", None)
        .is_err());
    let mut review = review;
    review.status = "completed".into();
    review.output = r#"{"approved":false,"summary":"需要修复","issues":["仍有问题"]}"#.into();
    f.store.checkpoint_attempt(&review).unwrap();
    assert!(f
        .store
        .finish_workflow(&a.workflow_id, "completed", "", None)
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
            .finish_workflow(&a.workflow_id, "completed", "通过", None)
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
fn project_broker_blocks_check_script_changes_large_files_and_binary_reads() {
    let mut f = Fixture::new();
    std::fs::write(f.root.join("binary.dat"), [0xff, 0xfe]).unwrap();
    std::fs::write(f.root.join("large.txt"), vec![b'a'; 1_048_577]).unwrap();
    let a = f.implement(&["check.py", "binary.dat", "large.txt", "new.txt"]);
    let mut b = f.broker(&a);
    let check = b.call("hub_read", json!({"path":"check.py"})).unwrap();
    assert!(b
        .call(
            "hub_write",
            json!({"path":"check.py","content":"pass","expected_sha256":check["sha256"]})
        )
        .is_err());
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
fn project_helper_open_does_not_recover_live_workflow_and_verify_is_read_only() {
    let mut f = Fixture::new();
    std::fs::write(f.root.join("a.py"), "source").unwrap();
    let a = f.implement(&["a.py"]);
    let b = f.broker(&a);
    drop(b);
    assert_eq!(f.store.attempt(&a.id).unwrap().status, "starting");
    assert_eq!(f.store.workflow(&a.workflow_id).unwrap().status, "running");
    complete(&mut f.store, &a);
    let verify = f
        .store
        .begin_attempt(&a.workflow_id, None, "hermes-win", "verify", None, None)
        .unwrap();
    let mut b = f.broker(&verify);
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
    let mut verify = f
        .store
        .begin_attempt(&a.workflow_id, None, "hermes-win", "verify", None, None)
        .unwrap();
    verify.status = "failed".into();
    f.store.checkpoint_attempt(&verify).unwrap();
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
        parse_review(&format!("```json\n{body}\n```\n已根据固定检查验收。"))
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
