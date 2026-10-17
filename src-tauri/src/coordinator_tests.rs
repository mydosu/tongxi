//! Coordinator policy and persistence tests. All fixtures live under a unique temp directory.
use crate::{
    coordinator_policy::{self, Reference, Route, Source},
    coordinator_store::TaskRoute,
    models::ModelOption,
    project_store::RoleChoice,
    store::Store,
};
use rusqlite::Connection;
use std::path::PathBuf;
use uuid::Uuid;

struct Fixture {
    store: Store,
    base: PathBuf,
    room: String,
}

impl Fixture {
    fn new() -> Self {
        let base = std::env::temp_dir().join(format!("tongxi-coordinator-test-{}", Uuid::new_v4()));
        let project_root = base.join("project");
        let db = base.join("data").join("hub.db");
        std::fs::create_dir_all(&project_root).unwrap();
        let mut store = Store::open(&db).unwrap();
        let room = store
            .create(
                "项目群",
                "group",
                &["codex-win".into(), "hermes-win".into(), "dsh-win".into()],
            )
            .unwrap()
            .id;
        let project = store
            .register_project("隔离项目", project_root.to_str().unwrap())
            .unwrap()
            .id;
        store.bind_project(&room, Some(&project)).unwrap();
        Self { store, base, room }
    }

    fn workflow(&mut self, request: &str) -> crate::project_store::Workflow {
        self.store
            .start_workflow(&self.room, &Uuid::new_v4().to_string(), request)
            .unwrap()
            .0
    }

    fn coordinated(&mut self, request: &str, agent: &str) -> crate::project_store::Workflow {
        let workflow = self.workflow(request);
        self.store
            .create_coordination(
                &workflow.id,
                &RoleChoice {
                    agent: agent.into(),
                    model: None,
                    effort: None,
                },
            )
            .unwrap();
        self.store.workflow(&workflow.id).unwrap()
    }

    fn close_store(&mut self) {
        let replacement = Store::initialize(
            Connection::open_in_memory().unwrap(),
            PathBuf::from(":memory:"),
        )
        .unwrap();
        let store = std::mem::replace(&mut self.store, replacement);
        drop(store);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.close_store();
        assert!(self
            .base
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("tongxi-coordinator-test-"));
        assert_eq!(self.base.parent().unwrap(), std::env::temp_dir());
        std::fs::remove_dir_all(&self.base).unwrap();
    }
}

fn source(id: &str, kind: &str, text: &str) -> Source {
    Source {
        id: id.into(),
        kind: kind.into(),
        text: text.into(),
    }
}

fn route(key: &str, purpose: &str, agent: &str, source_id: &str, quote: &str) -> Route {
    Route {
        key: key.into(),
        task_id: None,
        purpose: purpose.into(),
        agent_id: agent.into(),
        source_id: source_id.into(),
        quote: quote.into(),
        context_ids: vec![],
        depends_on: vec![],
        model: None,
        effort: None,
        model_source: None,
        subagent: false,
        instructions: None,
        reason: None,
        fresh_session: false,
        clear_context: false,
        allow_subagents: false,
        title: None,
    }
}

fn user_source(id: &str, text: &str) -> Vec<Source> {
    vec![source(id, "user", text)]
}

fn models() -> Vec<ModelOption> {
    vec![ModelOption {
        id: "gpt-6.1".into(),
        name: "GPT-6.1-Luna".into(),
        provider_id: Some("openai".into()),
        provider_name: Some("OpenAI".into()),
        efforts: vec!["low".into(), "medium".into(), "high".into()],
        default_effort: Some("medium".into()),
    }]
}

fn fallback(agent: &str, model: Option<&str>, effort: Option<&str>) -> RoleChoice {
    RoleChoice {
        agent: agent.into(),
        model: model.map(str::to_owned),
        effort: effort.map(str::to_owned),
    }
}

