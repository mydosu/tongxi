//! 项目会话和私聊里给成员用的 MCP 工具：问用户、项目记忆、发群消息；主脑会话换成主脑那套（见 project_brain）。
use crate::project_store::base_agent;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::Read;
use std::path::Path;
use std::time::Duration;
use uuid::Uuid;

type Result<T> = std::result::Result<T, String>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BroadcastArg {
    content: String,
}

pub fn tool_specs() -> Vec<Value> {
    let string = json!({"type":"string"});
    let definitions=[
        ("hub_ask_user","Ask the user a clarification question and wait for their reply. The question is shown in this conversation; the same native turn continues after the answer.",json!({"question":string,"options":{"type":"array","items":string}}),vec!["question"]),
        ("hub_memory_search","Search this project's memory: the shared team pool plus your own private notes (scope: all | shared | private, default all). Use before work to recall decisions and known pitfalls.",json!({"query":string,"scope":{"type":"string","enum":["all","shared","private"]}}),vec!["query"]),
        ("hub_memory_write","Save a concise note. scope=private (default) keeps it to yourself; scope=shared publishes it to every member of this project. Shared notes can only be updated by their author. Do not store private chats, raw reasoning or credentials.",json!({"name":string,"content":string,"scope":{"type":"string","enum":["private","shared"]}}),vec!["name","content"]),
        ("hub_memory_read","Read a note URI from this project's shared pool or your own private notes.",json!({"uri":string}),vec!["uri"]),
        ("hub_memory_list","List note URIs in this project's shared pool and/or your private notes (scope: all | shared | private).",json!({"scope":{"type":"string","enum":["all","shared","private"]}}),vec![]),
        ("hub_memory_share","Choose one of your private notes to share with the whole project team. The private original is kept; the shared copy records you as author and its source. Optional name renames the shared copy; optional note adds a short preface.",json!({"uri":string,"name":string,"note":string}),vec!["uri"]),
        ("hub_broadcast","Post a short message for the user and all agents in the shared project room. Only publish information the team should see; keep reasoning and tool details in your own agent session.",json!({"content":string}),vec!["content"]),
    ];
    definitions.into_iter().map(|(name,description,properties,required)|json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false}})).collect()
}

pub fn project_chat_mcp_server(db: &Path, conversation_id: &str) -> Result<Option<Value>> {
    let connection = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "项目 Agent 会话数据库不可用")?;
    let linked: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM project_agent_chats WHERE conversation_id=?1)",
            [conversation_id],
            |row| row.get(0),
        )
        .map_err(|_| "项目 Agent 会话映射不可用")?;
    if !linked {
        let direct:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM conversations WHERE id=?1 AND kind='direct' AND archived=0)",[conversation_id],|row|row.get(0)).map_err(|_|"私聊工具归属不可用")?;
        if !direct {
            return Ok(None);
        }
    }
    let command = std::env::current_exe().map_err(|_| "项目消息工具程序不可用")?;
    Ok(Some(
        json!({"name":"agent_hub_project","command":command,"args":["--project-chat-tools",db.to_string_lossy(),conversation_id],"env":[]}),
    ))
}

pub fn project_chat_dynamic_tools(db: &Path, conversation_id: &str) -> Result<Vec<Value>> {
    let server = project_chat_mcp_server(db, conversation_id)?;
    if server.is_none() {
        return Ok(Vec::new());
    }
    let connection = Connection::open(db).map_err(|_| "私聊工具目录不可用")?;
    let linked: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM project_agent_chats WHERE conversation_id=?1)",
            [conversation_id],
            |row| row.get(0),
        )
        .map_err(|_| "私聊工具目录不可用")?;
    Ok(if linked {
        project_chat_tool_specs(&connection, conversation_id)
    } else {
        tool_specs()
            .into_iter()
            .filter(|tool| tool["name"] == "hub_ask_user")
            .collect()
    }
    .into_iter()
    .map(|mut tool| {
        tool["type"] = json!("function");
        tool["deferLoading"] = json!(false);
        tool
    })
    .collect())
}

