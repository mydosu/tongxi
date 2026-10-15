//! 项目会话绑定：新建项目时可把各成员（含调度管家）接到它们原生已有的会话，未绑定就新建；
//! 项目进行中也能换绑。成员沿用项目专属私聊的会话映射，调度管家另存一张按群、按成员的绑定表。
//! 同一成员还能在项目里多开几个席位（`codex-win-2` …），每个席位一条自己的专属会话与模型。
use crate::{
    coordinator_policy::MANAGER_ACTIVITY,
    project_store::{base_agent, is_executor, EXECUTOR_AGENTS, SEAT_SLOTS},
    store::{now, Store},
};
use rusqlite::{params, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
type Result<T> = std::result::Result<T, String>;

const LIVE: &str = "('starting','running','cancelling')";
const ACTIVE_WORKFLOW: &str =
    "('queued','planning','running','verifying','reviewing','cancelling')";

pub fn migrate(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS project_coordinator_bindings (
           conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
           agent_id TEXT NOT NULL,native_session_id TEXT NOT NULL,cwd TEXT,updated_at INTEGER NOT NULL,
           PRIMARY KEY(conversation_id,agent_id));
         CREATE TABLE IF NOT EXISTS project_seats (
           conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
           seat_id TEXT NOT NULL,label TEXT NOT NULL,created_at INTEGER NOT NULL,
           PRIMARY KEY(conversation_id,seat_id));",
    )
    .map_err(|_| "调度会话绑定结构迁移失败".into())
}

/// 界面上一行绑定：成员的项目会话，或调度管家的会话。
#[derive(Clone, Debug, Serialize)]
pub struct SessionBinding {
    pub agent_id: String,
    /// `member` 或 `coordinator`。
    pub target: String,
    pub native_session_id: Option<String>,
    pub native_cwd: Option<String>,
    /// 正在跑任务时不能换绑。
    pub busy: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BindingChoice {
    pub agent_id: String,
    pub target: String,
    /// 空＝不绑定，下一轮新建会话。
    pub native_session_id: Option<String>,
    pub native_cwd: Option<String>,
}

fn clean(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(String::from)
}

impl Store {
    /// 调度管家在这个群里绑定的原生会话（按成员区分，换管家后各自保留）。
    pub fn coordinator_binding(
        &self,
        room: &str,
        agent: &str,
    ) -> Result<Option<(String, Option<String>)>> {
        self.connection
            .query_row(
                "SELECT native_session_id,cwd FROM project_coordinator_bindings WHERE conversation_id=?1 AND agent_id=?2",
                params![room, agent],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|_| "调度会话绑定读取失败".into())
    }