#[test]
fn coordinator_writes_task_briefs_and_ledger_but_no_unknown_fields() {
    let valid = r#"{"action":"delegate","routes":[{"key":"a","purpose":"implementation","agent_id":"dsh-win","source_id":"u1","quote":"DSH负责实现"}]}"#;
    assert_eq!(coordinator_policy::parse(valid).unwrap().action, "delegate");

    // 未知的顶层字段仍然拒绝。
    let top_level = r#"{"action":"delegate","instructions":"追加需求","routes":[{"key":"a","purpose":"implementation","agent_id":"dsh-win","source_id":"u1","quote":"DSH负责实现"}]}"#;
    assert!(coordinator_policy::parse(top_level).is_err());

    // 调度 2.0：任务说明、理由、临时会话、子 agent 许可和计划账本都是正式字段。
    let briefed = r#"{"action":"delegate","ledger":"目标：修好 calc。步骤1 实现（进行中）","routes":[{"key":"a","purpose":"implementation","agent_id":"dsh-win","source_id":"u1","quote":"DSH负责实现","instructions":"目标：……；产出：……；边界：只改 calc.py","reason":"机械修改，用便宜档","fresh_session":true,"allow_subagents":true}]}"#;
    let parsed = coordinator_policy::parse(briefed).unwrap();
    assert_eq!(parsed.ledger.as_deref(), Some("目标：修好 calc。步骤1 实现（进行中）"));
    assert_eq!(parsed.routes[0].instructions.as_deref(), Some("目标：……；产出：……；边界：只改 calc.py"));
    assert!(parsed.routes[0].fresh_session && parsed.routes[0].allow_subagents);

    let long_brief = format!(r#"{{"action":"delegate","routes":[{{"key":"a","purpose":"implementation","agent_id":"dsh-win","source_id":"u1","quote":"DSH负责实现","instructions":"{}"}}]}}"#, "很".repeat(4001));
    assert!(coordinator_policy::parse(&long_brief).is_err(), "任务说明不能无限长");

    let with_withdrawal = r#"{"action":"delegate","withdraw":[{"task_id":"task-1","source_id":"user-correction","quote":"取消旧任务"}],"routes":[{"key":"a","purpose":"implementation","agent_id":"dsh-win","source_id":"u1","quote":"DSH负责实现"}]}"#;
    let parsed = coordinator_policy::parse(with_withdrawal).unwrap();
    assert_eq!(parsed.withdraw.len(), 1);
}

#[test]
fn names_the_user_gave_members_count_as_naming_them() {
    let sources = user_source("u2", "算了，commit和push还是交由codex来做，克大姐和克小姐进行改动即可");
    let quote = "算了，commit和push还是交由codex来做，克大姐和克小姐进行改动即可";
    let refactor = route("a_impl", "implementation", "claude-win", "u2", quote);
    assert!(coordinator_policy::check_assignment(&refactor, &sources).is_err(), "只认产品名时会误以为只点了 Codex");
    let aliases = vec![("克大姐".to_owned(), "claude-win".to_owned()), ("克小姐".to_owned(), "claude-win".to_owned())];
    assert!(coordinator_policy::check_assignment_with(&refactor, &sources, &aliases).is_ok());
    let push = route("push", "closeout", "codex-win", "u2", quote);
    assert!(coordinator_policy::check_assignment_with(&push, &sources, &aliases).is_ok());
    let hermes = route("h", "closeout", "hermes-win", "u2", quote);
    assert!(coordinator_policy::check_assignment_with(&hermes, &sources, &aliases).is_err());
}

#[test]
fn coordinator_decides_tradeoffs_itself_instead_of_blocking_on_the_user() {
    let decided = r#"{"action":"delegate","decisions":[{"question":"守卫怎么处理","choice":"保留一个最小守卫","reason":"可回退"}],"routes":[{"key":"a","purpose":"implementation","agent_id":"dsh-win","source_id":"u1","quote":"DSH负责实现"}]}"#;
    let parsed = coordinator_policy::parse(decided).unwrap();
    assert_eq!(parsed.decisions[0].choice, "保留一个最小守卫");
    let blocking = r#"{"action":"ask_user","decisions":[{"question":"守卫怎么处理","choice":"保留"}],"question":{"prompt":"要推送吗","options":["推","不推"]}}"#;
    assert!(coordinator_policy::parse(blocking).is_err(), "能自己定的先推进，不和提问混在一起");
    let empty = r#"{"action":"finish","decisions":[{"question":"","choice":"保留"}]}"#;
    assert!(coordinator_policy::parse(empty).is_err());
    assert!(coordinator_policy::MANAGER_INSTRUCTION.contains("【自主推进】"));
    assert!(coordinator_policy::MANAGER_INSTRUCTION.contains("decisions?:[{question:string,choice:string,reason?:string}]"));
}

#[test]
fn routes_require_verbatim_user_quotes_and_honor_named_member() {
    let sources = user_source("u1", "Codex负责方案；DSH负责实现。");
    let solution = route("solution", "solution", "codex-win", "u1", "Codex负责方案");
    assert!(coordinator_policy::check_assignment(&solution, &sources).is_ok());

    let appended = route(
        "bad",
        "solution",
        "codex-win",
        "u1",
        "Codex负责方案，同时删除旧数据",
    );
    assert!(coordinator_policy::check_assignment(&appended, &sources).is_err());

    let reassigned = route("bad", "solution", "hermes-win", "u1", "Codex负责方案");
    assert!(coordinator_policy::check_assignment(&reassigned, &sources).is_err());

    let context_only = vec![source("member-result", "result", "Codex负责方案")];
    assert!(coordinator_policy::check_assignment(
        &route(
            "bad",
            "solution",
            "codex-win",
            "member-result",
            "Codex负责方案"
        ),
        &context_only
    )
    .is_err());

    let multi_assignment = user_source("u2", "Codex负责方案，DSH负责实现。");
    let bundled = route(
        "bad",
        "solution",
        "hermes-win",
        "u2",
        "Codex负责方案，DSH负责实现",
    );
    assert!(coordinator_policy::check_assignment(&bundled, &multi_assignment).is_err());
}

#[test]
fn solution_route_cannot_quote_later_implementation_and_verification_work() {
    let text = "先由Codex只读检查calc.py并给出方案。方案返回后，Codex再只修改calc.py。随后DSH实现classify.py并运行python -B check.py。最后由Hermes整理收尾文档。";
    let sources = user_source("u-scoped-route", text);
    let broad = route(
        "solution-all",
        "solution",
        "codex-win",
        "u-scoped-route",
        text,
    );
    assert!(coordinator_policy::check_assignment(&broad, &sources).is_err());
}

#[test]
fn explicit_user_model_and_effort_override_project_defaults_but_unmentioned_values_inherit() {
    let text = "Codex使用GPT-6.1-Luna、low强度完成验证。";
    let sources = user_source("u1", text);
    let mut selected = route("verify", "verification", "codex-win", "u1", text);
    selected.model = Some("gpt-6.1".into());
    selected.effort = Some("low".into());
    selected.model_source = Some(Reference {
        source_id: "u1".into(),
        quote: text.into(),
    });
    assert_eq!(
        coordinator_policy::overrides(
            &selected,
            &sources,
            &models(),
            &fallback("codex-win", Some("old-model"), Some("high"))
        )
        .unwrap(),
        (Some("gpt-6.1".into()), Some("low".into()))
    );

    let inherited = route("reply", "reply", "codex-win", "u2", "Codex回复进度");
    let inherited_sources = user_source("u2", "Codex回复进度。");
    assert_eq!(
        coordinator_policy::overrides(
            &inherited,
            &inherited_sources,
            &models(),
            &fallback("codex-win", Some("gpt-6.1"), Some("high"))
        )
        .unwrap(),
        (None, None)
    );

    let invented = {
        let mut value = route("invented", "verification", "codex-win", "u1", text);
        // 目录里没有的模型，谁选都不行。
        value.model = Some("unlisted-model".into());
        value.model_source = Some(Reference {
            source_id: "u1".into(),
            quote: text.into(),
        });
        value
    };
    assert!(coordinator_policy::overrides(
        &invented,
        &sources,
        &models(),
        &fallback("codex-win", None, None)
    )
    .is_err());
}

#[test]
fn filenames_and_function_names_do_not_override_model_effort() {
    let text = "检查max.py里的max(value)逻辑，保留当前实现成员的项目模型与强度。";
    let sources = user_source("u-max", text);
    let route = route("max-fix", "implementation", "codex-win", "u-max", text);
    assert_eq!(
        coordinator_policy::overrides(
            &route,
            &sources,
            &models(),
            &fallback("codex-win", Some("gpt-6.1"), Some("low")),
        )
        .unwrap(),
        (None, None)
    );
}

#[test]
fn a_named_dsh_model_does_not_turn_max_path_into_an_effort_override() {
    let text = "DSH使用DeepSeek V4.1 Flash模型新增max.py，保留max(value)作为函数名。";
    let sources = user_source("u-dsh-max", text);
    let route = route("dsh-module", "implementation", "dsh-win", "u-dsh-max", text);
    let model_id = "[\"command-code-daily-2\",\"deepseek/deepseek-v4.1-flash\"]";
    let catalog = vec![ModelOption {
        id: model_id.into(),
        name: "DeepSeek V4.1 Flash".into(),
        provider_id: Some("command-code-daily-2".into()),
        provider_name: Some("Command Code Daily Two".into()),
        efforts: vec!["low".into(), "high".into()],
        default_effort: Some("low".into()),
    }];
    assert_eq!(
        coordinator_policy::overrides(
            &route,
            &sources,
            &catalog,
            &fallback("dsh-win", Some(model_id), Some("low")),
        )
        .unwrap(),
        (Some(model_id.into()), None)
    );
}

#[test]
fn explicitly_requested_gpt_six_sol_high_is_preserved() {
    let text = "Codex使用OpenAI GPT-6.1 Sol模型，reasoning effort为high。";
    let sources = user_source("u-sol", text);
    let mut route = route("sol-check", "verification", "codex-win", "u-sol", text);
    route.model = Some("gpt-6.1-sol".into());
    route.effort = Some("high".into());
    route.model_source = Some(Reference {
        source_id: "u-sol".into(),
        quote: text.into(),
    });
    let mut catalog = models();
    catalog.push(ModelOption {
        id: "gpt-6.1-sol".into(),
        name: "GPT-6.1 Sol".into(),
        provider_id: Some("openai".into()),
        provider_name: Some("OpenAI".into()),
        efforts: vec!["high".into()],
        default_effort: Some("high".into()),
    });
    assert_eq!(
        coordinator_policy::overrides(
            &route,
            &sources,
            &catalog,
            &fallback("codex-win", Some("gpt-6.1"), Some("low")),
        )
        .unwrap(),
        (Some("gpt-6.1-sol".into()), Some("high".into()))
    );
}

#[test]
fn explicit_model_change_selects_target_model_and_last_requested_effort() {
    let text = "Codex从OpenAI GPT-6.1 Sol high改为OpenAI GPT-6.1 Luna low。";
    let sources = user_source("u-switch", text);
    let mut route = route(
        "codex-switch",
        "implementation",
        "codex-win",
        "u-switch",
        text,
    );
    route.model = Some("gpt-6.1".into());
    route.effort = Some("low".into());
    route.model_source = Some(Reference {
        source_id: "u-switch".into(),
        quote: text.into(),
    });
    let mut catalog = models();
    catalog.push(ModelOption {
        id: "gpt-6.1-sol".into(),
        name: "GPT-6.1 Sol".into(),
        provider_id: Some("openai".into()),
        provider_name: Some("OpenAI".into()),
        efforts: vec!["high".into()],
        default_effort: Some("high".into()),
    });
    assert_eq!(
        coordinator_policy::overrides(
            &route,
            &sources,
            &catalog,
            &fallback("codex-win", Some("gpt-6.1-sol"), Some("high")),
        )
        .unwrap(),
        (Some("gpt-6.1".into()), Some("low".into()))
    );
}

#[test]
fn explicit_daily_two_source_selects_that_provider_when_route_model_is_omitted() {
    let text = "Hermes使用DeepSeek V4.1 Flash，通过Command Code Daily Two进行验证。";
    let sources = user_source("u-daily-two", text);
    let route = route(
        "hermes-daily-two",
        "verification",
        "hermes-win",
        "u-daily-two",
        text,
    );
    let daily_one = "[\"command-code-daily\",\"deepseek/deepseek-v4.1-flash\"]";
    let daily_two = "[\"command-code-daily-2\",\"deepseek/deepseek-v4.1-flash\"]";
    let catalog = vec![
        ModelOption {
            id: daily_one.into(),
            name: "DeepSeek V4.1 Flash · Command Code Daily One".into(),
            provider_id: Some("command-code-daily".into()),
            provider_name: Some("Command Code Daily One".into()),
            efforts: vec!["low".into()],
            default_effort: Some("low".into()),
        },
        ModelOption {
            id: daily_two.into(),
            name: "DeepSeek V4.1 Flash · Command Code Daily Two".into(),
            provider_id: Some("command-code-daily-2".into()),
            provider_name: Some("Command Code Daily Two".into()),
            efforts: vec!["low".into()],
            default_effort: Some("low".into()),
        },
    ];
    assert_eq!(
        coordinator_policy::overrides(
            &route,
            &sources,
            &catalog,
            &fallback("hermes-win", Some(daily_one), Some("low")),
        )
        .unwrap(),
        (Some(daily_two.into()), None)
    );

    let mut wrong_provider = route;
    wrong_provider.model = Some(daily_one.into());
    assert!(coordinator_policy::overrides(
        &wrong_provider,
        &sources,
        &catalog,
        &fallback("hermes-win", Some(daily_one), Some("low")),
    )
    .is_err());
}

#[test]
fn coordinator_model_selection_keeps_project_provider_when_user_names_only_model_family() {
    let text = "Codex使用GPT-6.1-Luna进行验证。";
    let sources = user_source("u1", text);
    let catalog = vec![
        ModelOption {
            id: "openai:gpt-6.1".into(),
            name: "GPT-6.1-Luna".into(),
            provider_id: Some("openai".into()),
            provider_name: Some("OpenAI".into()),
            efforts: vec!["low".into()],
            default_effort: None,
        },
        ModelOption {
            id: "azure:gpt-6.1".into(),
            name: "GPT-6.1-Luna".into(),
            provider_id: Some("azure".into()),
            provider_name: Some("Azure".into()),
            efforts: vec!["low".into()],
            default_effort: None,
        },
    ];
    let choice = route("verify", "verification", "codex-win", "u1", text);
    assert_eq!(
        coordinator_policy::overrides(
            &choice,
            &sources,
            &catalog,
            &fallback("codex-win", Some("azure:gpt-6.1"), None)
        )
        .unwrap(),
        (Some("azure:gpt-6.1".into()), None)
    );

    let mut changed_provider = choice;
    changed_provider.model = Some("openai:gpt-6.1".into());
    assert!(coordinator_policy::overrides(
        &changed_provider,
        &sources,
        &catalog,
        &fallback("codex-win", Some("azure:gpt-6.1"), None)
    )
    .is_err());
}

#[test]
fn coordinator_routes_are_idempotent_and_completed_solution_is_never_requeued() {
    let mut f = Fixture::new();
    let request = "DSH给出方案。";
    let workflow = f.coordinated(request, "dsh-win");
    let first = route(
        "plan-1",
        "solution",
        "dsh-win",
        &workflow.user_message_id,
        request,
    );
    let ids = f
        .store
        .enqueue_routes(&workflow.id, &[(first.clone(), None, None)])
        .unwrap();
    assert_eq!(ids.len(), 1);
    f.store
        .connection
        .execute(
            "UPDATE project_tasks SET status='completed',output='方案已完成' WHERE id=?1",
            [&ids[0]],
        )
        .unwrap();

    let duplicate = route(
        "plan-again",
        "solution",
        "dsh-win",
        &workflow.user_message_id,
        request,
    );
    // 重复派发已完成的工作直接报错讲清原因，不再悄悄当成派成了（调度会以为活已经排上）。
    let again = f
        .store
        .enqueue_routes(&workflow.id, &[(duplicate.clone(), None, None)])
        .unwrap_err();
    assert!(again.contains("不能重复派发"), "{again}");
    assert_eq!(f.store.workflow(&workflow.id).unwrap().tasks.len(), 1);
    // 写了不同的任务名就是另一项工作，新开一项。
    let mut another = duplicate;
    another.title = Some("补一份回滚方案".into());
    let other = f.store.enqueue_routes(&workflow.id, &[(another, None, None)]).unwrap();
    assert_ne!(other, ids);
    assert_eq!(f.store.task(&other[0]).unwrap().title, "方案 · 补一份回滚方案");
    assert_eq!(f.store.workflow(&workflow.id).unwrap().tasks.len(), 2);
    assert_eq!(f.store.task(&ids[0]).unwrap().status, "completed");

    let mut explicit_resume = first;
    explicit_resume.task_id = Some(ids[0].clone());
    assert!(f
        .store
        .enqueue_routes(&workflow.id, &[(explicit_resume, None, None)])
        .is_err());
}

#[test]
fn v19_upgrade_lifts_one_task_per_quote_and_keeps_existing_routes() {
    let mut f = Fixture::new();
    let request = "DSH实现核心";
    let workflow = f.coordinated(request, "hermes-win");
    let task = f
        .store
        .enqueue_routes(
            &workflow.id,
            &[(
                route(
                    "impl",
                    "implementation",
                    "dsh-win",
                    &workflow.user_message_id,
                    request,
                ),
                None,
                None,
            )],
        )
        .unwrap()[0]
        .clone();
    // 退回 v18 的表结构：一句原文只能一个任务。
    f.store
        .connection
        .execute_batch(
            "CREATE TABLE old_routes AS SELECT * FROM coordination_routes;
             DROP TABLE coordination_routes;
             CREATE TABLE coordination_routes (
               task_id TEXT PRIMARY KEY REFERENCES project_tasks(id) ON DELETE CASCADE,workflow_id TEXT NOT NULL REFERENCES workflows(id) ON DELETE CASCADE,
               purpose TEXT NOT NULL,source_id TEXT NOT NULL,quote TEXT NOT NULL,context_ids TEXT NOT NULL,
               UNIQUE(workflow_id,purpose,source_id,quote));
             INSERT INTO coordination_routes SELECT task_id,workflow_id,purpose,source_id,quote,context_ids FROM old_routes;
             DROP TABLE old_routes;
             PRAGMA user_version=18;",
        )
        .unwrap();
    let db = f.store.path.clone();
    f.close_store();
    f.store = Store::open(&db).unwrap();
    let sql: String = f
        .store
        .connection
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE type='table' AND name='coordination_routes'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!sql.contains("UNIQUE"), "{sql}");
    let routes = f.store.coordination_routes(&workflow.id).unwrap();
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].task_id, task);
    assert_eq!(f.store.task(&task).unwrap().status, "queued");
}

#[test]
fn failed_verification_gates_closeout_until_a_passing_recheck() {
    let mut f = Fixture::new();
    let request = "DSH实现核心；Codex负责验证；Hermes收尾";
    let workflow = f.coordinated(request, "hermes-win");
    let source = workflow.user_message_id.clone();
    let ids = f
        .store
        .enqueue_routes(
            &workflow.id,
            &[
                (
                    route("impl", "implementation", "dsh-win", &source, "DSH实现核心"),
                    None,
                    None,
                ),
                (
                    route(
                        "verify",
                        "verification",
                        "codex-win",
                        &source,
                        "Codex负责验证",
                    ),
                    None,
                    None,
                ),
                (
                    route("close", "closeout", "hermes-win", &source, "Hermes收尾"),
                    None,
                    None,
                ),
            ],
        )
        .unwrap();
    let set = |f: &mut Fixture, id: &str, output: &str| {
        f.store
            .connection
            .execute(
                "UPDATE project_tasks SET status='completed',output=?2 WHERE id=?1",
                rusqlite::params![id, output],
            )
            .unwrap();
    };
    let gate = |f: &Fixture| {
        crate::project_coordinator::failed_verification(
            &f.store.workflow(&workflow.id).unwrap().tasks,
            &f.store.coordination_routes(&workflow.id).unwrap(),
        )
    };
    set(&mut f, &ids[0], "已实现");
    assert_eq!(gate(&f), None);
    set(
        &mut f,
        &ids[1],
        "## 问题清单
1. 模板仍在

**验证结论：未通过**",
    );
    assert_eq!(gate(&f), Some(ids[1].clone()));

    // 返修后复核通过：最新一次验证通过，关卡解除。
    let mut rework = route(
        "rework",
        "implementation",
        "dsh-win",
        &source,
        "DSH实现核心",
    );
    rework.context_ids = vec![ids[1].clone()];
    let fixed = f
        .store
        .enqueue_routes(&workflow.id, &[(rework, None, None)])
        .unwrap()[0]
        .clone();
    set(&mut f, &fixed, "已返修");
    let mut recheck = route(
        "recheck",
        "verification",
        "codex-win",
        &source,
        "Codex负责验证",
    );
    recheck.context_ids = vec![fixed];
    let rechecked = f
        .store
        .enqueue_routes(&workflow.id, &[(recheck, None, None)])
        .unwrap()[0]
        .clone();
    set(
        &mut f,
        &rechecked,
        "全部复测通过。
验证结论：通过",
    );
    assert_eq!(gate(&f), None);
}

