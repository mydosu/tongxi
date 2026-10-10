use crate::store::{now, Store};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;
use uuid::Uuid;

#[derive(Clone, Serialize)]
pub struct Question {
    pub id: String,
    pub workflow_id: String,
    pub attempt_id: Option<String>,
    pub delivery: String,
    pub conversation_id: String,
    pub agent_id: String,
    pub prompt: String,
    pub options: Vec<String>,
    pub status: String,
    pub answer: Option<String>,
}

impl Store {
    pub fn project_questions(&self, room: &str) -> Result<Vec<Question>, String> {
        let mut query=self.connection.prepare("SELECT id,workflow_id,conversation_id,agent_id,prompt,options,status,answer,attempt_id,delivery FROM project_questions WHERE conversation_id=?1 ORDER BY created_at,id").map_err(|_|"项目问题读取失败")?;
        let result = query
            .query_map([room], |row| {
                Ok(Question {
                    id: row.get(0)?,
                    workflow_id: row.get(1)?,
                    conversation_id: row.get(2)?,
                    agent_id: row.get(3)?,
                    prompt: row.get(4)?,
                    options: serde_json::from_str(&row.get::<_, String>(5)?).unwrap_or_default(),
                    status: row.get(6)?,
                    answer: row.get(7)?,
                    attempt_id: row.get(8)?,
                    delivery: row.get(9)?,
                })
            })
            .map_err(|_| "项目问题读取失败")?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "项目问题读取失败".into());
        result
    }
    pub fn answer_project_question(
        &mut self,
        room: &str,
        id: &str,
        message: &str,
        content: &str,
    ) -> Result<(), String> {
        let content = content.trim();
        if content.is_empty()
            || content.chars().count() > 16_000
            || Uuid::parse_str(message).is_err()
        {
            return Err("回答需为 1–16000 个字符，且消息标识有效".into());
        }
        let question = self
            .project_questions(room)?
            .into_iter()
            .find(|q| q.id == id && q.status == "pending")
            .ok_or("问题已回答或已取消")?;
        let job = self.workflow(&question.workflow_id)?;
        if !crate::project_store::live(&job.status) {
            return Err("这轮协作已停止，问题不会再提交给旧任务".into());
        }
        let roles = crate::project_directives::encoded_roles(self, &job.id, content)?;
        // Ordinary save_message rejects new work during a live workflow; answers
        // are allowed here and must be stored atomically with the question state.
        let tx = self.connection.transaction().map_err(|_| "回答保存失败")?;
        let timestamp = now();
        tx.execute("INSERT INTO messages(id,conversation_id,sender_id,content,status,created_at) VALUES(?1,?2,'user',?3,'delivered',?4)",params![message,room,content,timestamp]).map_err(|_|"回答保存失败")?;
        if tx.execute("UPDATE project_questions SET status='answered',answer=?1,answered_at=?3 WHERE id=?2 AND status='pending'",params![content,id,timestamp]).map_err(|_|"回答保存失败")? != 1 {
            return Err("问题已回答或已取消".into());
        }
        tx.execute(
            "UPDATE conversations SET updated_at=?1 WHERE id=?2",
            params![timestamp, room],
        )
        .map_err(|_| "回答保存失败")?;
        crate::project_directives::save_roles(&tx, &job.id, &roles, timestamp)?;
        tx.commit().map_err(|_| "回答保存失败")?;
        Ok(())
    }
    pub fn project_wait_duration(&self, workflow: &str) -> Duration {
        let Ok(mut query)=self.connection.prepare("SELECT created_at,COALESCE(answered_at,?2) FROM project_questions WHERE workflow_id=?1 ORDER BY created_at") else {return Duration::ZERO;};
        let Ok(rows) = query.query_map(params![workflow, now()], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        }) else {
            return Duration::ZERO;
        };
        let mut end = 0;
        let mut total = 0;
        for (start, stop) in rows.flatten() {
            if stop > start.max(end) {
                total += stop - start.max(end);
            }
            end = end.max(stop);
        }
        Duration::from_millis(total.max(0) as u64)
    }
}

