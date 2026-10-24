//! 项目会话绑定：新建项目时可把各成员（含主脑）接到它们原生已有的会话，未绑定就新建；
//! 项目进行中也能换绑。成员沿用项目专属私聊的会话映射；旧调度管家的绑定表只留着数据。
//! 同一成员还能在项目里多开几个席位（`codex-win-2` …），每个席位一条自己的专属会话与模型。
use crate::{
    project_store::{base_agent, is_executor, EXECUTOR_AGENTS, SEAT_SLOTS},
    store::{now, Store},
};
use rusqlite::{params, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
type Result<T> = std::result::Result<T, String>;

const LIVE: &str = "('starting','running','cancelling')";

pub fn migrate(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS project_coordinator_bindings (
           conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
           agent_id TEXT NOT NULL,native_session_id TEXT NOT NULL,cwd TEXT,updated_at INTEGER NOT NULL,
           PRIMARY KEY(conversation_id,agent_id));
         CREATE TABLE IF NOT EXISTS project_seats (
           conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
           seat_id TEXT NOT NULL,label TEXT NOT NULL,created_at INTEGER NOT NULL,
           PRIMARY KEY(conversation_id,seat_id));
         CREATE TABLE IF NOT EXISTS project_member_names (
           conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
           agent_id TEXT NOT NULL,label TEXT NOT NULL,PRIMARY KEY(conversation_id,agent_id));
         -- 额外席位没改过名时存空串，显示时按「成员名 N号」现算；早先写死的默认名转成空串。
         UPDATE project_seats SET label='' WHERE label = (CASE
           WHEN seat_id LIKE 'codex-win-%' THEN 'Codex' WHEN seat_id LIKE 'hermes-win-%' THEN 'Hermes'
           WHEN seat_id LIKE 'dsh-win-%' THEN 'DSH' WHEN seat_id LIKE 'claude-win-%' THEN 'Claude' WHEN seat_id LIKE 'opencode-win-%' THEN 'OpenCode' END)
           || ' ' || substr(seat_id, -1) || '号';",
    )
    .map_err(|_| "调度会话绑定结构迁移失败")?;
    // 调度为某次协作临时开的会话：记下属于哪次协作，协作结束后撤掉。
    crate::project_store::add_column(tx, "project_seats", "temporary_for", "TEXT")
}

/// 界面上一行绑定：成员（含主脑）的项目会话。
#[derive(Clone, Debug, Serialize)]
pub struct SessionBinding {
    pub agent_id: String,
    /// 固定为 `member`。
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