#[test]
fn failed_verification_routes_rework_and_recheck_with_a_round_limit() {
    let mut f = Fixture::new();
    let request = "DSH实现核心；Codex负责验证";
    let workflow = f.coordinated(request, "hermes-win");
    let source = workflow.user_message_id.clone();
    let complete = |f: &mut Fixture, id: &str, output: &str| {
        f.store
            .connection
            .execute(
                "UPDATE project_tasks SET status='completed',output=?2 WHERE id=?1",
                rusqlite::params![id, output],
            )
            .unwrap();
        // 记一次早于用户回答（answered_at=2）的运行，供“回答之后的轮次”计数。
        f.store
            .connection
            .execute(
                "INSERT INTO project_attempts(id,workflow_id,task_id,agent_id,stage,status,created_at) SELECT ?1||'-run',workflow_id,id,'dsh-win','implement','completed',1 FROM project_tasks WHERE id=?1",
                [id],
            )
            .unwrap();
    };
    let implement = route("impl", "implementation", "dsh-win", &source, "DSH实现核心");
    let verify = route(
        "verify",
        "verification",
        "codex-win",
        &source,
        "Codex负责验证",
    );
    let first = f
        .store
        .enqueue_routes(&workflow.id, &[(implement.clone(), None, None)])
        .unwrap()[0]
        .clone();
    complete(&mut f, &first, "已实现");
    let check = f
        .store
        .enqueue_routes(&workflow.id, &[(verify.clone(), None, None)])
        .unwrap()[0]
        .clone();
    complete(
        &mut f,
        &check,
        "验证结论：未通过
1. 边界值返回错误",
    );

    // 同一原文、带上更新的验证结果：开一轮返修。
    let mut rework = implement.clone();
    rework.key = "rework-1".into();
    rework.context_ids = vec![check.clone()];
    rework.title = Some("修好 0 的边界值".into());
    let fixed = f
        .store
        .enqueue_routes(&workflow.id, &[(rework.clone(), None, None)])
        .unwrap()[0]
        .clone();
    assert_ne!(fixed, first);
    let task = f.store.task(&fixed).unwrap();
    assert_eq!(task.status, "queued");
    assert_eq!(task.title, "返修 · 修好 0 的边界值", "调度写了任务名就按任务名显示");
    // 不带新结果的重复派发仍然幂等：返回最新那一轮，不再新建。
    let again = f
        .store
        .enqueue_routes(&workflow.id, &[(implement.clone(), None, None)])
        .unwrap();
    assert_eq!(again, vec![fixed.clone()]);
    complete(&mut f, &fixed, "已按问题返修");

    // 返修完成后复核。
    let mut recheck = verify.clone();
    recheck.key = "recheck-1".into();
    recheck.context_ids = vec![fixed.clone()];
    let rechecked = f
        .store
        .enqueue_routes(&workflow.id, &[(recheck, None, None)])
        .unwrap()[0]
        .clone();
    assert!(f.store.task(&rechecked).unwrap().title.starts_with("复核"));
    complete(
        &mut f,
        &rechecked,
        "验证结论：未通过
2. 仍有问题",
    );

    // 第二轮返修允许，第三轮拒绝并要求询问用户。
    let mut second = implement.clone();
    second.key = "rework-2".into();
    second.context_ids = vec![rechecked.clone()];
    let second_id = f
        .store
        .enqueue_routes(&workflow.id, &[(second, None, None)])
        .unwrap()[0]
        .clone();
    complete(&mut f, &second_id, "第二次返修");
    let mut recheck2 = verify.clone();
    recheck2.key = "recheck-2".into();
    recheck2.context_ids = vec![second_id.clone()];
    let recheck2_id = f
        .store
        .enqueue_routes(&workflow.id, &[(recheck2, None, None)])
        .unwrap()[0]
        .clone();
    complete(&mut f, &recheck2_id, "验证结论：未通过");
    let mut third = implement;
    third.key = "rework-3".into();
    third.context_ids = vec![recheck2_id];
    let error = f
        .store
        .enqueue_routes(&workflow.id, &[(third.clone(), None, None)])
        .unwrap_err();
    assert!(error.contains("ask_user"), "{error}");

    // 用户回答了管家的提问（同意继续）后，再放行一批返修。
    f.store
        .connection
        .execute(
            "INSERT INTO project_questions(id,workflow_id,conversation_id,agent_id,prompt,options,status,answer,created_at,answered_at) VALUES('q-continue',?1,?2,'hermes-win','继续吗？','[]','answered','继续修',1,2)",
            rusqlite::params![workflow.id, workflow.conversation_id],
        )
        .unwrap();
    let resumed = f
        .store
        .enqueue_routes(&workflow.id, &[(third.clone(), None, None)])
        .unwrap()[0]
        .clone();
    assert!(f.store.task(&resumed).unwrap().title.starts_with("返修"));
    complete(&mut f, &resumed, "第三次返修");
    f.store.connection.execute("UPDATE project_attempts SET created_at=3 WHERE task_id=?1", [&resumed]).unwrap();
    let mut recheck3 = verify.clone();
    recheck3.key = "recheck-3".into();
    recheck3.context_ids = vec![resumed.clone()];
    let recheck3_id = f.store.enqueue_routes(&workflow.id, &[(recheck3, None, None)]).unwrap()[0].clone();
    complete(&mut f, &recheck3_id, "验证结论：未通过");
    f.store.connection.execute("UPDATE project_attempts SET created_at=3 WHERE task_id=?1", [&recheck3_id]).unwrap();
    // 回答之后已返修 1 轮，还能再来 1 轮；之后必须再次询问用户。
    let mut fourth = third.clone();
    fourth.key = "rework-4".into();
    fourth.context_ids = vec![recheck3_id];
    let fourth_id = f.store.enqueue_routes(&workflow.id, &[(fourth.clone(), None, None)]).unwrap()[0].clone();
    complete(&mut f, &fourth_id, "第四次返修");
    f.store.connection.execute("UPDATE project_attempts SET created_at=3 WHERE task_id=?1", [&fourth_id]).unwrap();
    let mut recheck4 = verify.clone();
    recheck4.key = "recheck-4".into();
    recheck4.context_ids = vec![fourth_id];
    let recheck4_id = f.store.enqueue_routes(&workflow.id, &[(recheck4, None, None)]).unwrap()[0].clone();
    complete(&mut f, &recheck4_id, "验证结论：未通过");
    f.store.connection.execute("UPDATE project_attempts SET created_at=3 WHERE task_id=?1", [&recheck4_id]).unwrap();
    let mut fifth = fourth;
    fifth.key = "rework-5".into();
    fifth.context_ids = vec![recheck4_id];
    let error = f.store.enqueue_routes(&workflow.id, &[(fifth, None, None)]).unwrap_err();
    assert!(error.contains("ask_user"), "{error}");
}

#[test]
fn dependencies_block_early_start_and_allow_it_after_predecessor_completion() {
    let mut f = Fixture::new();
    let request = "DSH实现核心；DSH实现补充并等待核心；";
    let workflow = f.coordinated(request, "hermes-win");
    let first = route(
        "core",
        "implementation",
        "dsh-win",
        &workflow.user_message_id,
        "DSH实现核心",
    );
    let mut second = route(
        "extra",
        "implementation",
        "dsh-win",
        &workflow.user_message_id,
        "DSH实现补充并等待核心",
    );
    second.depends_on.push("core".into());
    let ids = f
        .store
        .enqueue_routes(&workflow.id, &[(first, None, None), (second, None, None)])
        .unwrap();
    let job = f.store.workflow(&workflow.id).unwrap();
    let manager = f
        .store
        .begin_coordinated_attempt(&workflow.id, None, "hermes-win", None, None)
        .unwrap();
    let second_task = job
        .tasks
        .iter()
        .find(|task| task.id == ids[1])
        .unwrap()
        .clone();
    assert!(f
        .store
        .begin_coordinated_attempt(&workflow.id, Some(&second_task), "dsh-win", None, None)
        .is_err());

    let first_task = job
        .tasks
        .iter()
        .find(|task| task.id == ids[0])
        .unwrap()
        .clone();
    let mut completed = f
        .store
        .begin_coordinated_attempt(&workflow.id, Some(&first_task), "dsh-win", None, None)
        .unwrap();
    completed.status = "completed".into();
    completed.output = "核心完成".into();
    f.store.checkpoint_attempt(&completed).unwrap();
    f.store
        .finish_coordinated_task(&first_task.id, &completed)
        .unwrap();
    assert!(f
        .store
        .begin_coordinated_attempt(&workflow.id, Some(&second_task), "dsh-win", None, None)
        .is_ok());
    assert_eq!(
        manager.activity.as_deref(),
        Some(coordinator_policy::MANAGER_ACTIVITY)
    );
}

#[test]
fn manager_attempt_does_not_block_its_worker_and_same_agent_worker_is_serialized() {
    let mut f = Fixture::new();
    let request = "DSH实现第一项；DSH实现第二项；Codex验证结果。";
    let workflow = f.coordinated(request, "dsh-win");
    let first = route(
        "one",
        "implementation",
        "dsh-win",
        &workflow.user_message_id,
        "DSH实现第一项",
    );
    let second = route(
        "two",
        "implementation",
        "dsh-win",
        &workflow.user_message_id,
        "DSH实现第二项",
    );
    let review = route(
        "review",
        "verification",
        "codex-win",
        &workflow.user_message_id,
        "Codex验证结果",
    );
    let ids = f
        .store
        .enqueue_routes(
            &workflow.id,
            &[
                (first, None, None),
                (second, None, None),
                (review, None, None),
            ],
        )
        .unwrap();
    let manager = f
        .store
        .begin_coordinated_attempt(&workflow.id, None, "dsh-win", None, None)
        .unwrap();
    let tasks = f.store.workflow(&workflow.id).unwrap().tasks;
    let by_id = |id: &String| tasks.iter().find(|task| &task.id == id).unwrap();
    let worker = f
        .store
        .begin_coordinated_attempt(&workflow.id, Some(by_id(&ids[0])), "dsh-win", None, None)
        .unwrap();
    assert!(f
        .store
        .begin_coordinated_attempt(&workflow.id, Some(by_id(&ids[1])), "dsh-win", None, None)
        .is_err());
    let independent = f
        .store
        .begin_coordinated_attempt(&workflow.id, Some(by_id(&ids[2])), "codex-win", None, None)
        .unwrap();
    assert_eq!(
        manager.activity.as_deref(),
        Some(coordinator_policy::MANAGER_ACTIVITY)
    );
    assert_eq!(worker.agent_id, "dsh-win");
    assert_eq!(independent.agent_id, "codex-win");
}

#[test]
fn first_manager_attempt_marks_workflow_running_before_asking_user() {
    let mut f = Fixture::new();
    let workflow = f.coordinated("请先确认需求范围。", "codex-win");
    assert_eq!(workflow.status, "queued");
    let manager = f
        .store
        .begin_coordinated_attempt(&workflow.id, None, "codex-win", None, None)
        .unwrap();
    assert_eq!(f.store.workflow(&workflow.id).unwrap().status, "running");

    let question = crate::project_questions::post(
        &f.store.path,
        &manager.id,
        "你希望保留现有函数名称吗？",
        vec!["保留".into(), "重命名".into()],
    )
    .unwrap();
    assert_eq!(
        f.store
            .project_questions(&workflow.conversation_id)
            .unwrap()[0]
            .id,
        question
    );
}