    /// 调度管家占用的原生会话：native_session_id → 「群名 · 调度」。
    pub fn coordinator_session_owners(&self, agent: &str) -> Result<Vec<(String, String, String)>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT b.native_session_id,c.title,b.conversation_id FROM project_coordinator_bindings b \
                 JOIN conversations c ON c.id=b.conversation_id WHERE b.agent_id=?1",
            )
            .map_err(|_| "调度会话绑定读取失败")?;
        let rows = statement
            .query_map([agent], |row| {
                Ok((
                    row.get(0)?,
                    format!("{} · 调度", row.get::<_, String>(1)?),
                    row.get(2)?,
                ))
            })
            .map_err(|_| "调度会话绑定读取失败")?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| "调度会话绑定读取失败".into())
    }

    /// 同一个原生会话同一时刻只挂一处：成员会话之外，再查调度绑定。
    pub fn coordinator_occupies(
        &self,
        agent: &str,
        native: &str,
        except_room: Option<&str>,
    ) -> Result<Option<String>> {
        Ok(self
            .coordinator_session_owners(agent)?
            .into_iter()
            .find(|(id, _, room)| id == native && Some(room.as_str()) != except_room)
            .map(|(_, owner, _)| owner))
    }

    /// 席位正在干活：项目 attempt 记在席位 id 上，专属会话里的私聊运行记在底层成员上。
    fn member_busy(&self, room: &str, chat: &str, agent: &str) -> Result<bool> {
        let attempt: bool = self.connection.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM project_attempts a JOIN workflows w ON w.id=a.workflow_id WHERE w.conversation_id=?1 AND a.agent_id=?2 AND a.status IN {LIVE} AND COALESCE(a.activity,'')<>?3)"),
            params![room, agent, MANAGER_ACTIVITY], |row| row.get(0),
        ).map_err(|_| "项目运行状态读取失败")?;
        let run: bool = self
            .connection
            .query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM runs WHERE conversation_id=?1 AND agent_id=?2 AND status IN {LIVE})"),
                params![chat, base_agent(agent)],
                |row| row.get(0),
            )
            .map_err(|_| "会话运行状态读取失败")?;
        Ok(attempt || run)
    }

    fn coordinator_busy(&self, room: &str, agent: &str) -> Result<bool> {
        self.connection.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM project_attempts a JOIN workflows w ON w.id=a.workflow_id WHERE w.conversation_id=?1 AND a.agent_id=?2 AND a.status IN {LIVE} AND a.activity=?3)"),
            params![room, agent, MANAGER_ACTIVITY], |row| row.get(0),
        ).map_err(|_| "调度运行状态读取失败".into())
    }

    /// 当前群的全部会话绑定：每位项目成员一行，外加当前调度管家一行。
    pub fn project_session_bindings(
        &mut self,
        room: &str,
        coordinator: &str,
    ) -> Result<Vec<SessionBinding>> {
        let chats = self.ensure_project_agent_chats(room)?;
        let mut bindings = Vec::new();
        for chat in &chats {
            let session = self
                .detail(&chat.conversation_id)?
                .sessions
                .into_iter()
                .find(|session| session.agent_id == base_agent(&chat.agent_id))
                .ok_or("项目成员原生会话不存在")?;
            bindings.push(SessionBinding {
                agent_id: chat.agent_id.clone(),
                target: "member".into(),
                native_session_id: session.native_session_id,
                native_cwd: session.native_cwd,
                busy: self.member_busy(room, &chat.conversation_id, &chat.agent_id)?,
            });
        }
        if chats.iter().any(|chat| chat.agent_id == coordinator) {
            let bound = self.coordinator_binding(room, coordinator)?;
            bindings.push(SessionBinding {
                agent_id: coordinator.to_owned(),
                target: "coordinator".into(),
                native_cwd: bound.as_ref().and_then(|(_, cwd)| cwd.clone()),
                native_session_id: bound.map(|(id, _)| id),
                busy: self.coordinator_busy(room, coordinator)?,
            });
        }
        Ok(bindings)
    }

    /// 换绑一位成员的项目会话。空＝解除绑定，下一轮由成员新建原生会话。
    pub fn bind_member_session(
        &mut self,
        room: &str,
        agent: &str,
        native: Option<&str>,
        cwd: Option<&str>,
    ) -> Result<()> {
        let chats = self.ensure_project_agent_chats(room)?;
        let chat = chats
            .iter()
            .find(|chat| chat.agent_id == agent)
            .ok_or("这位成员不在当前项目群")?;
        let current = self
            .detail(&chat.conversation_id)?
            .sessions
            .into_iter()
            .find(|session| session.agent_id == base_agent(agent))
            .and_then(|session| session.native_session_id);
        if current.as_deref() == native {
            return Ok(());
        }
        if self.member_busy(room, &chat.conversation_id, agent)? {
            return Err(format!(
                "{}正在执行项目任务，结束后再换会话",
                crate::store::agent_name(base_agent(agent))
            ));
        }
        let agent = base_agent(agent);
        match native {
            Some(native) => {
                if let Some(owner) = self.coordinator_occupies(agent, native, None)? {
                    return Err(format!("这个原生会话已被「{owner}」占用，先从那边换走"));
                }
                self.attach_native_session(&chat.conversation_id, agent, native, cwd)
            }
            None => {
                let root = self.conversation_project(room)?.ok_or("请先绑定项目")?.root;
                self.connection
                    .execute(
                        "UPDATE sessions SET native_session_id=NULL,native_cwd=?1 WHERE conversation_id=?2 AND agent_id=?3",
                        params![root, chat.conversation_id, agent],
                    )
                    .map_err(|_| "项目会话解绑失败")?;
                Ok(())
            }
        }
    }

    /// 群里给同一成员额外开的席位：(席位 id, 名字)。
    pub fn project_seats(&self, room: &str) -> Result<Vec<(String, String)>> {
        let mut statement = self
            .connection
            .prepare("SELECT seat_id,label FROM project_seats WHERE conversation_id=?1 ORDER BY seat_id")
            .map_err(|_| "项目席位读取失败")?;
        let rows = statement
            .query_map([room], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(|_| "项目席位读取失败")?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| "项目席位读取失败".into())
    }

    /// 给群里的某位成员再开一个席位：自己的专属会话、自己的模型，调度当独立成员派活。
    pub fn add_project_seat(&mut self, room: &str, agent: &str, label: Option<&str>) -> Result<String> {
        if !EXECUTOR_AGENTS.contains(&agent) {
            return Err("只有 Codex、Hermes、DSH、Claude 可以多开席位".into());
        }
        let conversation = self.conversation(room)?;
        if conversation.kind != "group" || conversation.archived || self.conversation_project(room)?.is_none() {
            return Err("请先绑定未归档项目群".into());
        }
        if !conversation.members.iter().any(|member| member == agent) {
            return Err("这位成员不在当前项目群".into());
        }
        let taken = self.project_seats(room)?;
        let slot = SEAT_SLOTS
            .iter()
            .find(|slot| !taken.iter().any(|(seat, _)| *seat == format!("{agent}-{slot}")))
            .ok_or(format!("{}最多再开 {} 个席位", crate::store::agent_name(agent), SEAT_SLOTS.len()))?;
        let seat = format!("{agent}-{slot}");
        let label = clean(label)
            .map(|label| label.chars().filter(|c| !c.is_control()).take(20).collect::<String>())
            .unwrap_or_else(|| format!("{} {slot}号", crate::store::agent_name(agent)));
        self.connection
            .execute(
                "INSERT INTO project_seats(conversation_id,seat_id,label,created_at) VALUES(?1,?2,?3,?4)",
                params![room, seat, label, now()],
            )
            .map_err(|_| "项目席位保存失败")?;
        self.ensure_project_agent_chats(room)?;
        Ok(seat)
    }

    /// 撤掉一个额外席位：正在干活或还有未完成任务时拒绝；它的专属会话归档保留，不删历史。
    pub fn remove_project_seat(&mut self, room: &str, seat: &str) -> Result<()> {
        if base_agent(seat) == seat || !is_executor(seat) {
            return Err("只能撤掉额外开的席位".into());
        }
        let chats = self.project_agent_chats(room)?;
        if let Some(chat) = chats.iter().find(|chat| chat.agent_id == seat) {
            if self.member_busy(room, &chat.conversation_id, seat)? {
                return Err("这个席位正在执行任务，结束后再撤".into());
            }
        }
        let pending: bool = self.connection.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM project_tasks t JOIN workflows w ON w.id=t.workflow_id WHERE w.conversation_id=?1 AND COALESCE(t.assigned_agent,t.agent_id)=?2 AND t.status IN ('queued','running') AND w.status IN {ACTIVE_WORKFLOW})"),
            params![room, seat], |row| row.get(0),
        ).map_err(|_| "席位任务读取失败")?;
        if pending {
            return Err("这个席位还有未完成的项目任务，先停止或等它完成".into());
        }
        let tx = self.connection.transaction().map_err(|_| "席位撤销失败")?;
        tx.execute(
            "UPDATE conversations SET archived=1 WHERE id IN (SELECT conversation_id FROM project_agent_chats WHERE group_conversation_id=?1 AND agent_id=?2)",
            params![room, seat],
        )
        .map_err(|_| "席位会话归档失败")?;
        tx.execute(
            "DELETE FROM project_agent_chats WHERE group_conversation_id=?1 AND agent_id=?2",
            params![room, seat],
        )
        .map_err(|_| "席位撤销失败")?;
        tx.execute(
            "DELETE FROM project_seats WHERE conversation_id=?1 AND seat_id=?2",
            params![room, seat],
        )
        .map_err(|_| "席位撤销失败")?;
        tx.commit().map_err(|_| "席位撤销失败".into())
    }

    /// 换绑调度管家的会话。进行中的协作也一起切过去：下一次调度就续在新会话里（空＝新开）。
    pub fn bind_coordinator_session(
        &mut self,
        room: &str,
        agent: &str,
        native: Option<&str>,
        cwd: Option<&str>,
    ) -> Result<()> {
        if !EXECUTOR_AGENTS.contains(&agent) {
            return Err("管家应从本项目的Codex、Hermes、DSH或Claude成员中选择".into());
        }
        let conversation = self.conversation(room)?;
        if conversation.kind != "group" || self.conversation_project(room)?.is_none() {
            return Err("请先绑定项目".into());
        }
        if !conversation.members.iter().any(|member| member == agent) {
            return Err("管家应从本项目成员中选择".into());
        }
        let previous = self.coordinator_binding(room, agent)?.map(|(id, _)| id);
        if previous.as_deref() == native {
            return Ok(());
        }
        if self.coordinator_busy(room, agent)? {
            return Err("调度管家正在安排任务，结束后再换会话".into());
        }
        if let Some(native) = native {
            let member: Option<String> = self.connection.query_row(
                "SELECT c.title FROM sessions s JOIN conversations c ON c.id=s.conversation_id WHERE s.agent_id=?1 AND s.native_session_id=?2",
                params![agent, native], |row| row.get(0),
            ).optional().map_err(|_| "原生会话占用读取失败")?;
            if let Some(owner) = member.or(self.coordinator_occupies(agent, native, Some(room))?) {
                return Err(format!("这个原生会话已被「{owner}」占用，先从那边换走"));
            }
        }
        let tx = self
            .connection
            .transaction()
            .map_err(|_| "调度会话绑定失败")?;
        match native {
            Some(native) => {
                tx.execute(
                    "INSERT INTO project_coordinator_bindings(conversation_id,agent_id,native_session_id,cwd,updated_at) VALUES(?1,?2,?3,?4,?5) \
                     ON CONFLICT(conversation_id,agent_id) DO UPDATE SET native_session_id=excluded.native_session_id,cwd=excluded.cwd,updated_at=excluded.updated_at",
                    params![room, agent, native, cwd, now()],
                ).map_err(|_| "调度会话绑定失败")?;
                // 进行中的协作已有自己的调度会话：改指向新绑定，下一次调度生效。
                tx.execute(
                    &format!("UPDATE coordinator_sessions SET native_session_id=?1 WHERE agent_id=?2 AND workflow_id IN (SELECT id FROM workflows WHERE conversation_id=?3 AND status IN {ACTIVE_WORKFLOW})"),
                    params![native, agent, room],
                ).map_err(|_| "进行中协作的调度会话切换失败")?;
            }
            None => {
                tx.execute(
                    "DELETE FROM project_coordinator_bindings WHERE conversation_id=?1 AND agent_id=?2",
                    params![room, agent],
                )
                .map_err(|_| "调度会话解绑失败")?;
                // 正在用旧绑定的协作改为下一次调度新开会话，避免旧会话同时挂在两处。
                if let Some(previous) = previous {
                    tx.execute(
                        &format!("UPDATE coordinator_sessions SET native_session_id=NULL WHERE agent_id=?1 AND native_session_id=?2 AND workflow_id IN (SELECT id FROM workflows WHERE conversation_id=?3 AND status IN {ACTIVE_WORKFLOW})"),
                        params![agent, previous, room],
                    ).map_err(|_| "进行中协作的调度会话切换失败")?;
                }
            }
        }
        tx.commit().map_err(|_| "调度会话绑定保存失败".into())
    }
}