    /// 席位正在干活：主脑派的活和私聊都跑在专属会话里，运行记在底层成员上。
    fn member_busy(&self, chat: &str, agent: &str) -> Result<bool> {
        let run: bool = self
            .connection
            .query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM runs WHERE conversation_id=?1 AND agent_id=?2 AND status IN {LIVE})"),
                params![chat, base_agent(agent)],
                |row| row.get(0),
            )
            .map_err(|_| "会话运行状态读取失败")?;
        Ok(run)
    }

    /// 当前群的全部会话绑定：每位项目成员一行，外加当前调度管家一行。
    pub fn project_session_bindings(&mut self, room: &str) -> Result<Vec<SessionBinding>> {
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
                busy: self.member_busy(&chat.conversation_id, &chat.agent_id)?,
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
        if self.member_busy(&chat.conversation_id, agent)? {
            return Err(format!(
                "{}正在执行项目任务，结束后再换会话",
                crate::store::agent_name(base_agent(agent))
            ));
        }
        let agent = base_agent(agent);
        match native {
            Some(native) => self.attach_native_session(&chat.conversation_id, agent, native, cwd),
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
        // 不起名就存空串：显示时按「成员名 N号」现算，成员改名后跟着变。
        let label = clean(label)
            .map(|label| label.chars().filter(|c| !c.is_control()).take(20).collect::<String>())
            .unwrap_or_default();
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
    /// 成员名（全局）：用户在「成员与服务」里起的名字；陪伴用它自己配置里的名字；都没有就是默认名。
    pub fn member_name(&self, agent: &str) -> String {
        if agent == "albion-wsl" {
            return self.companion().name;
        }
        self.connection
            .query_row("SELECT value FROM metadata WHERE key=?1", [format!("agent_name:{agent}")], |row| row.get::<_, String>(0))
            .optional()
            .ok()
            .flatten()
            .unwrap_or_else(|| crate::store::agent_name(agent).to_owned())
    }

    /// 改成员名（全局）；空名字＝恢复默认。陪伴的名字写回它自己的配置。
    pub fn rename_member(&mut self, agent: &str, name: &str) -> Result<String> {
        if !crate::store::agents().iter().any(|item| item.id == agent) {
            return Err("成员不存在".into());
        }
        let name: String = name.trim().chars().filter(|c| !c.is_control()).take(20).collect();
        if agent == "albion-wsl" {
            let mut config = self.companion();
            config.name = if name.is_empty() { "陪伴".into() } else { name };
            let text = serde_json::to_string(&config).map_err(|_| "陪伴配置保存失败")?;
            self.connection
                .execute("INSERT INTO metadata(key,value) VALUES('companion',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [text])
                .map_err(|_| "陪伴配置保存失败")?;
        } else if name.is_empty() || name == crate::store::agent_name(agent) {
            self.connection.execute("DELETE FROM metadata WHERE key=?1", [format!("agent_name:{agent}")]).map_err(|_| "成员改名失败")?;
        } else {
            self.connection
                .execute(
                    "INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                    params![format!("agent_name:{agent}"), name],
                )
                .map_err(|_| "成员改名失败")?;
        }
        Ok(self.member_name(agent))
    }

    pub fn seat_name(&self, room: &str, seat: &str) -> String {
        if crate::project_store::is_brain(seat) {
            return "主脑".into();
        }
        let base = base_agent(seat);
        let custom: Option<String> = if base == seat {
            self.connection
                .query_row("SELECT label FROM project_member_names WHERE conversation_id=?1 AND agent_id=?2", params![room, seat], |row| row.get(0))
                .optional()
                .ok()
                .flatten()
        } else {
            self.connection
                .query_row("SELECT label FROM project_seats WHERE conversation_id=?1 AND seat_id=?2", params![room, seat], |row| row.get(0))
                .optional()
                .ok()
                .flatten()
        };
        if let Some(label) = custom.filter(|label: &String| !label.is_empty()) {
            return label;
        }
        let several = self
            .project_seats(room)
            .map(|seats| seats.iter().any(|(id, _)| base_agent(id) == base))
            .unwrap_or(false);
        match (base == seat, several) {
            (true, false) => self.member_name(seat),
            (true, true) => format!("{} 1号", self.member_name(seat)),
            _ => format!("{} {}号", self.member_name(base), seat.rsplit('-').next().unwrap_or("")),
        }
    }

    /// 给这位成员在项目里的某个会话起名（每个会话都行）；空名字＝恢复默认。
    /// 只在这个项目里生效，成员的全局名字不变。
    pub fn rename_project_seat(&mut self, room: &str, seat: &str, label: &str) -> Result<String> {
        let label: String = label.trim().chars().filter(|c| !c.is_control()).take(20).collect();
        let base = base_agent(seat);
        if base == seat {
            let members = self.conversation(room)?.members;
            if !EXECUTOR_AGENTS.contains(&seat) || !members.iter().any(|member| member == seat) || self.conversation_project(room)?.is_none() {
                return Err("这位成员不在当前项目群".into());
            }
            if label.is_empty() {
                self.connection.execute("DELETE FROM project_member_names WHERE conversation_id=?1 AND agent_id=?2", params![room, seat]).map_err(|_| "席位改名失败")?;
            } else {
                self.connection
                    .execute(
                        "INSERT INTO project_member_names(conversation_id,agent_id,label) VALUES(?1,?2,?3) ON CONFLICT(conversation_id,agent_id) DO UPDATE SET label=excluded.label",
                        params![room, seat, label],
                    )
                    .map_err(|_| "席位改名失败")?;
            }
        } else {
            let changed = self
                .connection
                .execute("UPDATE project_seats SET label=?1 WHERE conversation_id=?2 AND seat_id=?3", params![label, room, seat])
                .map_err(|_| "席位改名失败")?;
            if changed == 0 {
                return Err("这个席位不在当前项目群".into());
            }
        }
        let name = self.seat_name(room, seat);
        if let (Some(project), Some(chat)) = (
            self.conversation_project(room)?,
            self.project_agent_chats(room)?.into_iter().find(|chat| chat.agent_id == seat),
        ) {
            self.rename(&chat.conversation_id, &format!("{} · {name}", project.name))?;
        }
        Ok(name)
    }

    pub fn remove_project_seat(&mut self, room: &str, seat: &str) -> Result<()> {
        if base_agent(seat) == seat || !is_executor(seat) || crate::project_store::is_brain(seat) {
            return Err("只能撤掉额外开的席位".into());
        }
        let chats = self.project_agent_chats(room)?;
        if let Some(chat) = chats.iter().find(|chat| chat.agent_id == seat) {
            if self.member_busy(&chat.conversation_id, seat)? {
                return Err("这个席位正在执行任务，结束后再撤".into());
            }
        }
        if self.brain_busy(seat) {
            return Err("这个席位手上有主脑派的活，先停止或等它交回".into());
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

}

#[tauri::command]
pub fn project_session_bindings(
    state: tauri::State<'_, crate::AppState>,
    conversation_id: String,
) -> Result<Vec<SessionBinding>> {
    let mut store = state.store.lock().map_err(|_| "本地数据暂不可用")?;
    store.project_session_bindings(&conversation_id)
}

#[tauri::command]
pub fn add_project_seat(
    state: tauri::State<'_, crate::AppState>,
    conversation_id: String,
    agent_id: String,
    label: Option<String>,
) -> Result<String> {
    state
        .store
        .lock()
        .map_err(|_| "本地数据暂不可用")?
        .add_project_seat(&conversation_id, &agent_id, label.as_deref())
}

#[tauri::command]
pub fn remove_project_seat(
    state: tauri::State<'_, crate::AppState>,
    conversation_id: String,
    seat_id: String,
) -> Result<crate::store::ConversationDetail> {
    let mut store = state.store.lock().map_err(|_| "本地数据暂不可用")?;
    store.remove_project_seat(&conversation_id, &seat_id)?;
    store.detail(&conversation_id)
}

/// 改成员名（全局）。
#[tauri::command]
pub fn rename_agent(state: tauri::State<'_, crate::AppState>, agent_id: String, name: String) -> Result<String> {
    state.store.lock().map_err(|_| "本地数据暂不可用")?.rename_member(&agent_id, &name)
}

#[tauri::command]
pub fn rename_project_seat(
    state: tauri::State<'_, crate::AppState>,
    conversation_id: String,
    seat_id: String,
    label: String,
) -> Result<String> {
    state.store.lock().map_err(|_| "本地数据暂不可用")?.rename_project_seat(&conversation_id, &seat_id, &label)
}

/// 一次保存多行绑定；任一行失败就停下并报出是哪位成员，已保存的行保留。
#[tauri::command]
pub fn bind_project_sessions(
    state: tauri::State<'_, crate::AppState>,
    conversation_id: String,
    bindings: Vec<BindingChoice>,
) -> Result<Vec<SessionBinding>> {
    let mut store = state.store.lock().map_err(|_| "本地数据暂不可用")?;
    for choice in &bindings {
        let native = clean(choice.native_session_id.as_deref());
        let cwd = clean(choice.native_cwd.as_deref());
        if choice.target != "member" {
            return Err("绑定对象无效".into());
        }
        store
            .bind_member_session(&conversation_id, &choice.agent_id, native.as_deref(), cwd.as_deref())
            .map_err(|error| format!("{}：{error}", store.seat_name(&conversation_id, &choice.agent_id)))?;
    }
    store.project_session_bindings(&conversation_id)
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
        let bindings = store.project_session_bindings(&room).unwrap();
        let of = |id: &str| bindings.iter().find(|b| b.agent_id == id && b.target == "member").unwrap().native_session_id.clone();
        assert_eq!(of("codex-win").as_deref(), Some("primary-thread"));
        assert_eq!(of(&seat).as_deref(), Some("second-thread"));
        // 同一个原生会话不能同时挂在两个席位上。
        assert!(store.bind_member_session(&room, "codex-win-3", Some("second-thread"), None).is_err());
        store.remove_project_seat(&room, &seat).unwrap();
        assert!(store.project_agent_chats(&room).unwrap().iter().all(|chat| chat.agent_id != seat));
        assert!(store.conversation(&chat.conversation_id).unwrap().archived, "撤掉的席位会话归档保留");
        assert!(store.remove_project_seat(&room, "codex-win").is_err(), "主席位不能撤");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn replies_in_a_seat_chat_are_recorded_under_the_seat() {
        let (mut store, room, base) = room();
        let seat = store.add_project_seat(&room, "codex-win", Some("快手")).unwrap();
        let chat = store.project_agent_chats(&room).unwrap().into_iter().find(|chat| chat.agent_id == seat).unwrap();
        let mut run = store.begin_agent_run(&chat.conversation_id, &Uuid::new_v4().to_string(), "做事", "codex-win").unwrap();
        assert_eq!(run.agent_id, "codex-win", "运行仍按成员走");
        store.mark_turn(&mut run, "turn").unwrap();
        run.status = "completed".into();
        store.checkpoint(&run, "做完了", "").unwrap();
        assert_eq!(store.detail(&chat.conversation_id).unwrap().messages[1].sender_id, seat);
        // 分享按「发送者 = 席位」认回复，额外席位的回复也能分享。
        store.share_project_agent_message(&room, &chat.conversation_id, &run.assistant_message_id).unwrap();
        // 旧版本记在成员名下的回复，重新打开（跑迁移）后改记席位。
        let db = base.join("data").join("hub.db");
        drop(store);
        rusqlite::Connection::open(&db).unwrap()
            .execute("UPDATE messages SET sender_id='codex-win' WHERE id=?1", [&run.assistant_message_id]).unwrap();
        let store = Store::open(&db).unwrap();
        assert_eq!(store.detail(&chat.conversation_id).unwrap().messages[1].sender_id, seat);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn an_archived_project_group_no_longer_holds_its_directory() {
        let (mut store, room, base) = room();
        let root = base.join("project");
        let original = store.conversation_project(&room).unwrap().unwrap();
        let error = store.register_project("新名字", root.to_str().unwrap()).unwrap_err();
        assert!(error.contains("项目群"), "还在用的群要点名：{error}");
        store.archive(&room, true).unwrap();
        let renamed = store.register_project("新名字", root.to_str().unwrap()).unwrap();
        assert_eq!((renamed.id.as_str(), renamed.name.as_str()), (original.id.as_str(), "新名字"), "沿用原登记，只改名");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn releasing_a_project_frees_its_directory_but_keeps_history() {
        let (mut store, room, base) = room();
        let root = base.join("project");
        let project = store.conversation_project(&room).unwrap().unwrap();
        assert!(store.release_project(&project.id).unwrap_err().contains("项目群"), "还在用的群不能解除");
        store.archive(&room, true).unwrap();
        store.release_project(&project.id).unwrap();
        assert!(store.projects().unwrap().is_empty(), "从已登记列表里拿掉");
        assert!(store.conversation_project(&room).unwrap().is_none(), "归档群的绑定解开");
        let fresh = store.register_project("另一个", root.to_str().unwrap()).unwrap();
        assert_ne!(fresh.id, project.id, "目录空出来，能登记成新项目");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn extra_seats_can_be_renamed_but_primary_seats_cannot() {
        let (mut store, room, base) = room();
        let seat = store.add_project_seat(&room, "codex-win", None).unwrap();
        assert_eq!(store.rename_project_seat(&room, &seat, "  审稿  ").unwrap(), "审稿");
        let chat = store.project_agent_chats(&room).unwrap().into_iter().find(|chat| chat.agent_id == seat).unwrap();
        assert_eq!(chat.label.as_deref(), Some("审稿"));
        assert_eq!(store.conversation(&chat.conversation_id).unwrap().title, "绑定项目 · 审稿", "专属会话标题跟着改");
        // 清空＝恢复默认「成员名 N号」，成员改名后默认名跟着变。
        assert_eq!(store.rename_project_seat(&room, &seat, " ").unwrap(), "Codex 2号");
        store.rename_member("codex-win", "小码").unwrap();
        assert_eq!(store.seat_name(&room, &seat), "小码 2号");
        assert_eq!(store.seat_name(&room, "codex-win"), "小码 1号", "开了多个会话时平行编号，不分主次");
        assert_eq!(store.seat_name(&room, "hermes-win"), "Hermes", "只有一个会话就用成员名");
        // 第一个会话也能起项目内的名字，只在本项目生效，成员的全局名字不变。
        assert_eq!(store.rename_project_seat(&room, "codex-win", "主力").unwrap(), "主力");
        assert_eq!(store.member_name("codex-win"), "小码");
        let chats = store.project_agent_chats(&room).unwrap();
        assert_eq!(chats.iter().find(|chat| chat.agent_id == "codex-win").unwrap().label.as_deref(), Some("主力"));
        assert!(store.rename_project_seat(&room, "dsh-win", "外人").is_err(), "不在群里的成员不能起名");
        store.rename_member("codex-win", "").unwrap();
        assert_eq!(store.member_name("codex-win"), "Codex", "清空成员名恢复默认");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn project_members_can_change_before_work_starts() {
        let (mut store, room, base) = room();
        store.set_project_coordinator(&room, &crate::project_store::RoleChoice { agent: "codex-win".into(), model: None, effort: None }).unwrap();
        store.add_project_seat(&room, "hermes-win", None).unwrap();
        // 加入 Claude：自动有了自己的会话；调度管家 Codex 还在，配置保留。
        store.replace_members(&room, &["codex-win".into(), "hermes-win".into(), "claude-win".into()]).unwrap();
        let ids = |store: &mut crate::store::Store| store.ensure_project_agent_chats(&room).unwrap().into_iter().map(|chat| chat.agent_id).collect::<Vec<_>>();
        assert!(ids(&mut store).contains(&"claude-win".to_string()));
        assert_eq!(store.project_coordinator(&room).unwrap().unwrap().agent, "codex-win");
        // 移出 Hermes：它和它多开的会话从项目里消失；加回来就恢复。
        store.replace_members(&room, &["codex-win".into(), "claude-win".into()]).unwrap();
        assert!(ids(&mut store).iter().all(|id| !id.starts_with("hermes-win")));
        store.replace_members(&room, &["codex-win".into(), "claude-win".into(), "hermes-win".into()]).unwrap();
        assert!(ids(&mut store).contains(&"hermes-win-2".to_string()));
        // 移出调度管家：配置回到默认。
        store.replace_members(&room, &["claude-win".into(), "hermes-win".into()]).unwrap();
        assert!(store.project_coordinator(&room).unwrap().is_none());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn a_single_member_project_collaborates_through_its_own_sessions() {
        let base = std::env::temp_dir().join(format!("tongxi-solo-{}", Uuid::new_v4()));
        let root = base.join("project");
        std::fs::create_dir_all(&root).unwrap();
        let mut store = Store::open(&base.join("data").join("hub.db")).unwrap();
        let room = store.create("只有 Claude", "group", &["claude-win".into()]).unwrap().id;
        let project = store.register_project("单人项目", root.to_str().unwrap()).unwrap().id;
        store.bind_project(&room, Some(&project)).unwrap();
        store.add_project_seat(&room, "claude-win", Some("审稿")).unwrap();
        let chats = store.ensure_project_agent_chats(&room).unwrap();
        let names: Vec<_> = chats.iter().map(|chat| chat.name.as_str()).collect();
        assert_eq!(names, ["主脑", "Claude 1号", "审稿"], "主脑排第一；两个会话就是两位成员");
        let _ = std::fs::remove_dir_all(base);
    }
}