#[test]
fn coordinated_native_completion_keeps_task_running_until_post_merge_finalization() {
    let mut f = Fixture::new();
    let workflow = f.coordinated("DSH实现一个新模块。", "hermes-win");
    let task_route = route(
        "implementation",
        "implementation",
        "dsh-win",
        &workflow.user_message_id,
        "DSH实现一个新模块",
    );
    let task_id = f
        .store
        .enqueue_routes(&workflow.id, &[(task_route, None, None)])
        .unwrap()
        .remove(0);
    let task = f.store.task(&task_id).unwrap();
    let mut native = f
        .store
        .begin_coordinated_attempt(
            &workflow.id,
            Some(&task),
            "dsh-win",
            Some("model".into()),
            Some("low".into()),
        )
        .unwrap();
    native.status = "completed".into();
    native.native_turn_id = Some("native-turn".into());
    native.output = "实现已完成".into();
    f.store.checkpoint_attempt(&native).unwrap();
    assert_eq!(f.store.attempt(&native.id).unwrap().status, "completed");
    assert_eq!(
        f.store.task(&task_id).unwrap().status,
        "running",
        "native completion must wait for successful merge"
    );

    f.store.finish_coordinated_task(&task_id, &native).unwrap();
    assert_eq!(f.store.task(&task_id).unwrap().status, "completed");
}

#[test]
fn coordinator_sessions_are_persisted_per_workflow_and_isolated_from_worker_ids() {
    let mut f = Fixture::new();
    let first = f.coordinated("Codex制定方案。", "codex-win");
    let first_cwd = f
        .base
        .join("coordinators")
        .join(&first.id)
        .join("codex-win")
        .join("workspace");
    f.store
        .bind_manager_session(&first.id, "codex-win", "manager-session-a", &first_cwd)
        .unwrap();
    let worker_chats = f.store.ensure_project_agent_chats(&f.room).unwrap();
    let codex_chat = worker_chats
        .iter()
        .find(|chat| chat.agent_id == "codex-win")
        .unwrap();
    f.store.connection.execute("UPDATE sessions SET native_session_id='worker-session' WHERE conversation_id=?1 AND agent_id='codex-win'", [&codex_chat.conversation_id]).unwrap();

    f.store
        .connection
        .execute(
            "UPDATE workflows SET status='interrupted' WHERE id=?1",
            [&first.id],
        )
        .unwrap();
    let second = f.coordinated("Hermes接收新一轮方案。", "hermes-win");
    let second_cwd = f
        .base
        .join("coordinators")
        .join(&second.id)
        .join("hermes-win")
        .join("workspace");
    f.store
        .bind_manager_session(&second.id, "hermes-win", "manager-session-b", &second_cwd)
        .unwrap();

    assert_eq!(
        f.store.manager_session(&first.id).unwrap().as_deref(),
        Some("manager-session-a")
    );
    let worker_session: Option<String> = f.store.connection.query_row("SELECT native_session_id FROM sessions WHERE conversation_id=?1 AND agent_id='codex-win'", [&codex_chat.conversation_id], |row| row.get(0)).unwrap();
    assert_eq!(worker_session.as_deref(), Some("worker-session"));
    assert_ne!(
        worker_session.as_deref(),
        f.store.manager_session(&first.id).unwrap().as_deref()
    );
    assert_eq!(
        f.store.manager_session(&second.id).unwrap().as_deref(),
        Some("manager-session-b")
    );
    assert_ne!(first.id, second.id);
    assert!(f.store.workflow(&first.id).unwrap().coordination.is_some());
    assert!(f.store.workflow(&second.id).unwrap().coordination.is_some());
}

#[test]
fn route_assignment_must_target_a_current_group_member() {
    let mut f = Fixture::new();
    let request = "Hermes验证结果。";
    let workflow = f.coordinated(request, "dsh-win");
    f.store
        .connection
        .execute(
            "DELETE FROM members WHERE conversation_id=?1 AND agent_id='hermes-win'",
            [&f.room],
        )
        .unwrap();
    let assignment = route(
        "verify",
        "verification",
        "hermes-win",
        &workflow.user_message_id,
        request,
    );
    assert!(f
        .store
        .enqueue_routes(&workflow.id, &[(assignment, None, None)])
        .is_err());
    assert!(f.store.workflow(&workflow.id).unwrap().tasks.is_empty());
}

#[test]
fn legacy_schema_16_workflow_survives_upgrade_with_no_coordinator_record() {
    let mut f = Fixture::new();
    let workflow = f.workflow("旧版本的协作请求");
    f.store
        .connection
        .execute_batch(
            "DROP TABLE coordinator_sessions;
         DROP TABLE coordination_routes;
         DROP TABLE coordination_runs;
         DROP TABLE project_coordinator_settings;
         PRAGMA user_version=16;",
        )
        .unwrap();
    f.close_store();
    f.store = Store::open(&f.base.join("data").join("hub.db")).unwrap();

    let migrated = f.store.workflow(&workflow.id).unwrap();
    let version: i64 = f
        .store
        .connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, crate::store::SCHEMA_VERSION);
    assert_eq!(migrated.request, "旧版本的协作请求");
    assert!(migrated.coordination.is_none());
    assert!(f.store.project_coordinator(&f.room).unwrap().is_none());
    assert!(f
        .store
        .coordination_routes(&workflow.id)
        .unwrap()
        .is_empty());
}

#[test]
fn manager_paths_and_codex_manager_profile_are_scoped_and_deny_project_tools() {
    let workflow = Uuid::new_v4().to_string();
    let db = std::env::temp_dir().join("tongxi-test-data").join("hub.db");
    let codex = crate::coordinator_clients::codex_arguments().join(" ");
    for denied in [
        "approval_reviewer=\"user\"",
        "mcp_servers={}",
        "plugins={}",
        "features.shell_tool=false",
        "features.unified_exec=false",
        "features.apply_patch_freeform=false",
        "features.collab=false",
        "features.multi_agent=false",
        "features.skills=false",
        "web_search=\"disabled\"",
    ] {
        assert!(
            codex.contains(denied),
            "missing Codex manager restriction: {denied}"
        );
    }
    assert!(
        crate::coordinator_clients::manager_directory(&db, &workflow, "codex-win")
            .unwrap()
            .to_string_lossy()
            .contains(&workflow)
    );
    assert!(crate::coordinator_clients::manager_directory(&db, &workflow, "albion-wsl").is_err());
    assert!(crate::coordinator_clients::manager_directory(&db, "not-a-uuid", "codex-win").is_err());
}

#[test]
fn native_manager_lifecycle_events_are_not_tools_but_real_tools_are_retained() {
    for kind in [
        "subAgentActivity",
        "enteredReviewMode",
        "exitedReviewMode",
        "contextCompaction",
    ] {
        let event = serde_json::json!({"item":{"type":kind,"id":"lifecycle-event"}});
        assert!(
            crate::codex::codex_tool_event(&event, false).is_none(),
            "{kind} start lifecycle metadata must not be recorded as a tool"
        );
        assert!(
            crate::codex::codex_tool_event(&event, true).is_none(),
            "{kind} completion lifecycle metadata must not be recorded as a tool"
        );
    }

    for (kind, completed) in [("collabAgentToolCall", false), ("commandExecution", true)] {
        let event = serde_json::json!({
            "item": {
                "type": kind,
                "id": format!("real-tool-{kind}"),
                "command": "python -B check.py",
                "status": "completed",
                "exitCode": 0
            }
        });
        let parsed = crate::codex::codex_tool_event(&event, completed)
            .expect("real native tool invocations must remain visible to the manager guard");
        assert_eq!(parsed.kind, kind);
        assert_eq!(
            parsed.status,
            if completed { "completed" } else { "running" }
        );
    }
}

#[test]
fn task_routes_retain_exact_source_quotes_and_context_ids() {
    let mut f = Fixture::new();
    let request = "Codex给出方案；DSH按方案实现。";
    let workflow = f.coordinated(request, "dsh-win");
    let solution = route(
        "solution",
        "solution",
        "codex-win",
        &workflow.user_message_id,
        "Codex给出方案",
    );
    let ids = f
        .store
        .enqueue_routes(&workflow.id, &[(solution, None, None)])
        .unwrap();
    f.store
        .connection
        .execute(
            "UPDATE project_tasks SET status='completed',output='成员给出的方案' WHERE id=?1",
            [&ids[0]],
        )
        .unwrap();
    let mut implementation = route(
        "implementation",
        "implementation",
        "dsh-win",
        &workflow.user_message_id,
        "DSH按方案实现",
    );
    implementation.context_ids.push(ids[0].clone());
    let implementation_id = f
        .store
        .enqueue_routes(&workflow.id, &[(implementation, None, None)])
        .unwrap()[0]
        .clone();
    let stored: Vec<TaskRoute> = f.store.coordination_routes(&workflow.id).unwrap();
    assert_eq!(stored.len(), 2);
    let route = stored
        .iter()
        .find(|route| route.task_id == implementation_id)
        .unwrap();
    assert_eq!(route.source_id, workflow.user_message_id);
    assert_eq!(route.quote, "DSH按方案实现");
    assert_eq!(route.context_ids, vec![ids[0].clone()]);
}

#[test]
fn resuming_paused_coordination_keeps_completed_routes_and_queued_work() {
    let mut f = Fixture::new();
    let request = "Codex给出方案；DSH按方案实现。";
    let workflow = f.coordinated(request, "hermes-win");
    let solution = route(
        "solution",
        "solution",
        "codex-win",
        &workflow.user_message_id,
        "Codex给出方案",
    );
    let implementation = route(
        "implementation",
        "implementation",
        "dsh-win",
        &workflow.user_message_id,
        "DSH按方案实现",
    );
    let ids = f
        .store
        .enqueue_routes(
            &workflow.id,
            &[(solution, None, None), (implementation, None, None)],
        )
        .unwrap();
    f.store
        .connection
        .execute(
            "UPDATE project_tasks SET status='completed',output='方案结果' WHERE id=?1",
            [&ids[0]],
        )
        .unwrap();
    // 第二个任务在执行中被软件退出打断。
    f.store
        .connection
        .execute(
            "UPDATE project_tasks SET status='interrupted',error='上次项目运行因退出中断' WHERE id=?1",
            [&ids[1]],
        )
        .unwrap();
    f.store
        .connection
        .execute(
            "UPDATE workflows SET status='interrupted' WHERE id=?1",
            [&workflow.id],
        )
        .unwrap();

    let resumed = f.store.resume_coordination(&workflow.id).unwrap();
    assert_eq!(resumed.status, "queued");
    // 继续协作必须能重新占用项目目录，不能报“项目协作不在排队状态”。
    assert!(f.store.acquire_project(&workflow.id).unwrap());
    assert_eq!(resumed.coordination.as_ref().unwrap().status, "routing");
    assert_eq!(resumed.tasks[0].status, "completed");
    assert_eq!(resumed.tasks[1].status, "queued");
    assert!(resumed
        .tasks
        .iter()
        .all(|task| !matches!(task.status.as_str(), "interrupted" | "failed")));
    assert_eq!(f.store.coordination_routes(&workflow.id).unwrap().len(), 2);
}