/// 主脑会话用主脑那套工具；成员会话只有问用户、记忆和发群消息，活由主脑派。
fn project_chat_tool_specs(connection: &Connection, conversation_id: &str) -> Vec<Value> {
    if crate::project_brain::is_brain_chat(connection, conversation_id) {
        return crate::project_brain::tool_specs();
    }
    tool_specs()
}

fn chat_context(
    connection: &Connection,
    conversation_id: &str,
) -> Result<(String, String, String)> {
    let context: (String,String,String) = connection.query_row("SELECT project_id,group_conversation_id,agent_id FROM project_agent_chats WHERE conversation_id=?1", [conversation_id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional().map_err(|_| "项目会话归属读取失败")?.ok_or("这不是项目内 Agent 会话")?;
    // 调度新开的会话（claude-win-2 这类）在群成员表里记的是本体，按本体校验。
    let allowed: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM members m JOIN conversations c ON c.id=m.conversation_id JOIN conversation_projects p ON p.conversation_id=c.id WHERE m.conversation_id=?1 AND m.agent_id=?2 AND p.project_id=?3 AND c.archived=0)", params![context.1,crate::project_store::base_agent(&context.2),context.0], |row| row.get(0)).map_err(|_| "项目成员归属校验失败")?;
    if !allowed {
        return Err("该项目会话已归档、解除绑定或成员已退出".into());
    }
    Ok(context)
}

fn chat_tool(
    connection: &Connection,
    conversation_id: &str,
    name: &str,
    args: Value,
) -> Result<Value> {
    let linked: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM project_agent_chats WHERE conversation_id=?1)",
            [conversation_id],
            |row| row.get(0),
        )
        .map_err(|_| "私聊工具归属不可用")?;
    if linked && crate::project_brain::is_brain_chat(connection, conversation_id) {
        return crate::project_brain::call(connection, conversation_id, name, args);
    }
    if !linked || name == "hub_ask_user" {
        let private_run:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM runs WHERE conversation_id=?1 AND discussion_id IS NULL AND status IN ('starting','running'))",[conversation_id],|row|row.get(0)).map_err(|_|"私聊状态不可用")?;
        if !linked || private_run {
            if name != "hub_ask_user" {
                return Err("普通私聊不提供项目协作工具".into());
            }
            let path: String = connection
                .query_row(
                    "SELECT file FROM pragma_database_list WHERE name='main'",
                    [],
                    |row| row.get(0),
                )
                .map_err(|_| "私聊数据库路径不可用")?;
            return crate::chat_questions::tool(Path::new(&path), conversation_id, args);
        }
    }
    let (project, _, agent) = chat_context(connection, conversation_id)?;
    match name {
        "hub_memory_search" | "hub_memory_write" | "hub_memory_read" | "hub_memory_list"
        | "hub_memory_share" => crate::project_memory::call(&project, base_agent(&agent), name, args),
        "hub_broadcast" => {
            let arg: BroadcastArg = serde_json::from_value(args).map_err(|_| "团队消息参数无效")?;
            chat_broadcast(connection, conversation_id, &arg.content)
        }
        _ => Err("此项目会话不支持该协作工具".into()),
    }
}

pub fn call_from_project_chat(
    db: &Path,
    conversation_id: &str,
    name: &str,
    args: Value,
) -> Result<Value> {
    let connection = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "项目消息数据库不可用")?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(|_| "项目消息数据库不可用")?;
    chat_tool(&connection, conversation_id, name, args)
}


