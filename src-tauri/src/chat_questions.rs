//! Private clarification questions: native requests or a small reply envelope.
use crate::store::{now, RunRecord, Store};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;
use uuid::Uuid;

type Result<T> = std::result::Result<T, String>;
pub const OPEN: &str = "```tongxi-question";
pub const GUIDANCE: &str = "同席支持向用户提问。仅在确实缺少用户信息或需要用户作关键取舍时使用：优先调用 hub_ask_user（或对应 MCP 工具），给出 question 和 options；工具不可用时，回复末尾输出一个 tongxi-question 代码块，内容为 JSON {\"question\":\"具体问题\",\"options\":[\"选项一\",\"选项二\"]}。选项可以为空。不需要原生终端选择器。提问后结束当前回复，等待用户实际回答，不猜答案、不擅自采用默认选项。普通工作自行推进，不为每个工具操作提问。问题与答案仅本私聊可见，除非用户明确要求分享，不广播也不写入项目共享记忆。";

#[derive(Clone, Serialize)]
pub struct Question {
    pub id: String,
    pub conversation_id: String,
    pub run_id: String,
    pub message_id: String,
    pub agent_id: String,
    pub prompt: String,
    pub options: Vec<String>,
    pub status: String,
    pub mode: String,
    pub answer: Option<String>,
    pub answer_message_id: Option<String>,
    pub delivery: String,
    pub error: Option<String>,
}
#[derive(Serialize)]
pub struct Pending {
    pub id: String,
    pub conversation_id: String,
    pub agent_id: String,
    pub group_id: Option<String>,
    pub kind: String,
}
#[derive(Deserialize)]
pub struct Input {
    pub question: String,
    #[serde(default)]
    pub options: Vec<String>,
}

pub fn migrate(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute_batch("CREATE TABLE IF NOT EXISTS chat_questions (
      id TEXT PRIMARY KEY,conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
      run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,message_id TEXT NOT NULL,
      agent_id TEXT NOT NULL,prompt TEXT NOT NULL,options TEXT NOT NULL,status TEXT NOT NULL,
      mode TEXT NOT NULL,answer TEXT,answer_message_id TEXT,delivery TEXT NOT NULL DEFAULT 'awaiting',
      error TEXT,created_at INTEGER NOT NULL,answered_at INTEGER,UNIQUE(run_id,prompt));
      CREATE INDEX IF NOT EXISTS chat_question_pending ON chat_questions(conversation_id,status);")
        .map_err(|_| "私聊问题结构迁移失败".into())
}

fn validate(input: &Input) -> Result<()> {
    if input.question.trim().is_empty()
        || input.question.chars().count() > 4000
        || input.options.len() > 8
        || input
            .options
            .iter()
            .any(|option| option.trim().is_empty() || option.chars().count() > 500)
    {
        return Err("问题或选项无效".into());
    }
    Ok(())
}