#[test]
fn withdrawal_requires_a_later_user_correction_and_only_skips_unstarted_work() {
    let mut f = Fixture::new();
    let request = "DSH实现一；DSH实现二；DSH实现三；DSH实现四；DSH实现五。";
    let workflow = f.coordinated(request, "hermes-win");
    let quotes = [
        "DSH实现一",
        "DSH实现二",
        "DSH实现三",
        "DSH实现四",
        "DSH实现五",
    ];
    let routes = quotes
        .iter()
        .enumerate()
        .map(|(index, quote)| {
            (
                route(
                    &format!("task-{index}"),
                    "implementation",
                    "dsh-win",
                    &workflow.user_message_id,
                    quote,
                ),
                None,
                None,
            )
        })
        .collect::<Vec<_>>();
    let ids = f.store.enqueue_routes(&workflow.id, &routes).unwrap();
    for (index, status) in ["queued", "failed", "interrupted", "running", "completed"]
        .iter()
        .enumerate()
    {
        f.store
            .connection
            .execute(
                "UPDATE project_tasks SET status=?1 WHERE id=?2",
                rusqlite::params![status, ids[index]],
            )
            .unwrap();
    }

    let correction = f
        .store
        .post_user_team_message(&f.room, "纠正：取消第二项和第三项，不要继续旧实现。")
        .unwrap();
    let unauthorized_quote = crate::coordinator_policy::Withdrawal {
        task_id: ids[0].clone(),
        source_id: correction.id.clone(),
        quote: "第二项".into(),
    };
    assert!(f
        .store
        .withdraw_coordination_tasks(&workflow.id, &[unauthorized_quote], &[])
        .is_err());

    let original_source = crate::coordinator_policy::Withdrawal {
        task_id: ids[0].clone(),
        source_id: workflow.user_message_id.clone(),
        quote: request.into(),
    };
    assert!(f
        .store
        .withdraw_coordination_tasks(&workflow.id, &[original_source], &[])
        .is_err());

    let wrong_number = crate::coordinator_policy::Withdrawal {
        task_id: ids[0].clone(),
        source_id: correction.id.clone(),
        quote: "纠正：取消第二项和第三项，不要继续旧实现。".into(),
    };
    assert!(f
        .store
        .withdraw_coordination_tasks(&workflow.id, &[wrong_number], &[])
        .is_err());
    assert_eq!(f.store.task(&ids[0]).unwrap().status, "queued");

    for index in [3usize, 4] {
        let protected = crate::coordinator_policy::Withdrawal {
            task_id: ids[index].clone(),
            source_id: correction.id.clone(),
            quote: "取消第二项和第三项".into(),
        };
        assert!(f
            .store
            .withdraw_coordination_tasks(&workflow.id, &[protected], &[])
            .is_err());
        assert_eq!(
            f.store.task(&ids[index]).unwrap().status,
            ["queued", "failed", "interrupted", "running", "completed"][index]
        );
    }

    let withdrawals = [1usize, 2]
        .iter()
        .map(|index| crate::coordinator_policy::Withdrawal {
            task_id: ids[*index].clone(),
            source_id: correction.id.clone(),
            quote: "纠正：取消第二项和第三项，不要继续旧实现。".into(),
        })
        .collect::<Vec<_>>();
    f.store
        .withdraw_coordination_tasks(&workflow.id, &withdrawals, &[])
        .unwrap();
    for index in [1usize, 2] {
        assert_eq!(f.store.task(&ids[index]).unwrap().status, "skipped");
    }
    assert_eq!(f.store.task(&ids[0]).unwrap().status, "queued");
    assert_eq!(f.store.task(&ids[3]).unwrap().status, "running");
    assert_eq!(f.store.task(&ids[4]).unwrap().status, "completed");

    // 用户后来给了授权（不是取消）：只有同一回合按同一原文重新派发、修订任务时才能撤回旧任务。
    let grant = f.store.post_user_team_message(&f.room, "现在可以直接部署，推送交给Codex做").unwrap();
    let revise = crate::coordinator_policy::Withdrawal {
        task_id: ids[0].clone(),
        source_id: grant.id.clone(),
        quote: "现在可以直接部署，推送交给Codex做".into(),
    };
    assert!(f.store.withdraw_coordination_tasks(&workflow.id, &[revise.clone()], &[]).is_err(), "光有授权不能丢掉用户要的活");
    f.store
        .withdraw_coordination_tasks(&workflow.id, &[revise], &[(workflow.user_message_id.clone(), "DSH实现一".into())])
        .unwrap();
    assert_eq!(f.store.task(&ids[0]).unwrap().status, "skipped");
    let mut revised = route("task-0-revised", "implementation", "dsh-win", &workflow.user_message_id, "DSH实现一");
    revised.instructions = Some("按新授权：改完直接部署".into());
    let reopened = f.store.enqueue_routes(&workflow.id, &[(revised, None, None)]).unwrap();
    assert_eq!(reopened, vec![ids[0].clone()], "撤回的任务按同一原文再派就重新打开");
    assert_eq!(f.store.task(&ids[0]).unwrap().status, "queued");
}

#[test]
fn broad_user_correction_can_withdraw_queued_failed_and_interrupted_tasks() {
    let mut f = Fixture::new();
    let request = "DSH实现甲；DSH实现乙；DSH实现丙。";
    let workflow = f.coordinated(request, "hermes-win");
    let quotes = ["DSH实现甲", "DSH实现乙", "DSH实现丙"];
    let routes = quotes
        .iter()
        .enumerate()
        .map(|(index, quote)| {
            (
                route(
                    &format!("cancel-{index}"),
                    "implementation",
                    "dsh-win",
                    &workflow.user_message_id,
                    quote,
                ),
                None,
                None,
            )
        })
        .collect::<Vec<_>>();
    let ids = f.store.enqueue_routes(&workflow.id, &routes).unwrap();
    for (index, status) in ["queued", "failed", "interrupted"].iter().enumerate() {
        f.store
            .connection
            .execute(
                "UPDATE project_tasks SET status=?1 WHERE id=?2",
                rusqlite::params![status, ids[index]],
            )
            .unwrap();
    }
    let correction = f
        .store
        .post_user_team_message(&f.room, "纠正：取消前三项未完成旧实现。")
        .unwrap();
    let withdrawals = ids
        .iter()
        .map(|task_id| crate::coordinator_policy::Withdrawal {
            task_id: task_id.clone(),
            source_id: correction.id.clone(),
            quote: "纠正：取消前三项未完成旧实现".into(),
        })
        .collect::<Vec<_>>();
    f.store
        .withdraw_coordination_tasks(&workflow.id, &withdrawals, &[])
        .unwrap();
    assert!(ids
        .iter()
        .all(|task_id| f.store.task(task_id).unwrap().status == "skipped"));
}

#[test]
fn named_member_correction_cannot_withdraw_another_members_task() {
    let mut f = Fixture::new();
    let request = "Hermes验证结果；DSH实现功能。";
    let workflow = f.coordinated(request, "codex-win");
    let verification = route(
        "verify",
        "verification",
        "hermes-win",
        &workflow.user_message_id,
        "Hermes验证结果",
    );
    let implementation = route(
        "implement",
        "implementation",
        "dsh-win",
        &workflow.user_message_id,
        "DSH实现功能",
    );
    let ids = f
        .store
        .enqueue_routes(
            &workflow.id,
            &[(verification, None, None), (implementation, None, None)],
        )
        .unwrap();
    let correction = f
        .store
        .post_user_team_message(&f.room, "纠正：不要让Hermes验证，取消未开始的验证任务。")
        .unwrap();
    let withdrawal = |task_id: &String| crate::coordinator_policy::Withdrawal {
        task_id: task_id.clone(),
        source_id: correction.id.clone(),
        quote: "纠正：不要让Hermes验证，取消未开始的验证任务".into(),
    };
    assert!(f
        .store
        .withdraw_coordination_tasks(&workflow.id, &[withdrawal(&ids[1])], &[])
        .is_err());
    assert_eq!(f.store.task(&ids[1]).unwrap().status, "queued");
    f.store
        .withdraw_coordination_tasks(&workflow.id, &[withdrawal(&ids[0])], &[])
        .unwrap();
    assert_eq!(f.store.task(&ids[0]).unwrap().status, "skipped");
}

#[test]
fn skipped_dependencies_stay_blocked_until_user_authorizes_revised_scope() {
    let mut f = Fixture::new();
    let request = "DSH实现核心；DSH实现后续并等待核心。";
    let workflow = f.coordinated(request, "hermes-win");
    let first = route(
        "core",
        "implementation",
        "dsh-win",
        &workflow.user_message_id,
        "DSH实现核心",
    );
    let mut second = route(
        "followup",
        "implementation",
        "dsh-win",
        &workflow.user_message_id,
        "DSH实现后续并等待核心",
    );
    second.depends_on.push("core".into());
    let ids = f
        .store
        .enqueue_routes(&workflow.id, &[(first, None, None), (second, None, None)])
        .unwrap();
    let correction = f
        .store
        .post_user_team_message(&f.room, "纠正：取消核心实现，后续工作独立继续。")
        .unwrap();
    let withdrawal = crate::coordinator_policy::Withdrawal {
        task_id: ids[0].clone(),
        source_id: correction.id.clone(),
        quote: "取消核心实现".into(),
    };
    f.store
        .withdraw_coordination_tasks(&workflow.id, &[withdrawal], &[])
        .unwrap();

    let followup = f.store.task(&ids[1]).unwrap();
    assert_eq!(f.store.task(&ids[0]).unwrap().status, "skipped");
    assert!(f
        .store
        .begin_coordinated_attempt(&workflow.id, Some(&followup), "dsh-win", None, None)
        .is_err());

    let mut revised = route(
        "followup-revised",
        "implementation",
        "dsh-win",
        &correction.id,
        "后续工作独立继续",
    );
    revised.task_id = Some(ids[1].clone());
    revised.context_ids.push(workflow.user_message_id.clone());
    let updated = f
        .store
        .enqueue_routes(&workflow.id, &[(revised, None, None)])
        .unwrap();
    assert_eq!(updated, vec![ids[1].clone()]);
    let followup = f.store.task(&ids[1]).unwrap();
    assert!(followup.depends_on.is_empty());
    assert!(f
        .store
        .begin_coordinated_attempt(&workflow.id, Some(&followup), "dsh-win", None, None)
        .is_ok());
}

#[test]
fn completed_or_explicitly_skipped_tasks_can_be_used_in_finish_summary() {
    let mut f = Fixture::new();
    let request = "Codex验证结果；DSH实现保留部分。";
    let workflow = f.coordinated(request, "hermes-win");
    let verification = route(
        "verify",
        "verification",
        "codex-win",
        &workflow.user_message_id,
        "Codex验证结果",
    );
    let implementation = route(
        "implement",
        "implementation",
        "dsh-win",
        &workflow.user_message_id,
        "DSH实现保留部分",
    );
    let ids = f
        .store
        .enqueue_routes(
            &workflow.id,
            &[(verification, None, None), (implementation, None, None)],
        )
        .unwrap();
    f.store
        .connection
        .execute(
            "UPDATE project_tasks SET status='skipped',error='按用户纠正取消' WHERE id=?1",
            [&ids[1]],
        )
        .unwrap();
    f.store
        .connection
        .execute(
            "UPDATE project_tasks SET status='completed',output='验证结果已完成' WHERE id=?1",
            [&ids[0]],
        )
        .unwrap();
    let reference = Reference {
        source_id: ids[0].clone(),
        quote: "验证结果已完成".into(),
    };
    assert_eq!(
        f.store
            .finished_coordination_summary(&workflow.id, Some(&reference))
            .unwrap(),
        "验证结果已完成"
    );
}

#[test]
fn multi_member_user_text_without_model_or_effort_authority_inherits_defaults() {
    for (index, text) in [
        "Codex把这项交DSH执行。",
        "Codex、DSH、Hermes一起处理。",
        "Codex、DSH、Hermes一起思考问题。",
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("u-multi-no-auth-{}", index);
        let sources = user_source(&id, text);
        let key = format!("multi-no-auth-{}", index);
        let selected = route(&key, "implementation", "dsh-win", &id, text);
        assert_eq!(
            coordinator_policy::overrides(
                &selected,
                &sources,
                &models(),
                &fallback("dsh-win", Some("gpt-6.1"), Some("low")),
            )
            .unwrap(),
            (None, None),
            "original member mentions must keep project defaults: {}",
            text
        );
    }
}

#[test]
fn multi_member_text_with_unresolvable_model_and_effort_authority_is_rejected() {
    let text = "Codex、DSH、Hermes一起使用GPT-6.1-Luna low强度处理。";
    let sources = user_source("u-multi-ambiguous-params", text);
    let mut selected = route(
        "multi-ambiguous-params",
        "implementation",
        "dsh-win",
        "u-multi-ambiguous-params",
        text,
    );
    selected.model = Some("gpt-6.1".into());
    selected.effort = Some("low".into());
    selected.model_source = Some(Reference {
        source_id: "u-multi-ambiguous-params".into(),
        quote: text.into(),
    });
    assert!(coordinator_policy::overrides(
        &selected,
        &sources,
        &models(),
        &fallback("dsh-win", Some("gpt-6.1"), Some("low")),
    )
    .is_err());
}

#[test]
fn manager_turn_sandbox_matches_installed_read_only_protocol() {
    let native = include_str!("project_native.rs");
    assert!(native.contains("json!({\"type\":\"readOnly\",\"networkAccess\":false})"));
    assert!(!native.contains("\"access\":{\"type\":\"restricted\""));
}

#[test]
fn solution_member_prompt_scope_forbids_implementation_and_other_execution() {
    let prompt = coordinator_policy::member_prompt(
        "codex-win",
        "solution",
        "先给方案，再按方案实现并验证。",
        "先给方案，再按方案实现并验证。",
        vec![],
        None,
        false,
    );
    assert_eq!(prompt["purpose"], "solution");
    assert_eq!(
        prompt["task_scope"],
        "本次只完成用户要求的方案或分析，不提前执行引用中的后续实现、验证或收尾。"
    );
}