fn configured_coordinator(store: &Store, room: &str) -> Result<String> {
    if let Some(choice) = store.project_coordinator(room)? {
        return Ok(choice.agent);
    }
    let members = store.conversation(room)?.members;
    ["hermes-win", "codex-win", "dsh-win", "claude-win"]
        .iter()
        .find(|id| members.iter().any(|member| member == **id))
        .map(|id| id.to_string())
        .ok_or_else(|| "项目没有可用技术成员".into())
}

#[tauri::command]
pub fn project_session_bindings(
    runtime: tauri::State<'_, Arc<crate::projects::Runtime>>,
    conversation_id: String,
) -> Result<Vec<SessionBinding>> {
    let mut store = runtime.store.lock().unwrap();
    let coordinator = configured_coordinator(&store, &conversation_id)?;
    store.project_session_bindings(&conversation_id, &coordinator)
}

#[tauri::command]
pub fn add_project_seat(
    runtime: tauri::State<'_, Arc<crate::projects::Runtime>>,
    conversation_id: String,
    agent_id: String,
    label: Option<String>,
) -> Result<String> {
    runtime
        .store
        .lock()
        .unwrap()
        .add_project_seat(&conversation_id, &agent_id, label.as_deref())
}

#[tauri::command]
pub fn remove_project_seat(
    runtime: tauri::State<'_, Arc<crate::projects::Runtime>>,
    conversation_id: String,
    seat_id: String,
) -> Result<crate::store::ConversationDetail> {
    let mut store = runtime.store.lock().unwrap();
    store.remove_project_seat(&conversation_id, &seat_id)?;
    store.detail(&conversation_id)
}