pub fn post(
    db: &Path,
    attempt: &str,
    prompt: &str,
    options: Vec<String>,
) -> Result<String, String> {
    if prompt.trim().is_empty() || options.len() > 8 {
        return Err("提问参数无效".into());
    }
    let mut connection = Connection::open(db).map_err(|_| "问题数据库不可用")?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(|_| "问题数据库不可用")?;
    let tx = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|_| "问题数据库不可用")?;
    let (workflow,room,agent,project):(String,String,String,String)=tx.query_row("SELECT w.id,w.conversation_id,a.agent_id,w.project_id FROM project_attempts a JOIN workflows w ON w.id=a.workflow_id WHERE a.id=?1 AND a.status IN ('starting','running') AND w.status IN ('planning','running','reviewing')",[attempt],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).optional().map_err(|_|"提问归属读取失败")?.ok_or("任务已停止")?;
    let id = Uuid::new_v4().to_string();
    tx.execute("INSERT INTO project_questions(id,workflow_id,conversation_id,agent_id,prompt,options,status,created_at,attempt_id) VALUES(?1,?2,?3,?4,?5,?6,'pending',?7,?8)",params![id,workflow,room,agent,prompt,serde_json::to_string(&options).unwrap(),now(),attempt]).map_err(|_|"提问保存失败")?;
    tx.execute("INSERT INTO project_team_messages(id,project_id,conversation_id,workflow_id,attempt_id,sender_agent,message_kind,content,created_at) VALUES(?1,?2,?3,?4,?5,?6,'agent',?7,?8)",params![id,project,room,workflow,attempt,agent,prompt,now()]).map_err(|_|"问题发布失败")?;
    tx.commit().map_err(|_| "问题发布失败")?;
    Ok(id)
}

pub fn ask(db: &Path, attempt: &str, prompt: &str, options: Vec<String>) -> Result<String, String> {
    let id = post(db, attempt, prompt, options)?;
    let connection = Connection::open(db).map_err(|_| "问题数据库不可用")?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(|_| "问题数据库不可用")?;
    loop {
        let state: Option<(String, Option<String>)> = connection
            .query_row(
                "SELECT status,answer FROM project_questions WHERE id=?1",
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|_| "问题状态读取失败")?;
        match state {
            Some((status, answer)) if status == "answered" => {
                connection
                    .execute(
                        "UPDATE project_questions SET delivery='native' WHERE id=?1",
                        [&id],
                    )
                    .map_err(|_| "回答状态保存失败")?;
                return Ok(answer.unwrap_or_default());
            }
            Some((status, _)) if status == "cancelled" => {
                return Err("用户已停止此任务，问题等待已取消".into())
            }
            None => return Err("问题记录已删除".into()),
            _ => {}
        }
        let running:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM project_attempts a JOIN workflows w ON w.id=a.workflow_id WHERE a.id=?1 AND a.status IN ('starting','running') AND w.status IN ('planning','running','reviewing'))",[attempt],|row|row.get(0)).unwrap_or(false);
        if !running {
            return Err("用户已停止此任务，问题等待已取消".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub fn ask_native(db: &Path, attempt: &str, params: &Value) -> Result<Value, String> {
    let mut answers = serde_json::Map::new();
    for question in params["questions"].as_array().ok_or("原生提问参数无效")? {
        let id = question["id"].as_str().ok_or("问题缺少标识")?;
        let options = question["options"]
            .as_array()
            .map(|options| {
                options
                    .iter()
                    .filter_map(|option| option["label"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let answer = ask(
            db,
            attempt,
            question["question"].as_str().ok_or("问题缺少内容")?,
            options,
        )?;
        answers.insert(id.into(), json!({"answers":[answer]}));
    }
    Ok(json!({"answers":answers}))
}