#[test]
fn completed_coordination_cannot_be_paused_for_wait() {
    let mut f = Fixture::new();
    let request = "Codex给出方案。";
    let workflow = f.coordinated(request, "dsh-win");
    let solution = route(
        "solution",
        "solution",
        "codex-win",
        &workflow.user_message_id,
        request,
    );
    let ids = f
        .store
        .enqueue_routes(&workflow.id, &[(solution, None, None)])
        .unwrap();
    f.store
        .connection
        .execute(
            "UPDATE project_tasks SET status='completed' WHERE id=?1",
            [&ids[0]],
        )
        .unwrap();

    assert!(f.store.pause_coordination_wait(&workflow.id).is_err());
    assert_eq!(f.store.workflow(&workflow.id).unwrap().status, "running");
    assert_eq!(f.store.task(&ids[0]).unwrap().status, "completed");
}

#[test]
fn queued_independent_work_continues_instead_of_pausing_for_wait() {
    let mut f = Fixture::new();
    let request = "Codex给出方案。DSH实现功能。";
    let workflow = f.coordinated(request, "hermes-win");
    let solution = route(
        "solution",
        "solution",
        "codex-win",
        &workflow.user_message_id,
        "Codex给出方案。",
    );
    let implementation = route(
        "implementation",
        "implementation",
        "dsh-win",
        &workflow.user_message_id,
        "DSH实现功能。",
    );
    let ids = f
        .store
        .enqueue_routes(
            &workflow.id,
            &[(solution, None, None), (implementation, None, None)],
        )
        .unwrap();

    assert!(!f.store.pause_coordination_wait(&workflow.id).unwrap());
    assert_eq!(f.store.workflow(&workflow.id).unwrap().status, "running");
    assert!(ids
        .iter()
        .all(|id| f.store.task(id).unwrap().status == "queued"));
}

#[test]
fn queued_work_blocked_by_skipped_dependency_does_not_pause_without_a_failed_task() {
    let mut f = Fixture::new();
    let request = "DSH实现核心。Hermes继续实现。";
    let workflow = f.coordinated(request, "codex-win");
    let core = route(
        "core",
        "implementation",
        "dsh-win",
        &workflow.user_message_id,
        "DSH实现核心。",
    );
    let mut followup = route(
        "followup",
        "implementation",
        "hermes-win",
        &workflow.user_message_id,
        "Hermes继续实现。",
    );
    followup.depends_on.push("core".into());
    let ids = f
        .store
        .enqueue_routes(&workflow.id, &[(core, None, None), (followup, None, None)])
        .unwrap();
    f.store
        .connection
        .execute(
            "UPDATE project_tasks SET status='skipped' WHERE id=?1",
            [&ids[0]],
        )
        .unwrap();

    assert!(f.store.pause_coordination_wait(&workflow.id).is_err());
    let current = f.store.workflow(&workflow.id).unwrap();
    assert_eq!(current.status, "running");
    assert_eq!(current.tasks[0].status, "skipped");
    assert_eq!(current.tasks[1].status, "queued");
}

#[test]
fn failed_task_with_independent_queued_work_continues_before_waiting() {
    let mut f = Fixture::new();
    let request = "Codex方案。DSH实现。Hermes收尾。";
    let workflow = f.coordinated(request, "codex-win");
    let solution = route(
        "solution",
        "solution",
        "codex-win",
        &workflow.user_message_id,
        "Codex方案。",
    );
    let implementation = route(
        "implementation",
        "implementation",
        "dsh-win",
        &workflow.user_message_id,
        "DSH实现。",
    );
    let closeout = route(
        "closeout",
        "closeout",
        "hermes-win",
        &workflow.user_message_id,
        "Hermes收尾。",
    );
    let ids = f
        .store
        .enqueue_routes(
            &workflow.id,
            &[
                (solution, None, None),
                (implementation, None, None),
                (closeout, None, None),
            ],
        )
        .unwrap();
    for (id, status) in [
        (&ids[0], "completed"),
        (&ids[1], "failed"),
        (&ids[2], "queued"),
    ] {
        f.store
            .connection
            .execute(
                "UPDATE project_tasks SET status=?1 WHERE id=?2",
                rusqlite::params![status, id],
            )
            .unwrap();
    }

    assert!(!f.store.pause_coordination_wait(&workflow.id).unwrap());
    let current = f.store.workflow(&workflow.id).unwrap();
    assert_eq!(current.status, "running");
    assert_eq!(current.tasks[0].status, "completed");
    assert_eq!(current.tasks[1].status, "failed");
    assert_eq!(current.tasks[2].status, "queued");
}

#[test]
fn failed_task_with_only_dependent_queued_work_pauses_and_preserves_completed_results() {
    let mut f = Fixture::new();
    let request = "Codex方案。DSH实现。Hermes收尾。";
    let workflow = f.coordinated(request, "codex-win");
    let solution = route(
        "solution",
        "solution",
        "codex-win",
        &workflow.user_message_id,
        "Codex方案。",
    );
    let implementation = route(
        "implementation",
        "implementation",
        "dsh-win",
        &workflow.user_message_id,
        "DSH实现。",
    );
    let mut closeout = route(
        "closeout",
        "closeout",
        "hermes-win",
        &workflow.user_message_id,
        "Hermes收尾。",
    );
    closeout.depends_on.push("implementation".into());
    let ids = f
        .store
        .enqueue_routes(
            &workflow.id,
            &[
                (solution, None, None),
                (implementation, None, None),
                (closeout, None, None),
            ],
        )
        .unwrap();
    for (id, status) in [
        (&ids[0], "completed"),
        (&ids[1], "failed"),
        (&ids[2], "queued"),
    ] {
        f.store
            .connection
            .execute(
                "UPDATE project_tasks SET status=?1 WHERE id=?2",
                rusqlite::params![status, id],
            )
            .unwrap();
    }

    assert!(f.store.pause_coordination_wait(&workflow.id).unwrap());
    let current = f.store.workflow(&workflow.id).unwrap();
    assert_eq!(current.status, "interrupted");
    assert_eq!(current.coordination.unwrap().status, "paused");
    assert_eq!(current.tasks[0].status, "completed");
    assert_eq!(current.tasks[1].status, "failed");
    assert_eq!(current.tasks[2].status, "queued");
}

#[test]
fn subagent_model_prefers_cheap_tier_with_high_effort() {
    let option = |id: &str, provider: &str, efforts: &[&str]| ModelOption {
        id: id.into(),
        name: id.into(),
        provider_id: Some(provider.into()),
        provider_name: None,
        efforts: efforts.iter().map(|e| e.to_string()).collect(),
        default_effort: None,
    };
    let claude = vec![
        option("opus", "anthropic", &["low", "high", "max"]),
        option("claude-haiku-4-5", "anthropic", &[]),
        option("haiku", "anthropic", &["low", "medium", "high", "max"]),
    ];
    let none = fallback("claude-win", Some("opus"), None);
    assert_eq!(
        coordinator_policy::subagent_model(&claude, &none),
        (Some("haiku".into()), Some("high".into()))
    );
    // 配置的模型本身就是低价档时沿用它。
    let flash = vec![
        option("daily-1/flash", "deepseek", &["off", "low", "high", "max"]),
        option("daily-2/flash", "deepseek", &["off", "low", "high", "max"]),
    ];
    assert_eq!(
        coordinator_policy::subagent_model(&flash, &fallback("dsh-win", Some("daily-2/flash"), None)).0,
        Some("daily-2/flash".into())
    );
    // 多提供商时优先当前配置的提供商；只有 max 档时取 max。
    let mixed = vec![
        option("gemini-flash", "google", &["high"]),
        option("opus", "anthropic", &["high"]),
        option("haiku", "anthropic", &["max"]),
    ];
    assert_eq!(
        coordinator_policy::subagent_model(&mixed, &fallback("hermes-win", Some("opus"), None)),
        (Some("haiku".into()), Some("max".into()))
    );
    // 没有低价档就沿用成员配置。
    assert_eq!(
        coordinator_policy::subagent_model(&[option("opus", "anthropic", &["high"])], &none),
        (Some("opus".into()), Some("high".into()))
    );
}

#[test]
fn subagent_verification_is_dispatched_by_the_implementer_only_when_allowed() {
    let mut f = Fixture::new();
    let workflow = f.coordinated("DSH实现核心；Codex负责验证", "hermes-win");
    let source = workflow.user_message_id.clone();
    let complete = |f: &mut Fixture, id: &str, output: &str| {
        f.store
            .connection
            .execute(
                "UPDATE project_tasks SET status='completed',output=?2 WHERE id=?1",
                rusqlite::params![id, output],
            )
            .unwrap();
    };
    let implement = route("impl", "implementation", "dsh-win", &source, "DSH实现核心");
    let verify = route("verify", "verification", "codex-win", &source, "Codex负责验证");
    let first = f
        .store
        .enqueue_routes(&workflow.id, &[(implement.clone(), None, None)])
        .unwrap()[0]
        .clone();
    complete(&mut f, &first, "已实现");
    let check = f
        .store
        .enqueue_routes(&workflow.id, &[(verify.clone(), None, None)])
        .unwrap()[0]
        .clone();
    complete(&mut f, &check, "验证结论：未通过");

    let mut sub = verify.clone();
    sub.key = "sub".into();
    sub.agent_id = "dsh-win".into();
    sub.subagent = true;
    sub.context_ids = vec![first.clone()];
    // 点名 Codex 的验证原文，普通路由仍不能换派给 DSH。
    let mut plain = sub.clone();
    plain.subagent = false;
    let sources = f.store.coordination_sources(&workflow.id).unwrap();
    assert!(coordinator_policy::check_assignment(&plain, &sources).is_err());
    assert!(coordinator_policy::check_assignment(&sub, &sources).is_ok());
    // 只失败一次：不放行。
    let error = f
        .store
        .enqueue_routes(&workflow.id, &[(sub.clone(), None, None)])
        .unwrap_err();
    assert!(error.contains("2 次"), "{error}");

    let mut rework = implement.clone();
    rework.key = "rework".into();
    rework.context_ids = vec![check.clone()];
    let fixed = f
        .store
        .enqueue_routes(&workflow.id, &[(rework, None, None)])
        .unwrap()[0]
        .clone();
    complete(&mut f, &fixed, "已返修");
    let mut recheck = verify.clone();
    recheck.key = "recheck".into();
    recheck.context_ids = vec![fixed.clone()];
    let rechecked = f
        .store
        .enqueue_routes(&workflow.id, &[(recheck, None, None)])
        .unwrap()[0]
        .clone();
    complete(&mut f, &rechecked, "验证结论：未通过");

    // 两次未通过后：非改动者不能派，改动者可以。
    let mut wrong = sub.clone();
    wrong.agent_id = "codex-win".into();
    let error = f
        .store
        .enqueue_routes(&workflow.id, &[(wrong, None, None)])
        .unwrap_err();
    assert!(error.contains("改动源文件"), "{error}");
    sub.context_ids = vec![fixed];
    let id = f
        .store
        .enqueue_routes(&workflow.id, &[(sub, Some("haiku".into()), Some("high".into()))])
        .unwrap()[0]
        .clone();
    let task = f.store.task(&id).unwrap();
    assert!(task.title.starts_with("子代理验证"), "{}", task.title);
    assert_eq!(task.agent_id, "dsh-win");
    assert_eq!(task.model.as_deref(), Some("haiku"));
    let routes = f.store.coordination(&workflow.id).unwrap().unwrap().routes;
    assert!(routes.iter().any(|route| route.task_id == id && route.subagent));
    assert!(!routes.iter().any(|route| route.task_id == rechecked && route.subagent));
}

#[test]
fn subagent_verification_is_allowed_upfront_when_the_user_asks() {
    let mut f = Fixture::new();
    let workflow = f.coordinated("DSH实现核心；用执行者的子代理验证", "hermes-win");
    let source = workflow.user_message_id.clone();
    let first = f
        .store
        .enqueue_routes(
            &workflow.id,
            &[(route("impl", "implementation", "dsh-win", &source, "DSH实现核心"), None, None)],
        )
        .unwrap()[0]
        .clone();
    f.store
        .connection
        .execute("UPDATE project_tasks SET status='completed',output='已实现' WHERE id=?1", [&first])
        .unwrap();
    let mut sub = route("sub", "verification", "dsh-win", &source, "用执行者的子代理验证");
    sub.subagent = true;
    sub.context_ids = vec![first];
    let id = f
        .store
        .enqueue_routes(&workflow.id, &[(sub, None, None)])
        .unwrap()[0]
        .clone();
    assert!(f.store.task(&id).unwrap().title.starts_with("子代理验证"));
}