fn chat_broadcast(connection: &Connection, conversation_id: &str, content: &str) -> Result<Value> {
    let text = content.trim();
    if text.is_empty()
        || text.chars().count() > 4000
        || text
            .chars()
            .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
    {
        return Err("团队消息需为 1–4000 个有效字符".into());
    }
    let (project, group, agent) = chat_context(connection, conversation_id)?;
    let allowed: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM members WHERE conversation_id=?1 AND agent_id=?2)",
            params![group, base_agent(&agent)],
            |row| row.get(0),
        )
        .map_err(|_| "团队成员校验失败")?;
    if !allowed {
        return Err("该 Agent 已不属于项目团队".into());
    }
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM project_team_messages WHERE conversation_id=?1",
            [&group],
            |row| row.get(0),
        )
        .map_err(|_| "团队消息计数失败")?;
    if count >= 500 {
        return Err("团队消息已达到 500 条上限".into());
    }
    let id = Uuid::new_v4().to_string();
    connection.execute(
        "INSERT INTO project_team_messages(id,project_id,conversation_id,sender_agent,message_kind,content,created_at) VALUES(?1,?2,?3,?4,'agent',?5,?6)",
        params![id,project,group,agent,text,crate::store::now()],
    ).map_err(|_|"消息发布到团队失败")?;
    Ok(json!({"sent":true,"message_id":id,"audience":"project_group"}))
}

pub fn serve_project_chat(db: &Path, conversation_id: &str) -> Result<()> {
    use std::io::{BufRead, Write};
    if !db.is_absolute() || !db.is_file() || Uuid::parse_str(conversation_id).is_err() {
        return Err("项目 Agent 工具参数无效".into());
    }
    let connection = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "项目消息数据库不可用")?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(|_| "项目消息数据库不可用")?;
    let linked: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM project_agent_chats WHERE conversation_id=?1)",
            [conversation_id],
            |row| row.get(0),
        )
        .map_err(|_| "项目 Agent 会话映射不可用")?;
    if !linked {
        let direct:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM conversations WHERE id=?1 AND kind='direct' AND archived=0)",[conversation_id],|row|row.get(0)).map_err(|_|"私聊归属不可用")?;
        if !direct {
            return Err("私聊会话不存在".into());
        }
    }
    let input = std::io::stdin();
    let mut reader = input.lock();
    let output = std::io::stdout();
    let mut writer = output.lock();
    let mut initialized = false;
    loop {
        let mut line = String::new();
        let count = std::io::Read::by_ref(&mut reader)
            .take(64 * 1024)
            .read_line(&mut line)
            .map_err(|_| "项目消息工具输入中断")?;
        if count == 0 {
            break;
        }
        if !line.ends_with('\n') {
            return Err("项目消息工具请求过长或不完整".into());
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = value.get("id") else {
            continue;
        };
        let response = match value["method"].as_str().unwrap_or("") {
            "initialize" if !initialized => {
                initialized = true;
                json!({"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"agent-hub-project-chat","version":env!("CARGO_PKG_VERSION")}}})
            }
            "ping" => json!({"result":{}}),
            "tools/list" if initialized => {
                json!({"result":{"tools":if linked {project_chat_tool_specs(&connection, conversation_id)} else {tool_specs().into_iter().filter(|tool|tool["name"]=="hub_ask_user").collect::<Vec<_>>()}}})
            }
            "tools/call" if initialized => {
                match chat_tool(
                    &connection,
                    conversation_id,
                    value["params"]["name"].as_str().unwrap_or(""),
                    value["params"]["arguments"].clone(),
                ) {
                    Ok(result) => {
                        json!({"result":{"content":[{"type":"text","text":result.to_string()}],"isError":false}})
                    }
                    Err(error) => {
                        json!({"result":{"content":[{"type":"text","text":error}],"isError":true}})
                    }
                }
            }
            _ => json!({"error":{"code":-32601,"message":"Unsupported project chat tool request"}}),
        };
        let mut response = response;
        response["id"] = id.clone();
        response["jsonrpc"] = json!("2.0");
        serde_json::to_writer(&mut writer, &response).map_err(|_| "项目消息工具输出中断")?;
        writer
            .write_all(b"\n")
            .and_then(|_| writer.flush())
            .map_err(|_| "项目消息工具输出中断")?;
    }
    Ok(())
}