pub fn post(connection: &Connection, run: &str, input: &Input, mode: &str) -> Result<String> {
    validate(input)?;
    let tx =
        rusqlite::Transaction::new_unchecked(connection, rusqlite::TransactionBehavior::Immediate)
            .map_err(|_| "问题数据库忙")?;
    let (room,agent,message):(String,String,String)=tx.query_row("SELECT r.conversation_id,r.agent_id,r.assistant_message_id FROM runs r JOIN conversations c ON c.id=r.conversation_id WHERE r.id=?1 AND r.discussion_id IS NULL AND c.kind='direct' AND c.archived=0 AND (r.status IN ('starting','running') OR (?2='deferred' AND r.status IN ('completed','succeeded')))",params![run,mode],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional().map_err(|_|"问题归属读取失败")?.ok_or("当前私聊任务不可提问")?;
    if !matches!(
        agent.as_str(),
        "codex-win" | "hermes-win" | "dsh-win" | "claude-win"
    ) {
        return Err("此成员尚未接入私聊问答".into());
    }
    let id = Uuid::new_v4().to_string();
    tx.execute("INSERT OR IGNORE INTO chat_questions(id,conversation_id,run_id,message_id,agent_id,prompt,options,status,mode,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,'pending',?8,?9)",params![id,room,run,message,agent,input.question.trim(),serde_json::to_string(&input.options).unwrap(),mode,now()]).map_err(|_|"私聊问题保存失败")?;
    let id = tx
        .query_row(
            "SELECT id FROM chat_questions WHERE run_id=?1 AND prompt=?2",
            params![run, input.question.trim()],
            |row| row.get(0),
        )
        .map_err(|_| "私聊问题读取失败")?;
    tx.commit().map_err(|_| "私聊问题保存失败")?;
    Ok(id)
}

impl Store {
    pub fn chat_questions(&self, room: &str) -> Result<Vec<Question>> {
        let mut query=self.connection.prepare("SELECT id,conversation_id,run_id,message_id,agent_id,prompt,options,status,mode,answer,answer_message_id,delivery,error FROM chat_questions WHERE conversation_id=?1 ORDER BY created_at,rowid").map_err(|_|"私聊问题读取失败")?;
        let rows = query
            .query_map([room], |row| {
                Ok(Question {
                    id: row.get(0)?,
                    conversation_id: row.get(1)?,
                    run_id: row.get(2)?,
                    message_id: row.get(3)?,
                    agent_id: row.get(4)?,
                    prompt: row.get(5)?,
                    options: serde_json::from_str(&row.get::<_, String>(6)?).unwrap_or_default(),
                    status: row.get(7)?,
                    mode: row.get(8)?,
                    answer: row.get(9)?,
                    answer_message_id: row.get(10)?,
                    delivery: row.get(11)?,
                    error: row.get(12)?,
                })
            })
            .map_err(|_| "私聊问题读取失败")?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| "私聊问题读取失败".into());
        rows
    }
    pub fn pending_chat_questions(&self) -> Result<Vec<Pending>> {
        let mut query=self.connection.prepare("SELECT q.id,q.conversation_id,q.agent_id,p.group_conversation_id,'chat' FROM chat_questions q LEFT JOIN project_agent_chats p ON p.conversation_id=q.conversation_id WHERE q.status='pending'").map_err(|_|"待答提示读取失败")?;
        let rows = query
            .query_map([], |row| {
                Ok(Pending {
                    id: row.get(0)?,
                    conversation_id: row.get(1)?,
                    agent_id: row.get(2)?,
                    group_id: row.get(3)?,
                    kind: row.get(4)?,
                })
            })
            .map_err(|_| "待答提示读取失败")?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| "待答提示读取失败".into());
        rows
    }
    pub fn answer_chat_question(&mut self, room: &str, id: &str, answer: &str) -> Result<Question> {
        let answer = answer.trim();
        if answer.is_empty() || answer.chars().count() > 16000 {
            return Err("请输入1—16000字符的回答".into());
        }
        let question = self
            .chat_questions(room)?
            .into_iter()
            .find(|q| q.id == id && q.status == "pending")
            .ok_or("问题已回答或已取消")?;
        if question.mode == "native" {
            let active: bool = self
                .connection
                .query_row(
                    "SELECT status IN ('starting','running') FROM runs WHERE id=?1",
                    [&question.run_id],
                    |row| row.get(0),
                )
                .map_err(|_| "原生问题状态不可用")?;
            if !active {
                return Err("原生任务已经停止，不能提交旧问题的回答".into());
            }
        }
        let message = Uuid::new_v4().to_string();
        let tx = self.connection.transaction().map_err(|_| "回答保存失败")?;
        tx.execute("UPDATE chat_questions SET status='answered',answer=?1,answer_message_id=?2,delivery=?3,error=NULL,answered_at=?4 WHERE id=?5 AND status='pending'",params![answer,message,if question.mode=="native"{"sent"}else{"queued"},now(),id]).map_err(|_|"回答保存失败")?;
        tx.commit().map_err(|_| "回答保存失败")?;
        self.chat_questions(room)?
            .into_iter()
            .find(|q| q.id == id)
            .ok_or("问题读取失败".into())
    }
    pub fn cancel_chat_question(&mut self, room: &str, id: &str) -> Result<()> {
        self.connection.execute("UPDATE chat_questions SET status='cancelled',delivery='cancelled' WHERE conversation_id=?1 AND id=?2 AND (status='pending' OR delivery='queued')",params![room,id]).map_err(|_|"问题取消失败")?;
        Ok(())
    }
    pub fn capture_chat_question(&self, record: &RunRecord, content: &str) -> Result<()> {
        if record.discussion_id.is_some()
            || !matches!(
                record.agent_id.as_str(),
                "codex-win" | "hermes-win" | "dsh-win" | "claude-win"
            )
        {
            return Ok(());
        }
        if matches!(record.status.as_str(), "completed" | "succeeded") {
            if let Some(start) = content.find(OPEN) {
                let rest = content[start + OPEN.len()..].trim_start();
                if let Some(end) = rest.find("```") {
                    if let Ok(input) = serde_json::from_str::<Input>(rest[..end].trim()) {
                        post(&self.connection, &record.id, &input, "deferred")?;
                    }
                }
            }
            self.connection.execute("UPDATE chat_questions SET status='cancelled' WHERE run_id=?1 AND mode='native' AND status='pending'",[&record.id]).map_err(|_|"问题状态保存失败")?;
        } else if matches!(
            record.status.as_str(),
            "failed" | "interrupted" | "cancelling" | "cancelled"
        ) {
            self.connection.execute("UPDATE chat_questions SET status='cancelled',delivery='cancelled' WHERE run_id=?1 AND (status='pending' OR delivery='queued')",[&record.id]).map_err(|_|"问题状态保存失败")?;
        }
        Ok(())
    }
}

