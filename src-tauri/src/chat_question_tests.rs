use crate::{chat_questions, store::Store};
use rusqlite::Connection;
use serde_json::json;
use std::{path::PathBuf, time::Duration};
use uuid::Uuid;

fn fixture() -> (Store, PathBuf) {
    let root = std::env::temp_dir().join(format!("hub-private-qa-{}", Uuid::new_v4()));
    let store = Store::open(&root.join("data/hub.db")).unwrap();
    (store, root)
}

#[test]
fn private_question_is_idempotent_scoped_and_does_not_enter_the_team() {
    let (mut store, root) = fixture();
    let group = store
        .create(
            "团队",
            "group",
            &["codex-win".into(), "hermes-win".into(), "dsh-win".into()],
        )
        .unwrap();
    let project_dir = root.join("project");
    std::fs::create_dir_all(&project_dir).unwrap();
    let project = store
        .register_project("项目", project_dir.to_str().unwrap())
        .unwrap();
    store.bind_project(&group.id, Some(&project.id)).unwrap();
    let room = store
        .ensure_project_agent_chats(&group.id)
        .unwrap()
        .into_iter()
        .find(|chat| chat.agent_id == "codex-win")
        .unwrap()
        .conversation_id;
    let mut run = store
        .begin_agent_run(
            &room,
            &Uuid::new_v4().to_string(),
            "提出一个问题",
            "codex-win",
        )
        .unwrap();
    run.status = "completed".into();
    let text="```tongxi-question\n{\"question\":\"PRIVATE_ONLY：选A还是B？\",\"options\":[\"A\",\"B\"]}\n```";
    store.checkpoint(&run, text, "").unwrap();
    store.checkpoint(&run, text, "").unwrap();
    let questions = store.chat_questions(&room).unwrap();
    assert_eq!(questions.len(), 1);
    assert!(store.chat_questions(&group.id).unwrap().is_empty());
    assert!(store.team_messages(&group.id).unwrap().is_empty());
    let pending = store.pending_chat_questions().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].group_id.as_deref(), Some(group.id.as_str()));
    assert!(!serde_json::to_string(&pending)
        .unwrap()
        .contains("PRIVATE_ONLY"));
    assert!(store
        .answer_chat_question(&group.id, &questions[0].id, "B")
        .is_err());
    store
        .answer_chat_question(&room, &questions[0].id, "B")
        .unwrap();
    assert!(store
        .answer_chat_question(&room, &questions[0].id, "A")
        .is_err());
    assert!(store.pending_chat_questions().unwrap().is_empty());
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn native_question_waits_for_actual_answer_instead_of_returning_empty_answers() {
    let (mut store, root) = fixture();
    let room = store
        .create("Codex", "direct", &["codex-win".into()])
        .unwrap();
    let run = store
        .begin_agent_run(
            &room.id,
            &Uuid::new_v4().to_string(),
            "测试原生问题",
            "codex-win",
        )
        .unwrap();
    let db = store.path.clone();
    let run_id = run.id.clone();
    let thread = std::thread::spawn(move || {
        chat_questions::ask_native(
            &db,
            &run_id,
            &json!({"questions":[{"id":"native-q","question":"选择","options":[{"label":"A"},{"label":"B"}]}]}),
        )
    });
    let mut question = None;
    for _ in 0..100 {
        question = store.chat_questions(&room.id).unwrap().into_iter().next();
        if question.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!thread.is_finished());
    let question = question.unwrap();
    store
        .answer_chat_question(&room.id, &question.id, "B")
        .unwrap();
    assert_eq!(
        thread.join().unwrap().unwrap(),
        json!({"answers":{"native-q":{"answers":["B"]}}})
    );
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn ordinary_private_mcp_question_works_for_three_technical_members() {
    let (mut store, root) = fixture();
    for agent in ["codex-win", "hermes-win", "dsh-win"] {
        let room = store.create("私聊", "direct", &[agent.into()]).unwrap();
        let mut run = store
            .begin_agent_run(&room.id, &Uuid::new_v4().to_string(), "澄清目标", agent)
            .unwrap();
        let value = chat_questions::tool(
            &store.path,
            &room.id,
            json!({"question":"选哪个？","options":["一","二"]}),
        )
        .unwrap();
        assert_eq!(value["awaiting_user"], true);
        run.status = "completed".into();
        store.checkpoint(&run, "问题已经显示", "").unwrap();
        let q = store.chat_questions(&room.id).unwrap().pop().unwrap();
        assert_eq!(q.agent_id, agent);
        store.cancel_chat_question(&room.id, &q.id).unwrap();
        assert_eq!(
            store.chat_questions(&room.id).unwrap()[0].status,
            "cancelled"
        );
    }
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn restart_preserves_finished_private_questions_but_cancels_native_waits_and_does_not_replay() {
    let (mut store, root) = fixture();
    let room = store.create("私聊", "direct", &["dsh-win".into()]).unwrap();
    let mut run = store
        .begin_agent_run(&room.id, &Uuid::new_v4().to_string(), "问答", "dsh-win")
        .unwrap();
    run.status = "completed".into();
    store
        .checkpoint(
            &run,
            "```tongxi-question\n{\"question\":\"选择目标\",\"options\":[]}\n```",
            "",
        )
        .unwrap();
    let q = store.chat_questions(&room.id).unwrap()[0].clone();
    drop(store);
    let mut store = Store::open(&root.join("data/hub.db")).unwrap();
    assert_eq!(store.chat_questions(&room.id).unwrap()[0].status, "pending");
    store
        .answer_chat_question(&room.id, &q.id, "目标A")
        .unwrap();
    drop(store);
    let store = Store::open(&root.join("data/hub.db")).unwrap();
    assert_eq!(store.chat_questions(&room.id).unwrap()[0].status, "pending");
    assert_eq!(
        store
            .connection
            .query_row("SELECT count(*) FROM runs", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn schema_fifteen_upgrade_keeps_history_and_adds_private_questions() {
    let mut store = Store::initialize(
        Connection::open_in_memory().unwrap(),
        PathBuf::from(":memory:"),
    )
    .unwrap();
    let room = store
        .create("旧会话", "direct", &["hermes-win".into()])
        .unwrap();
    store
        .save_message(&room.id, &Uuid::new_v4().to_string(), "旧消息")
        .unwrap();
    store
        .connection
        .execute_batch("DROP TABLE chat_questions; PRAGMA user_version=15;")
        .unwrap();
    let store = Store::initialize(store.connection, PathBuf::from(":memory:")).unwrap();
    assert_eq!(
        store.detail(&room.id).unwrap().messages[0].content,
        "旧消息"
    );
    assert!(store.chat_questions(&room.id).unwrap().is_empty());
    assert_eq!(
        store
            .connection
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        16
    );
}