/// 一次保存多行绑定；任一行失败就停下并报出是哪位成员，已保存的行保留。
#[tauri::command]
pub fn bind_project_sessions(
    runtime: tauri::State<'_, Arc<crate::projects::Runtime>>,
    conversation_id: String,
    bindings: Vec<BindingChoice>,
) -> Result<Vec<SessionBinding>> {
    let mut store = runtime.store.lock().unwrap();
    for choice in &bindings {
        let native = clean(choice.native_session_id.as_deref());
        let cwd = clean(choice.native_cwd.as_deref());
        let result = match choice.target.as_str() {
            "member" => store.bind_member_session(
                &conversation_id,
                &choice.agent_id,
                native.as_deref(),
                cwd.as_deref(),
            ),
            "coordinator" => store.bind_coordinator_session(
                &conversation_id,
                &choice.agent_id,
                native.as_deref(),
                cwd.as_deref(),
            ),
            _ => Err("绑定对象无效".into()),
        };
        result.map_err(|error| {
            let role = if choice.target == "coordinator" {
                "调度"
            } else {
                "成员"
            };
            format!(
                "{}（{role}）：{error}",
                crate::store::agent_name(base_agent(&choice.agent_id))
            )
        })?;
    }
    let coordinator = configured_coordinator(&store, &conversation_id)?;
    store.project_session_bindings(&conversation_id, &coordinator)
}