pub fn tool(db: &Path, room: &str, args: Value) -> Result<Value> {
    let input: Input = serde_json::from_value(args).map_err(|_| "提问参数无效")?;
    let connection = Connection::open(db).map_err(|_| "问题数据库不可用")?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(|_| "问题数据库不可用")?;
    let run:String=connection.query_row("SELECT id FROM runs WHERE conversation_id=?1 AND discussion_id IS NULL AND status IN ('starting','running') ORDER BY rowid DESC LIMIT 1",[room],|row|row.get(0)).map_err(|_|"当前没有可提问的私聊任务")?;
    let id = post(&connection, &run, &input, "deferred")?;
    Ok(
        json!({"question_id":id,"awaiting_user":true,"instruction":"问题已显示在本私聊。结束当前回复，等待用户实际回答，不再继续工具操作。"}),
    )
}

pub fn ask_native(db: &Path, run: &str, params: &Value) -> Result<Value> {
    let connection = Connection::open(db).map_err(|_| "问题数据库不可用")?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(|_| "问题数据库不可用")?;
    let mut answers = serde_json::Map::new();
    for item in params["questions"].as_array().ok_or("原生问题无效")? {
        let input = Input {
            question: item["question"].as_str().ok_or("问题为空")?.into(),
            options: item["options"]
                .as_array()
                .map(|options| {
                    options
                        .iter()
                        .filter_map(|option| option["label"].as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
        };
        let id = post(&connection, run, &input, "native")?;
        loop {
            let (status, answer): (String, Option<String>) = connection
                .query_row(
                    "SELECT status,answer FROM chat_questions WHERE id=?1",
                    [&id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(|_| "问题已删除")?;
            if status == "answered" {
                answers.insert(
                    item["id"].as_str().ok_or("原生问题标识为空")?.into(),
                    json!({"answers":[answer.unwrap_or_default()]}),
                );
                break;
            }
            let active: bool = connection
                .query_row(
                    "SELECT status IN ('starting','running') FROM runs WHERE id=?1",
                    [run],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            if status == "cancelled" || !active {
                return Err("问题等待已取消".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    Ok(json!({"answers":answers}))
}
