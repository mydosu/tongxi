use crate::store::Store;
use rusqlite::params;
use serde::Serialize;

#[derive(Serialize)]
pub struct RemovedItem {}

impl Store {
    pub fn remove_conversation_item(
        &mut self,
        room: &str,
        kind: &str,
        id: &str,
    ) -> Result<RemovedItem, String> {
        self.conversation(room)?;
        match kind {
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
                let active_discussion: bool = self.connection.query_row("SELECT EXISTS(SELECT 1 FROM discussions WHERE user_message_id=?1 AND status IN ('running','cancelling'))", [id], |row|row.get(0)).map_err(|_|"群聊状态读取失败")?;
                if active_discussion {
                    return Err("请先停止本轮讨论再删除".into());
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
        let tx = self
            .connection
            .transaction()
            .map_err(|_| "删除事务创建失败")?;
        if kind == "team_message" {
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
        Ok(RemovedItem {})
    }
}