#[test]
fn subagent_scope_tells_the_implementer_to_dispatch_its_own_subagent() {
    let claude = coordinator_policy::subagent_scope("claude-win", Some("haiku"));
    assert!(claude.contains("Agent 工具") && claude.contains("model 设为 haiku") && claude.contains("effort 设为 high"), "{claude}");
    assert!(claude.contains("不要自己验证") && claude.contains("验证结论"));
    let other = coordinator_policy::subagent_scope("dsh-win", Some("flash"));
    assert!(other.contains("自带的子代理") && other.contains("flash") && other.contains("高思考强度"), "{other}");
}

#[test]
fn new_round_after_user_answer_rechecks_and_moves_queued_closeout_behind_it() {
    let mut f = Fixture::new();
    let workflow = f.coordinated("DSH实现核心；Codex负责验证；DSH收尾整理", "hermes-win");
    let source = workflow.user_message_id.clone();
    let complete = |f: &mut Fixture, id: &str, output: &str| {
        f.store
            .connection
            .execute(
                "UPDATE project_tasks SET status='completed',output=?2 WHERE id=?1",
                rusqlite::params![id, output],
            )
            .unwrap();
    };
    let implement = route("impl", "implementation", "dsh-win", &source, "DSH实现核心");
    let mut verify = route("verify", "verification", "codex-win", &source, "Codex负责验证");
    verify.depends_on = vec!["impl".into()];
    let mut close = route("close", "closeout", "dsh-win", &source, "DSH收尾整理");
    close.depends_on = vec!["impl".into(), "verify".into()];
    let ids = f
        .store
        .enqueue_routes(
            &workflow.id,
            &[(implement, None, None), (verify.clone(), None, None), (close.clone(), None, None)],
        )
        .unwrap();
    complete(&mut f, &ids[0], "已实现");
    complete(&mut f, &ids[1], "验证结论：未通过");
    // 用户回答“让 DSH 出方案再改”：新原文的实现，复用原验证与收尾原文，依赖本批的新实现。
    f.store
        .connection
        .execute(
            "INSERT INTO project_questions(id,workflow_id,conversation_id,agent_id,prompt,options,status,answer,created_at,answered_at) VALUES('q1',?1,?2,'hermes-win','怎么办？','[]','answered','让DSH出方案，然后改',1,2)",
            rusqlite::params![workflow.id, workflow.conversation_id],
        )
        .unwrap();
    let mut again = route("i1", "implementation", "dsh-win", "answer:q1", "让DSH出方案，然后改");
    again.context_ids = vec![ids[1].clone()];
    let mut recheck = verify;
    recheck.key = "v1".into();
    recheck.depends_on = vec!["i1".into()];
    let mut closeout = close;
    closeout.key = "c1".into();
    closeout.depends_on = vec!["i1".into(), "v1".into()];
    let next = f
        .store
        .enqueue_routes(
            &workflow.id,
            &[(again, None, None), (recheck, None, None), (closeout, None, None)],
        )
        .unwrap();
    assert_ne!(next[1], ids[1], "依赖新实现的验证是新一轮复核，不是重复派发");
    assert!(f.store.task(&next[1]).unwrap().title.starts_with("复核"));
    assert_eq!(next[2], ids[2], "未开始的收尾沿用原任务");
    let close_task = f.store.task(&ids[2]).unwrap();
    let new_impl = f.store.task(&next[0]).unwrap();
    let new_check = f.store.task(&next[1]).unwrap();
    assert!(close_task.position > new_check.position && new_check.position > new_impl.position);
    assert_eq!(close_task.depends_on, vec![new_impl.position, new_check.position]);
}

#[test]
fn quota_failures_are_recognised_and_retried_with_growing_intervals() {
    for error in [
        "原生 agent session/prompt 请求失败：模型服务限流，请稍后继续当前任务（协议代码 -32603）",
        "Claude usage limit reached",
        "HTTP 429 Too Many Requests",
        "rate_limit_error",
        "本月额度已用完",
    ] {
        assert!(coordinator_policy::quota_exhausted(error), "{error}");
    }
    for error in ["项目成员的聊天接口已断开", "编译失败", "管家回合未完成"] {
        assert!(!coordinator_policy::quota_exhausted(error), "{error}");
    }
    let minutes = (0..8).map(coordinator_policy::quota_retry_minutes).collect::<Vec<_>>();
    assert_eq!(minutes, vec![5, 10, 15, 20, 25, 30, 30, 30]);
}

#[test]
fn coordinator_can_route_work_to_a_second_seat_of_the_named_member() {
    let mut f = Fixture::new();
    let request = "让两个Codex一起实现：一个写代码，另一个补测试。";
    let workflow = f.coordinated(request, "hermes-win");
    // 没开过的席位不能接任务。
    let unopened = route("tests", "implementation", "codex-win-2", &workflow.user_message_id, request);
    assert!(f.store.enqueue_routes(&workflow.id, &[(unopened.clone(), None, None)]).is_err());
    assert_eq!(f.store.add_project_seat(&f.room, "codex-win", Some("测试手")).unwrap(), "codex-win-2");
    // 原话点名「Codex」，派给 Codex 的第二个席位也算点对了人。
    let ids = f
        .store
        .enqueue_routes(
            &workflow.id,
            &[
                (route("code", "implementation", "codex-win", &workflow.user_message_id, request), None, None),
                (unopened, None, None),
            ],
        )
        .unwrap();
    assert_eq!(ids.len(), 2);
    let job = f.store.workflow(&workflow.id).unwrap();
    let agents: Vec<_> = job.tasks.iter().map(|task| task.agent_id.as_str()).collect();
    assert_eq!(agents, ["codex-win", "codex-win-2"]);
    // 两个席位各自起 attempt，互不因「同一成员忙」而阻塞。
    let first = job.tasks[0].clone();
    let second = job.tasks[1].clone();
    f.store.begin_coordinated_attempt(&workflow.id, Some(&first), "codex-win", None, None).unwrap();
    let mut attempt = f.store.begin_coordinated_attempt(&workflow.id, Some(&second), "codex-win-2", None, None).unwrap();
    // 非 DSH 成员的模型在运行中被改动，仍按身份不符拒绝保存。
    attempt.model = Some("别的模型".into());
    assert!(f.store.checkpoint_attempt(&attempt).is_err());
}

#[test]
fn dsh_route_notices_may_update_the_running_model() {
    let mut f = Fixture::new();
    let request = "DSH回答一个问题。";
    let workflow = f.coordinated(request, "hermes-win");
    f.store
        .enqueue_routes(&workflow.id, &[(route("ask", "reply", "dsh-win", &workflow.user_message_id, request), None, None)])
        .unwrap();
    let task = f.store.workflow(&workflow.id).unwrap().tasks[0].clone();
    let mut attempt = f.store.begin_coordinated_attempt(&workflow.id, Some(&task), "dsh-win", None, None).unwrap();
    // DSH 运行中通报实际线路：没指定模型的任务也能记下实际模型，而不是整轮判失败。
    attempt.model = Some("[\"deepseek-official\",\"deepseek-v4-flash\"]".into());
    attempt.status = "running".into();
    f.store.checkpoint_attempt(&attempt).unwrap();
}

fn two_models() -> Vec<ModelOption> {
    let mut catalog = models();
    catalog.push(ModelOption {
        id: "haiku".into(),
        name: "Haiku 5.5".into(),
        provider_id: None,
        provider_name: None,
        efforts: vec!["low".into(), "high".into()],
        default_effort: Some("low".into()),
    });
    catalog
}

#[test]
fn coordinator_may_pick_model_and_effort_when_user_named_none() {
    let text = "Claude检查一下边界条件。";
    let sources = user_source("u-free", text);
    let mut chosen = route("check", "verification", "claude-win", "u-free", text);
    chosen.model = Some("haiku".into());
    chosen.effort = Some("high".into());
    assert_eq!(
        coordinator_policy::overrides(&chosen, &sources, &two_models(), &fallback("claude-win", Some("gpt-6.1"), Some("medium"))).unwrap(),
        (Some("haiku".into()), Some("high".into())),
        "用户没点名时，调度可以按任务选目录里的组合"
    );
    chosen.effort = Some("xhigh".into());
    assert!(
        coordinator_policy::overrides(&chosen, &sources, &two_models(), &fallback("claude-win", None, None)).is_err(),
        "选的强度该模型不支持就不行"
    );
}

#[test]
fn user_named_model_and_effort_still_win_over_coordinator_choice() {
    let text = "Codex使用GPT-6.1-Luna、low强度完成验证。";
    let sources = user_source("u-named", text);
    let mut other = route("verify", "verification", "codex-win", "u-named", text);
    other.model = Some("haiku".into());
    assert!(
        coordinator_policy::overrides(&other, &sources, &two_models(), &fallback("codex-win", None, None)).is_err(),
        "用户点名了模型，调度不能换"
    );
    let mut stronger = route("verify", "verification", "codex-win", "u-named", text);
    stronger.effort = Some("high".into());
    assert!(
        coordinator_policy::overrides(&stronger, &sources, &two_models(), &fallback("codex-win", None, None)).is_err(),
        "用户点名了强度，调度不能改"
    );
    // 用户只点了强度：模型可由调度选。
    let effort_only = "Claude用low思考强度回复进度。";
    let sources = user_source("u-effort", effort_only);
    let mut pick = route("reply", "reply", "claude-win", "u-effort", effort_only);
    pick.model = Some("haiku".into());
    assert_eq!(
        coordinator_policy::overrides(&pick, &sources, &two_models(), &fallback("claude-win", Some("gpt-6.1"), None)).unwrap(),
        (Some("haiku".into()), Some("low".into()))
    );
}

#[test]
fn coordinator_reads_delivery_summary_instead_of_full_output() {
    let output = format!("{}\n交付摘要：改了 calc.py，测试通过。\n验证结论：通过", "过程".repeat(5000));
    assert_eq!(coordinator_policy::delivery_summary(&output), "交付摘要：改了 calc.py，测试通过。\n验证结论：通过");
    let long = "长".repeat(9000);
    let clipped = coordinator_policy::delivery_summary(&long);
    assert!(clipped.chars().count() < 2600 && clipped.contains("中间省略"));
    assert_eq!(coordinator_policy::delivery_summary("短回复"), "短回复");
}

#[test]
fn member_prompt_carries_brief_and_subagent_permission() {
    let prompt = coordinator_policy::member_prompt("claude-win", "verification", "验证一下", "验证一下", vec![], Some("目标：只验证 calc.py"), true);
    assert_eq!(prompt["coordinator_brief"], "目标：只验证 calc.py");
    assert!(prompt["subagents"].as_str().unwrap().contains("子 agent"));
    assert!(prompt["instruction"].as_str().unwrap().contains("交付摘要："));
    let plain = coordinator_policy::member_prompt("claude-win", "reply", "说明", "说明", vec![], Some("  "), false);
    assert!(plain.get("coordinator_brief").is_none() && plain.get("subagents").is_none());
}

#[test]
fn coordinator_sessions_stay_in_the_project_like_user_sessions() {
    let mut f = Fixture::new();
    let room = f.room.clone();
    let workflow = f.coordinated("Codex给出方案。", "codex-win");
    let seat = f.store.add_temporary_seat(&workflow.id, "codex-win", "验证").unwrap();
    assert_eq!(f.store.temporary_seats(&workflow.id).unwrap(), vec![seat.clone()]);
    f.store.connection.execute("UPDATE workflows SET status='completed' WHERE id=?1", [&workflow.id]).unwrap();
    let chats = f.store.project_agent_chats(&room).unwrap();
    let chat = chats.iter().find(|chat| chat.agent_id == seat).expect("协作结束后调度开的会话仍在项目里");
    assert!(chat.opened_by_coordinator);
    assert_eq!(chat.name, "验证");
    assert!(chats.iter().filter(|chat| chat.agent_id != seat).all(|chat| !chat.opened_by_coordinator));
    assert_eq!(f.store.coordinator_seats(&room).unwrap(), vec![seat]);
}

