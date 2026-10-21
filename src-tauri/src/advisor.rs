//! 请教模式：成员干活时遇到拿不准的关键决定，用项目工具 hub_ask_advisor 向调度请教。
//! 请求写进 advisor_requests 排队，主进程里盯这项任务的线程交给调度回答（复用调度已缓存的系统提示，
//! 一次只多几千 token），成员这边等回答回来再接着做，会话不中断、缓存不冷。次数由调度按任务给（advisor_calls）。
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, String>;

/// 成员最多等这么久；调度那边撞限额或在忙就按自己的判断继续。
const WAIT: Duration = Duration::from_secs(600);

pub fn ask(connection: &Connection, attempt_id: &str, args: Value) -> Result<Value> {
    let question = args["question"].as_str().map(str::trim).unwrap_or_default();
    if question.is_empty() || question.chars().count() > 4000 {
        return Err("写清要请教的问题（4000 字以内）".into());
    }
    let context = args["context"].as_str().unwrap_or_default();
    if context.chars().count() > 12_000 {
        return Err("附带的代码片段或报错太长，挑相关的部分（12000 字以内）".into());
    }
    let (workflow, task): (String, Option<String>) = connection
        .query_row("SELECT workflow_id,task_id FROM project_attempts WHERE id=?1", [attempt_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(|_| "当前任务不可用")?;
    let task = task.ok_or("只有调度派的任务能请教")?;
    let allowed: i64 = connection
        .query_row("SELECT COALESCE(advisor_calls,0) FROM coordination_routes WHERE task_id=?1", [&task], |r| r.get(0))
        .optional()
        .map_err(|_| "请教设置读取失败")?
        .unwrap_or(0);
    let used: i64 = connection
        .query_row("SELECT COUNT(*) FROM advisor_requests WHERE task_id=?1 AND status='answered'", [&task], |r| r.get(0))
        .map_err(|_| "请教记录读取失败")?;
    if allowed <= 0 {
        return Err("调度没有给这项任务开放请教：按自己的判断做；确实做不了就在交付摘要里单独一行写「需要升档：卡在哪、试过什么」".into());
    }
    if used >= allowed {
        return Err(format!("这项任务的请教次数已用完（{allowed} 次）：按已有的回答和自己的判断继续"));
    }
    let id = uuid::Uuid::new_v4().to_string();
    connection
        .execute(
            "INSERT INTO advisor_requests(id,workflow_id,task_id,attempt_id,question,context,status,created_at) VALUES(?1,?2,?3,?4,?5,?6,'pending',?7)",
            params![id, workflow, task, attempt_id, question, context, crate::store::now()],
        )
        .map_err(|_| "请教请求保存失败")?;
    let started = Instant::now();
    while started.elapsed() < WAIT {
        std::thread::sleep(Duration::from_secs(1));
        let row: Option<(String, Option<String>)> = connection
            .query_row("SELECT status,answer FROM advisor_requests WHERE id=?1", [&id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()
            .map_err(|_| "请教状态读取失败")?;
        match row {
            Some((status, answer)) if status == "answered" => {
                return Ok(json!({"answer":answer.unwrap_or_default(),"remaining_calls":allowed - used - 1}));
            }
            Some((status, answer)) if status == "failed" => {
                return Err(answer.unwrap_or_else(|| "顾问没能回答，按自己的判断继续".into()));
            }
            None => return Err("请教请求已被撤下".into()),
            _ => {}
        }
    }
    let _ = connection.execute("UPDATE advisor_requests SET status='expired' WHERE id=?1 AND status='pending'", [&id]);
    Err("顾问暂时没有回复：按自己的判断继续，必要时在交付里写明拿不准的地方".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database() -> (Connection, String) {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE project_attempts(id TEXT,workflow_id TEXT,task_id TEXT);
                 CREATE TABLE coordination_routes(task_id TEXT,advisor_calls INTEGER);
                 CREATE TABLE advisor_requests(id TEXT PRIMARY KEY,workflow_id TEXT,task_id TEXT,attempt_id TEXT,question TEXT,context TEXT,status TEXT,answer TEXT,created_at INTEGER,answered_at INTEGER);
                 INSERT INTO project_attempts VALUES('a','w','t');",
            )
            .unwrap();
        (connection, "a".into())
    }

    #[test]
    fn asking_needs_calls_granted_by_the_coordinator() {
        let (connection, attempt) = database();
        connection.execute("INSERT INTO coordination_routes VALUES('t',0)", []).unwrap();
        let error = ask(&connection, &attempt, json!({"question":"用锁还是通道？"})).unwrap_err();
        assert!(error.contains("没有给这项任务开放请教") && error.contains("需要升档"));
        connection.execute("UPDATE coordination_routes SET advisor_calls=1", []).unwrap();
        connection.execute("INSERT INTO advisor_requests(id,task_id,status) VALUES('old','t','answered')", []).unwrap();
        assert!(ask(&connection, &attempt, json!({"question":"再问一次"})).unwrap_err().contains("已用完"));
        assert!(ask(&connection, &attempt, json!({"question":"  "})).is_err());
    }
}