#[cfg(test)]
mod tests {
    use crate::store::Store;
    use uuid::Uuid;

    fn room() -> (Store, String, std::path::PathBuf) {
        let base = std::env::temp_dir().join(format!("tongxi-session-binding-{}", Uuid::new_v4()));
        let root = base.join("project");
        std::fs::create_dir_all(&root).unwrap();
        let mut store = Store::open(&base.join("data").join("hub.db")).unwrap();
        let room = store
            .create(
                "项目群",
                "group",
                &["codex-win".into(), "hermes-win".into()],
            )
            .unwrap()
            .id;
        let project = store
            .register_project("绑定项目", root.to_str().unwrap())
            .unwrap()
            .id;
        store.bind_project(&room, Some(&project)).unwrap();
        (store, room, base)
    }

    #[test]
    fn members_and_coordinator_bind_existing_sessions_and_can_switch_back_to_new() {
        let (mut store, room, base) = room();
        store
            .bind_member_session(
                &room,
                "codex-win",
                Some("codex-thread"),
                Some("D:/elsewhere"),
            )
            .unwrap();
        store
            .bind_coordinator_session(&room, "hermes-win", Some("hermes-old"), None)
            .unwrap();
        let bindings = store.project_session_bindings(&room, "hermes-win").unwrap();
        let codex = bindings
            .iter()
            .find(|b| b.agent_id == "codex-win" && b.target == "member")
            .unwrap();
        assert_eq!(codex.native_session_id.as_deref(), Some("codex-thread"));
        let coordinator = bindings.iter().find(|b| b.target == "coordinator").unwrap();
        assert_eq!(coordinator.native_session_id.as_deref(), Some("hermes-old"));
        // 中途换成「新建」：成员会话清空、目录回到项目根；调度绑定删除。
        store
            .bind_member_session(&room, "codex-win", None, None)
            .unwrap();
        store
            .bind_coordinator_session(&room, "hermes-win", None, None)
            .unwrap();
        let bindings = store.project_session_bindings(&room, "hermes-win").unwrap();
        assert!(bindings.iter().all(|b| b.native_session_id.is_none()));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn one_native_session_cannot_serve_a_member_and_the_coordinator_at_once() {
        let (mut store, room, base) = room();
        store
            .bind_coordinator_session(&room, "codex-win", Some("shared"), None)
            .unwrap();
        let error = store
            .bind_member_session(&room, "codex-win", Some("shared"), None)
            .unwrap_err();
        assert!(error.contains("调度"), "{error}");
        store
            .bind_coordinator_session(&room, "codex-win", None, None)
            .unwrap();
        store
            .bind_member_session(&room, "codex-win", Some("shared"), None)
            .unwrap();
        let error = store
            .bind_coordinator_session(&room, "codex-win", Some("shared"), None)
            .unwrap_err();
        assert!(error.contains("占用"), "{error}");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn switching_the_coordinator_redirects_active_workflows() {
        let (mut store, room, base) = room();
        let message = store
            .save_message(&room, &Uuid::new_v4().to_string(), "做个功能")
            .unwrap();
        let (workflow, _) = store
            .start_workflow(&room, &message.id, "做个功能")
            .unwrap();
        store
            .bind_manager_session(&workflow.id, "hermes-win", "hidden", &base)
            .unwrap();
        store
            .bind_coordinator_session(&room, "hermes-win", Some("picked"), None)
            .unwrap();
        assert_eq!(
            store.manager_session(&workflow.id).unwrap().as_deref(),
            Some("picked")
        );
        store
            .bind_coordinator_session(&room, "hermes-win", None, None)
            .unwrap();
        assert_eq!(store.manager_session(&workflow.id).unwrap(), None);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn one_agent_can_hold_several_project_seats_with_their_own_sessions() {
        use crate::project_store::{base_agent, is_executor};
        assert_eq!(base_agent("codex-win-2"), "codex-win");
        assert_eq!(base_agent("codex-win"), "codex-win");
        assert_eq!(base_agent("codex-win-9"), "codex-win-9");
        assert!(is_executor("claude-win-4") && !is_executor("albion-wsl-2"));
        let (mut store, room, base) = room();
        let seat = store.add_project_seat(&room, "codex-win", Some("快手")).unwrap();
        assert_eq!(seat, "codex-win-2");
        assert_eq!(store.add_project_seat(&room, "codex-win", None).unwrap(), "codex-win-3");
        assert!(store.add_project_seat(&room, "dsh-win", None).is_err(), "不在群里的成员不能开席位");
        let chats = store.project_agent_chats(&room).unwrap();
        let chat = chats.iter().find(|chat| chat.agent_id == seat).unwrap();
        assert_eq!(chat.label.as_deref(), Some("快手"));
        // 席位的专属会话里，原生成员仍是 Codex 本人；会话映射与主席位互不相干。
        let detail = store.detail(&chat.conversation_id).unwrap();
        assert_eq!(detail.conversation.members, vec!["codex-win".to_string()]);
        store.bind_member_session(&room, "codex-win", Some("primary-thread"), None).unwrap();
        store.bind_member_session(&room, &seat, Some("second-thread"), None).unwrap();
        let bindings = store.project_session_bindings(&room, "hermes-win").unwrap();
        let of = |id: &str| bindings.iter().find(|b| b.agent_id == id && b.target == "member").unwrap().native_session_id.clone();
        assert_eq!(of("codex-win").as_deref(), Some("primary-thread"));
        assert_eq!(of(&seat).as_deref(), Some("second-thread"));
        // 同一个原生会话不能同时挂在两个席位上。
        assert!(store.bind_member_session(&room, "codex-win-3", Some("second-thread"), None).is_err());
        // 席位 id 能写进项目运行记录（成员白名单已放宽）。
        let message = store.save_message(&room, &Uuid::new_v4().to_string(), "两个 Codex 一起做").unwrap();
        let (workflow, _) = store.start_workflow(&room, &message.id, "两个 Codex 一起做").unwrap();
        store.connection.execute(
            "INSERT INTO project_attempts(id,workflow_id,agent_id,stage,status,created_at) VALUES(?1,?2,?3,'implement','completed',0)",
            rusqlite::params![Uuid::new_v4().to_string(), workflow.id, seat],
        ).unwrap();
        store.remove_project_seat(&room, &seat).unwrap();
        assert!(store.project_agent_chats(&room).unwrap().iter().all(|chat| chat.agent_id != seat));
        assert!(store.conversation(&chat.conversation_id).unwrap().archived, "撤掉的席位会话归档保留");
        assert!(store.remove_project_seat(&room, "codex-win").is_err(), "主席位不能撤");
        let _ = std::fs::remove_dir_all(base);
    }
}
