use crate::{project_store, store::Store};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;

#[derive(Serialize)]
pub struct RemovedItem {
    pub workflow_id: Option<String>,
}

impl Store {
    pub fn remove_conversation_item(
        &mut self,
        room: &str,
        kind: &str,
        id: &str,
    ) -> Result<RemovedItem, String> {
        self.conversation(room)?;
        let mut workflow_id = None;
        let mut user_message = None;
        match kind {
            "workflow" => {
                workflow_id = Some(id.to_owned());
            }
            "message" => {
                let exists: bool = self
                    .connection
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM messages WHERE id=?1 AND conversation_id=?2)",
                        params![id, room],
                        |row| row.get(0),
                    )
                    .map_err(|_| "消息读取失败")?;
                if !exists {
                    return Err("消息不属于此会话".into());
                }
                let busy: bool = self.connection.query_row("SELECT EXISTS(SELECT 1 FROM runs WHERE (user_message_id=?1 OR assistant_message_id=?1) AND status IN ('starting','running','cancelling'))", [id], |row|row.get(0)).map_err(|_|"消息运行状态读取失败")?;
                if busy {
                    return Err("请先停止本条消息的运行再删除".into());
                }
                workflow_id = self
                    .connection
                    .query_row(
                        "SELECT id FROM workflows WHERE user_message_id=?1",
                        [id],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(|_| "协作归属读取失败")?;
                let active_discussion: bool = self.connection.query_row("SELECT EXISTS(SELECT 1 FROM discussions WHERE user_message_id=?1 AND status IN ('running','cancelling'))", [id], |row|row.get(0)).map_err(|_|"群聊状态读取失败")?;
                if active_discussion {
                    return Err("请先停止本轮讨论再删除".into());
                }
            }
            "peer_message" => {
                let owner: Option<(String,String)> = self.connection.query_row("SELECT w.id,w.status FROM project_agent_messages m JOIN workflows w ON w.id=m.workflow_id WHERE m.id=?1 AND w.conversation_id=?2", params![id,room], |row|Ok((row.get(0)?,row.get(1)?))).optional().map_err(|_|"成员消息归属读取失败")?;
                if let Some((_, status)) = owner {
                    if project_store::live(&status) {
                        return Err("请先停止本轮协作再删除成员消息".into());
                    }
                } else {
                    let belongs:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM project_team_messages WHERE id=?1 AND conversation_id=?2 AND recipient_agent IS NOT NULL)",params![id,room],|row|row.get(0)).map_err(|_|"成员消息读取失败")?;
                    if !belongs {
                        return Err("成员消息不属于此会话".into());
                    }
                }
            }
            "team_message" => {
                let belongs: bool = self.connection.query_row("SELECT EXISTS(SELECT 1 FROM project_team_messages WHERE id=?1 AND conversation_id=?2)", params![id,room], |row|row.get(0)).map_err(|_|"团队消息读取失败")?;
                if !belongs {
                    return Err("团队消息不属于此会话".into());
                }
            }
            _ => return Err("不支持删除这种会话内容".into()),
        }
        if let Some(workflow) = &workflow_id {
            let job = self.workflow(workflow)?;
            if job.conversation_id != room {
                return Err("协作不属于此会话".into());
            }
            if project_store::live(&job.status) {
                return Err("请先停止这轮协作再删除".into());
            }
            user_message = Some(job.user_message_id);
        }
        let tx = self
            .connection
            .transaction()
            .map_err(|_| "删除事务创建失败")?;
        if let Some(workflow) = &workflow_id {
            tx.execute("DELETE FROM project_sessions WHERE project_id=(SELECT project_id FROM workflows WHERE id=?1) AND native_session_id IN (SELECT native_thread_id FROM project_attempts WHERE workflow_id=?1)", [workflow]).map_err(|_|"协作会话映射删除失败")?;
            tx.execute("DELETE FROM workflows WHERE id=?1", [workflow])
                .map_err(|_| "协作记录删除失败")?;
            tx.execute("DELETE FROM messages WHERE id=?1", [user_message.unwrap()])
                .map_err(|_| "协作消息删除失败")?;
        } else if kind == "peer_message" {
            tx.execute("DELETE FROM project_agent_messages WHERE id=?1", [id])
                .map_err(|_| "成员消息删除失败")?;
            tx.execute("DELETE FROM project_team_messages WHERE id=?1", [id])
                .map_err(|_| "成员消息删除失败")?;
        } else if kind == "team_message" {
            tx.execute("DELETE FROM project_team_messages WHERE id=?1", [id])
                .map_err(|_| "团队消息删除失败")?;
        } else {
            tx.execute("DELETE FROM messages WHERE id=?1", [id])
                .map_err(|_| "消息删除失败")?;
        }
        tx.execute(
            "UPDATE conversations SET updated_at=?1 WHERE id=?2",
            params![crate::store::now(), room],
        )
        .map_err(|_| "会话更新时间失败")?;
        tx.commit().map_err(|_| "删除未保存")?;
        Ok(RemovedItem { workflow_id })
    }
}