#[test]
fn full_member_recycles_only_an_idle_coordinator_session() {
    let mut f = Fixture::new();
    let room = f.room.clone();
    let first = f.coordinated("Codex给出方案。", "codex-win");
    let opened = f.store.add_temporary_seat(&first.id, "codex-win", "验证").unwrap();
    let mine = [f.store.add_project_seat(&room, "codex-win", Some("我的A")).unwrap(), f.store.add_project_seat(&room, "codex-win", Some("我的B")).unwrap()];
    assert!(f.store.add_temporary_seat(&first.id, "codex-win", "回复").is_err(), "协作还在进行，它开的会话不能回收");
    f.store.connection.execute("UPDATE workflows SET status='completed' WHERE id=?1", [&first.id]).unwrap();
    let second = f.coordinated("Codex再看一遍。", "codex-win");
    let reopened = f.store.add_temporary_seat(&second.id, "codex-win", "回复").unwrap();
    assert_eq!(reopened, opened, "回收最早的调度会话，空出的位置给新会话");
    let seats = f.store.project_seats(&room).unwrap();
    assert!(mine.iter().all(|seat| seats.iter().any(|(id, _)| id == seat)), "用户建的会话不动");
    assert_eq!(f.store.project_agent_chats(&room).unwrap().iter().find(|chat| chat.agent_id == reopened).unwrap().name, "回复");
    f.store.connection.execute("UPDATE workflows SET status='completed' WHERE id=?1", [&second.id]).unwrap();
    f.store.remove_project_seat(&room, &reopened).unwrap();
    f.store.add_project_seat(&room, "codex-win", Some("我的C")).unwrap();
    let third = f.coordinated("Codex第三次。", "codex-win");
    let full = f.store.add_temporary_seat(&third.id, "codex-win", "回复").unwrap_err();
    assert!(full.contains("开满"), "{full}");
}

#[test]
fn coordinator_cannot_open_sessions_for_agents_outside_the_project() {
    let mut f = Fixture::new();
    let workflow = f.coordinated("Codex给出方案。", "codex-win");
    let error = f.store.add_temporary_seat(&workflow.id, "claude-win", "回复").unwrap_err();
    assert!(error.contains("不在这个项目里"), "{error}");
    assert!(f.store.project_seats(&f.room.clone()).unwrap().is_empty());
}

#[test]
fn project_preference_defaults_to_balanced_and_bounds_temporary_sessions() {
    let mut f = Fixture::new();
    let room = f.room.clone();
    assert_eq!(f.store.project_preference(&room), "balanced");
    f.store.set_project_preference(&room, "save").unwrap();
    assert_eq!(f.store.project_preference(&room), "save");
    assert!(f.store.set_project_preference(&room, "turbo").is_err());
    assert_eq!(crate::coordinator_store::temporary_limit("save"), 1);
    assert_eq!(crate::coordinator_store::temporary_limit("quality"), 3);
}

#[test]
fn usage_counts_only_this_task_from_codex_thread_totals_and_reads_acp_results() {
    use crate::project_store::Usage;
    let breakdown = |input: u64, output: u64| serde_json::json!({"inputTokens":input,"cachedInputTokens":input/2,"outputTokens":output,"reasoningOutputTokens":output/2,"totalTokens":input+output});
    // 线程里之前已经用了 1000+100；本任务第一次调用 300+30，之后累计到 1500+200。
    let first_total = Usage::from_codex(&breakdown(1300, 130)).unwrap();
    let first_last = Usage::from_codex(&breakdown(300, 30)).unwrap();
    let base = first_total.since(&first_last);
    let latest = Usage::from_codex(&breakdown(1500, 200)).unwrap().since(&base);
    assert_eq!((latest.input, latest.output, latest.total()), (500, 100, 600));
    let acp = serde_json::json!({"stopReason":"end_turn","usage":{"inputTokens":24413,"outputTokens":4,"cachedReadTokens":10},"_meta":{"contextTokens":24417,"costUsd":0.0048}});
    let usage = Usage::from_acp(&acp).unwrap();
    assert_eq!((usage.total(), usage.cached, usage.context), (24417, 10, 24417));
    assert_eq!(usage.cost_usd, Some(0.0048));
    assert!(Usage::from_acp(&serde_json::json!({"stopReason":"end_turn"})).is_none());
}

#[test]
fn clear_context_starts_only_the_first_attempt_fresh_and_ledgers_carry_to_the_next_run() {
    let mut f = Fixture::new();
    let workflow = f.coordinated("DSH实现甲；DSH实现乙", "hermes-win");
    let mut first = route("a", "implementation", "dsh-win", &workflow.user_message_id, "DSH实现甲");
    first.clear_context = true;
    let second = route("b", "implementation", "dsh-win", &workflow.user_message_id, "DSH实现乙");
    let ids = f
        .store
        .enqueue_routes(&workflow.id, &[(first, None, None), (second, None, None)])
        .unwrap();
    let attempt = f
        .store
        .begin_coordinated_attempt(&workflow.id, Some(&f.store.task(&ids[0]).unwrap()), "dsh-win", None, None)
        .unwrap();
    assert!(f.store.clears_context(&attempt), "设了 clear_context 的任务第一次从零上下文开始");
    f.store.connection.execute("UPDATE project_attempts SET status='failed' WHERE id=?1", [&attempt.id]).unwrap();
    f.store.connection.execute("UPDATE project_tasks SET status='queued' WHERE id=?1", [&ids[0]]).unwrap();
    let retry = f
        .store
        .begin_coordinated_attempt(&workflow.id, Some(&f.store.task(&ids[0]).unwrap()), "dsh-win", None, None)
        .unwrap();
    assert!(!f.store.clears_context(&retry), "重试续接第一次新开的会话");
    f.store.connection.execute("UPDATE project_attempts SET status='failed' WHERE id=?1", [&retry.id]).unwrap();
    f.store.connection.execute("UPDATE project_tasks SET status='queued' WHERE id=?1", [&ids[0]]).unwrap();
    let other = f
        .store
        .begin_coordinated_attempt(&workflow.id, Some(&f.store.task(&ids[1]).unwrap()), "dsh-win", None, None)
        .unwrap();
    assert!(!f.store.clears_context(&other));

    f.store.set_coordination_ledger(&workflow.id, "板块一已完成；下一个：板块二").unwrap();
    f.store
        .connection
        .execute("UPDATE workflows SET status='completed' WHERE id=?1", [&workflow.id])
        .unwrap();
    let next = f.coordinated("继续下一个板块", "hermes-win");
    assert_eq!(f.store.previous_ledger(&next.id).as_deref(), Some("板块一已完成；下一个：板块二"));
    assert_eq!(f.store.previous_ledger(&workflow.id), None);
}

#[test]
fn finishing_a_section_defers_unstarted_tasks_and_resets_worked_members() {
    assert!(coordinator_policy::parse(r#"{"action":"delegate","next_sections":[{"title":"TTS"}]}"#).is_err(), "只在 finish 时列剩下的板块");
    let parsed = coordinator_policy::parse(r#"{"action":"finish","next_sections":[{"title":"TTS 板块","summary":"语音合成精简"}]}"#).unwrap();
    assert_eq!(parsed.next_sections[0].title, "TTS 板块");
    for (raw, shown) in [("3. 小修遗留", "小修遗留"), ("4、杂活余项", "杂活余项"), ("（5）原方案", "原方案"), ("第6块：token 轮换", "token 轮换"), ("2026 计划", "2026 计划"), ("8", "8"), ("TTS 遗留", "TTS 遗留")] {
        assert_eq!(coordinator_policy::section_title(raw), shown, "板块标题去掉旧序号");
    }

    let mut f = Fixture::new();
    let workflow = f.coordinated("DSH实现甲；Codex实现乙", "hermes-win");
    let ids = f
        .store
        .enqueue_routes(
            &workflow.id,
            &[
                (route("a", "implementation", "dsh-win", &workflow.user_message_id, "DSH实现甲"), None, None),
                (route("b", "implementation", "codex-win", &workflow.user_message_id, "Codex实现乙"), None, None),
            ],
        )
        .unwrap();
    let attempt = f
        .store
        .begin_coordinated_attempt(&workflow.id, Some(&f.store.task(&ids[0]).unwrap()), "dsh-win", None, None)
        .unwrap();
    f.store.connection.execute("UPDATE project_attempts SET status='completed' WHERE id=?1", [&attempt.id]).unwrap();
    f.store.connection.execute("UPDATE project_tasks SET status='completed' WHERE id=?1", [&ids[0]]).unwrap();
    let chats = f.store.ensure_project_agent_chats(&f.room).unwrap();
    for chat in &chats {
        f.store
            .connection
            .execute("UPDATE sessions SET native_session_id='old-'||agent_id WHERE conversation_id=?1", [&chat.conversation_id])
            .unwrap();
    }

    assert_eq!(f.store.defer_unstarted_tasks(&workflow.id).unwrap(), 1);
    assert_eq!(f.store.task(&ids[1]).unwrap().status, "skipped");
    assert_eq!(f.store.task(&ids[0]).unwrap().status, "completed");
    assert_eq!(f.store.reset_worked_sessions(&workflow.id).unwrap(), 1, "只清这次干过活的成员");
    let session = |agent: &str| {
        let chat = chats.iter().find(|chat| chat.agent_id == agent).unwrap();
        f.store.connection.query_row("SELECT native_session_id FROM sessions WHERE conversation_id=?1", [&chat.conversation_id], |row| row.get::<_, Option<String>>(0)).unwrap()
    };
    assert_eq!(session("dsh-win"), None);
    assert_eq!(session("codex-win").as_deref(), Some("old-codex-win"));
}

#[test]
fn quota_blocked_task_can_be_handed_to_another_member_on_the_same_worktree() {
    let mut f = Fixture::new();
    let request = "实现核心";
    let workflow = f.coordinated(request, "hermes-win");
    let first = route("impl", "implementation", "codex-win", &workflow.user_message_id, request);
    let ids = f.store.enqueue_routes(&workflow.id, &[(first.clone(), None, None)]).unwrap();
    f.store
        .connection
        .execute(
            "UPDATE project_tasks SET status='failed',error='usage limit reached, try again at 3:00 PM',worktree='wt',branch='hub/x' WHERE id=?1",
            [&ids[0]],
        )
        .unwrap();
    let mut handed = first;
    handed.task_id = Some(ids[0].clone());
    handed.agent_id = "dsh-win".into();
    assert_eq!(f.store.enqueue_routes(&workflow.id, &[(handed, None, None)]).unwrap(), ids);
    let task = f.store.task(&ids[0]).unwrap();
    assert_eq!((task.agent_id.as_str(), task.status.as_str()), ("dsh-win", "queued"));
    assert_eq!(task.worktree.as_deref(), Some("wt"), "接着原工作树做");
}

#[test]
fn project_grants_are_saved_per_project_and_shown_in_detail() {
    let mut f = Fixture::new();
    let room = f.room.clone();
    assert_eq!(f.store.project_grants(&room), "");
    f.store.set_project_grants(&room, "  允许提交并推送 main，不 force push  ").unwrap();
    assert_eq!(f.store.project_grants(&room), "允许提交并推送 main，不 force push");
    assert_eq!(f.store.detail(&room).unwrap().project_grants, "允许提交并推送 main，不 force push");
    assert!(f.store.set_project_grants(&room, &"长".repeat(501)).is_err());
    f.store.set_project_grants(&room, "").unwrap();
    assert_eq!(f.store.project_grants(&room), "");
}

#[test]
fn private_messages_wait_while_the_seat_runs_a_project_task() {
    let mut f = Fixture::new();
    let request = "实现核心";
    let workflow = f.coordinated(request, "hermes-win");
    let first = route("impl", "implementation", "dsh-win", &workflow.user_message_id, request);
    let ids = f.store.enqueue_routes(&workflow.id, &[(first, None, None)]).unwrap();
    let chat = f
        .store
        .ensure_project_agent_chats(&f.room)
        .unwrap()
        .into_iter()
        .find(|chat| chat.agent_id == "dsh-win")
        .unwrap();
    let task = f.store.task(&ids[0]).unwrap();
    f.store
        .begin_coordinated_attempt(&workflow.id, Some(&task), "dsh-win", None, None)
        .unwrap();
    let error = f
        .store
        .begin_agent_run(&chat.conversation_id, "private-1", "补充一句", "dsh-win")
        .unwrap_err();
    assert!(error.contains("正在这条会话里做项目任务"), "{error}");
    assert!(f.store.detail(&chat.conversation_id).unwrap().messages.iter().all(|message| message.id != "private-1"), "拦下的话不存进聊天记录");
}
