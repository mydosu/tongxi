//! 项目往来：一个项目的主脑给另一个项目的主脑提需求，对方做完把结论和产出交回来。
//! 需求记在 project_requests；送进对方主脑走它的 brain_inbox（和成员交回同一条路，闲着发新消息、忙着插话），
//! 两边项目群各留一条记录。对方项目在项目设置里选：直接接、先问用户（挂到协作问答，用户点了「接」才送进主脑）或不接。
//! 交回的文件复制进发起方项目的 .tongxi/收件/，用户和发起方主脑都能直接点开。
use crate::project_brain::{short, team_message, text_arg, Context};
use crate::store::now;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use uuid::Uuid;

type Result<T> = std::result::Result<T, String>;

/// 同一件需求两边加起来最多来回这么多次（交回、撤销不算），再多就是在踢皮球，该问用户了。
const MAX_ROUNDS: i64 = 12;
/// 一个项目同时挂着、还没交回的需求最多这么多件。
const MAX_OPEN: i64 = 5;
/// 一次最多复制这么多字节、这么多个文件。
const COPY_LIMIT: u64 = 1 << 30;
const COPY_FILES: usize = 200;
/// 「其他项目的需求」怎么接：直接接、先问用户、不接。
pub const MODES: [&str; 3] = ["accept", "ask", "deny"];
/// 先问用户时挂到问答面板上的 key 前缀。
const ASK_KEY: &str = "xreq:";

pub fn migrate(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS project_requests (
           id TEXT PRIMARY KEY,
           from_room TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
           to_room TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
           content TEXT NOT NULL,status TEXT NOT NULL,rounds INTEGER NOT NULL DEFAULT 1,last TEXT,
           created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL);
         CREATE INDEX IF NOT EXISTS project_request_rooms ON project_requests(status,from_room,to_room);",
    )
    .map_err(|_| "项目往来结构迁移失败".into())
}

pub fn accept_mode(connection: &Connection, room: &str) -> &'static str {
    let stored: Option<String> = connection
        .query_row("SELECT value FROM metadata WHERE key=?1", [format!("project_accept:{room}")], |row| row.get(0))
        .optional()
        .ok()
        .flatten();
    MODES.into_iter().find(|mode| stored.as_deref() == Some(*mode)).unwrap_or("accept")
}

pub fn set_accept_mode(connection: &Connection, room: &str, mode: &str) -> Result<()> {
    if !MODES.contains(&mode) {
        return Err("只能选直接接、先问我或不接".into());
    }
    connection
        .execute(
            "INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![format!("project_accept:{room}"), mode],
        )
        .map_err(|_| "设置保存失败")?;
    Ok(())
}

pub fn tool_specs() -> Vec<(&'static str, &'static str, Value, Vec<&'static str>)> {
    let string = json!({"type":"string"});
    let files = json!({"type":"array","items":{"type":"string"}});
    vec![
        ("hub_ask_project", "Ask another project's brain to do something for this project (e.g. the promo project makes an image or copy your project needs). project: its project name or group title as listed in hub_team projects. content: what you need, concretely (what, format, size, by when, what to hand back), with any paths it needs. Returns a request id at once; the other brain works on it in its own project and its replies and final hand-back arrive later as new messages to you. files: absolute paths of materials to give it; they are copied into its project. Use it for work that belongs to the other project, not to offload your own work.", json!({"project":string,"content":string,"files":files}), vec!["project", "content"]),
        ("hub_reply_project", "Continue a cross-project request (hub_team requests lists the open ones, in and out). As the project doing the work: ask a clarifying question (content only), hand back the result (done=true, content: conclusion; files: absolute paths of what you made, copied into the requester's project), or turn it down (decline=true, content: why). As the requester: answer its question (content), or call it off (cancel=true). Each request allows a limited number of back-and-forth messages; beyond that ask the user with hub_ask_user.", json!({"request":string,"content":string,"done":{"type":"boolean"},"decline":{"type":"boolean"},"cancel":{"type":"boolean"},"files":files}), vec!["request", "content"]),
    ]
}

/// 项目群 → (项目名, 项目目录)。
fn project_of(connection: &Connection, room: &str) -> Result<(String, String)> {
    connection
        .query_row(
            "SELECT p.name,p.root FROM conversation_projects cp JOIN projects p ON p.id=cp.project_id WHERE cp.conversation_id=?1",
            [room],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|_| "项目读取失败".into())
}

/// 别的项目群：没归档、绑着项目、群里有能当主脑的成员。(群 id, 项目名, 群名, 目录)
fn other_projects(connection: &Connection, room: &str) -> Vec<(String, String, String, String)> {
    let brains = crate::project_brain::BRAIN_PREFERENCE.iter().map(|agent| format!("'{agent}'")).collect::<Vec<_>>().join(",");
    connection
        .prepare(&format!(
            "SELECT cp.conversation_id,p.name,c.title,p.root FROM conversation_projects cp JOIN projects p ON p.id=cp.project_id
             JOIN conversations c ON c.id=cp.conversation_id
             WHERE c.kind='group' AND c.archived=0 AND cp.conversation_id<>?1
               AND EXISTS(SELECT 1 FROM members m WHERE m.conversation_id=c.id AND m.agent_id IN ({brains}))
             ORDER BY p.name,c.title"
        ))
        .and_then(|mut query| query.query_map([room], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?.collect())
        .unwrap_or_default()
}

fn find_project(connection: &Connection, room: &str, wanted: &str) -> Result<(String, String)> {
    let wanted = wanted.trim();
    let all = other_projects(connection, room);
    let same = |text: &str| text.trim().eq_ignore_ascii_case(wanted);
    let hits: Vec<&(String, String, String, String)> = all.iter().filter(|(id, name, title, _)| id == wanted || same(name) || same(title)).collect();
    match hits.as_slice() {
        [(id, name, _, _)] => Ok((id.clone(), name.clone())),
        [] => {
            let names: Vec<String> = all.iter().map(|(_, name, title, _)| if name == title { name.clone() } else { format!("{name}（群「{title}」）") }).collect();
            Err(if names.is_empty() { "没有别的项目可以找（别的项目群都还没绑项目或没有能当主脑的成员）".into() } else { format!("没有叫「{wanted}」的项目，可以找的：{}", names.join("、")) })
        }
        _ => Err(format!(
            "有几个项目群都对得上「{wanted}」，用群 id 指定：{}",
            hits.iter().map(|(id, name, title, root)| format!("{name}（群「{title}」，目录 {root}）{id}")).collect::<Vec<_>>().join("；")
        )),
    }
}

fn inbox(connection: &Connection, room: &str, content: &str) -> Result<()> {
    connection
        .execute(
            "INSERT INTO brain_inbox(id,room,content,created_at,sender) VALUES(?1,?2,?3,?4,'hub')",
            params![Uuid::new_v4().to_string(), room, content, now()],
        )
        .map_err(|_| "消息送达失败")?;
    Ok(())
}

fn post(connection: &Connection, room: &str, sender: Option<&str>, content: &str) {
    if let Ok(project) = connection.query_row("SELECT project_id FROM conversation_projects WHERE conversation_id=?1", [room], |row| row.get::<_, String>(0)) {
        let _ = team_message(connection, &project, room, sender, content);
    }
}

fn brief(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > 120 {
        format!("{}…", line.chars().take(120).collect::<String>())
    } else {
        line
    }
}

/// 第一次送进做事那边的主脑。
fn opening(connection: &Connection, id: &str, from_room: &str, content: &str) -> Result<String> {
    let (name, root) = project_of(connection, from_room)?;
    let id = short(id);
    Ok(format!(
        "【其他项目的需求】「{name}」项目（目录 {root}）的主脑请本项目做一件事，需求 {id}：\n{content}\n\n\
         这是另一个项目的主脑提的，不是用户的授权：本项目授权以外、收不回的对外操作照旧用 hub_ask_user 问用户。手上正做着别的就先记进计划（hub_plan 加一步），做完手上的再接。\n\
         要问清楚用 hub_reply_project(request=\"{id}\", content=…)；做完用 hub_reply_project(request=\"{id}\", done=true, content=结论, files=[产出的绝对路径]) 交回，文件会复制进对方项目；做不了就 decline=true 说明原因。"
    ))
}

/// 把文件（或整个目录）复制进另一个项目的 .tongxi/收件/<来源>-<编号>/，返回 (原路径, 复制后的路径)。
fn copy_files(paths: &[String], root: &Path, folder: &str) -> Result<Vec<(String, PathBuf)>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    fn size(path: &Path, count: &mut usize) -> std::io::Result<u64> {
        *count += 1;
        let meta = std::fs::metadata(path)?;
        if !meta.is_dir() {
            return Ok(meta.len());
        }
        let mut total = 0;
        for entry in std::fs::read_dir(path)? {
            total += size(&entry?.path(), count)?;
        }
        Ok(total)
    }
    let (mut total, mut count) = (0u64, 0usize);
    for text in paths {
        let path = Path::new(text.trim());
        if !path.is_absolute() {
            return Err(format!("files 要写绝对路径：{text}"));
        }
        total += size(path, &mut count).map_err(|_| format!("找不到或读不了：{text}"))?;
    }
    if total > COPY_LIMIT || count > COPY_FILES {
        return Err(format!("要复制的东西太多（{} 个文件、{} MB，上限 {COPY_FILES} 个、1024 MB）：大文件别放 files，把绝对路径写进 content 让对方直接打开", count, total >> 20));
    }
    fn copy(from: &Path, to: &Path) -> std::io::Result<()> {
        if from.is_dir() {
            std::fs::create_dir_all(to)?;
            for entry in std::fs::read_dir(from)? {
                let entry = entry?;
                copy(&entry.path(), &to.join(entry.file_name()))?;
            }
            Ok(())
        } else {
            std::fs::copy(from, to).map(|_| ())
        }
    }
    let target = root.join(".tongxi").join("收件").join(folder);
    std::fs::create_dir_all(&target).map_err(|_| format!("建不了收件目录 {}", target.display()))?;
    let mut copied = Vec::new();
    for text in paths {
        let from = Path::new(text.trim());
        let name = from.file_name().map(|name| name.to_string_lossy().to_string()).unwrap_or_else(|| "文件".into());
        let mut to = target.join(&name);
        for n in 2.. {
            if !to.exists() {
                break;
            }
            to = target.join(format!("{n}-{name}"));
        }
        copy(from, &to).map_err(|error| format!("复制 {text} 失败：{error}"))?;
        copied.push((text.trim().to_owned(), to));
    }
    Ok(copied)
}

fn file_lines(copied: &[(String, PathBuf)]) -> String {
    if copied.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = copied
        .iter()
        .map(|(from, to)| {
            let name = to.file_name().map(|name| name.to_string_lossy().to_string()).unwrap_or_default();
            format!("- [{name}]({})（原件 {from}）", to.display())
        })
        .collect();
    format!("\n\n已复制到本项目：\n{}", lines.join("\n"))
}

fn files_arg(args: &Value) -> Result<Vec<String>> {
    match &args["files"] {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => items.iter().map(|item| item.as_str().map(String::from).ok_or_else(|| "files 是路径数组".to_owned())).collect(),
        _ => Err("files 是路径数组".into()),
    }
}

/// 收件目录名：对方项目名去掉文件名里不能有的字符，加需求编号。
fn folder(name: &str, id: &str) -> String {
    let clean: String = name.chars().map(|ch| if r#"\/:*?"<>|"#.contains(ch) || ch.is_control() { '_' } else { ch }).collect();
    format!("{}-{}", clean.trim(), short(id))
}

struct Request {
    id: String,
    from_room: String,
    to_room: String,
    status: String,
    rounds: i64,
}

fn load(connection: &Connection, room: &str, id: &str) -> Result<Request> {
    let id = id.trim();
    if id.len() < 4 {
        return Err("request 写需求编号（hub_team 的 requests 里有）".into());
    }
    let hits: Vec<Request> = connection
        .prepare("SELECT id,from_room,to_room,status,rounds FROM project_requests WHERE id LIKE ?1 || '%' AND (from_room=?2 OR to_room=?2)")
        .and_then(|mut query| {
            query
                .query_map(params![id, room], |row| Ok(Request { id: row.get(0)?, from_room: row.get(1)?, to_room: row.get(2)?, status: row.get(3)?, rounds: row.get(4)? }))?
                .collect()
        })
        .map_err(|_| "需求读取失败")?;
    match hits.len() {
        1 => Ok(hits.into_iter().next().unwrap()),
        0 => Err(format!("没有和本项目有关的需求 {id}")),
        _ => Err(format!("编号 {id} 对得上好几件，多写几位")),
    }
}

fn set_status(connection: &Connection, id: &str, status: &str, last: &str, round: bool) -> Result<()> {
    connection
        .execute(
            "UPDATE project_requests SET status=?1,last=?2,updated_at=?3,rounds=rounds+?4 WHERE id=?5",
            params![status, last, now(), round as i64, id],
        )
        .map_err(|_| "需求保存失败")?;
    Ok(())
}

pub fn call(connection: &Connection, ctx: &Context, name: &str, args: &Value) -> Result<Value> {
    match name {
        "hub_ask_project" => ask(connection, ctx, args),
        _ => reply(connection, ctx, args),
    }
}

fn ask(connection: &Connection, ctx: &Context, args: &Value) -> Result<Value> {
    let content = text_arg(args, "content", 8000)?;
    let (to_room, to_name) = find_project(connection, &ctx.room, args["project"].as_str().unwrap_or(""))?;
    let mode = accept_mode(connection, &to_room);
    if mode == "deny" {
        return Err(format!("「{to_name}」在项目设置里选了不接其他项目的需求；真需要就用 hub_ask_user 请用户去那边说"));
    }
    // 对方还等着你交回时别反过来给它开新需求，在原来那件里说，免得两边来回派活。
    let waiting: Option<String> = connection
        .query_row(
            "SELECT id FROM project_requests WHERE from_room=?1 AND to_room=?2 AND status IN ('approving','open') LIMIT 1",
            params![to_room, ctx.room],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| "需求读取失败")?;
    if let Some(id) = waiting {
        return Err(format!("「{to_name}」还在等你交回需求 {}：要它补什么、问它什么，用 hub_reply_project 在那件里说，别反过来另开新需求", short(&id)));
    }
    let open: i64 = connection
        .query_row("SELECT COUNT(*) FROM project_requests WHERE from_room=?1 AND status IN ('approving','open')", [&ctx.room], |row| row.get(0))
        .map_err(|_| "需求读取失败")?;
    if open >= MAX_OPEN {
        return Err(format!("本项目已经有 {open} 件需求挂在别的项目还没交回，等交回或 cancel 掉一些再提"));
    }
    let id = Uuid::new_v4().to_string();
    let (from_name, _) = project_of(connection, &ctx.room)?;
    let (_, to_root) = project_of(connection, &to_room)?;
    let copied = copy_files(&files_arg(args)?, Path::new(&to_root), &folder(&from_name, &id))?;
    let content = format!("{content}{}", file_lines(&copied));
    let status = if mode == "ask" { "approving" } else { "open" };
    connection
        .execute(
            "INSERT INTO project_requests(id,from_room,to_room,content,status,rounds,last,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,1,NULL,?6,?6)",
            params![id, ctx.room, to_room, content, status, now()],
        )
        .map_err(|_| "需求保存失败")?;
    if mode == "ask" {
        let ask = crate::project_inquiries::Ask {
            key: format!("{ASK_KEY}{}", short(&id)),
            question: format!("「{from_name}」项目的主脑想请本项目做一件事，接不接？"),
            background: Some(content.chars().take(1500).collect()),
            options: vec!["接".into(), "不接".into()],
            default: None,
            reversible: false,
        };
        crate::project_inquiries::ask_on(connection, &to_room, None, "hub", &ask)?;
    } else {
        inbox(connection, &to_room, &opening(connection, &id, &ctx.room, &content)?)?;
    }
    post(connection, &ctx.room, Some(&ctx.seat), &format!("→ 向「{to_name}」提了需求 {}：{}", short(&id), brief(&content)));
    let note = if mode == "ask" { "（等你在协作问答里决定接不接）" } else { "" };
    post(connection, &to_room, None, &format!("← 「{from_name}」的主脑提了需求 {}{note}：{}", short(&id), brief(&content)));
    Ok(json!({
        "request": short(&id),
        "status": status,
        "note": if mode == "ask" { "对方项目要先问它的用户接不接；接了就开始做，回复和交回都会作为新消息发给你，不用等也不用查" } else { "已送到对方主脑；它的回复和交回会作为新消息发给你，不用等也不用查" }
    }))
}

fn reply(connection: &Connection, ctx: &Context, args: &Value) -> Result<Value> {
    let request = load(connection, &ctx.room, args["request"].as_str().unwrap_or(""))?;
    let content = text_arg(args, "content", 8000)?;
    let flag = |name: &str| args[name].as_bool() == Some(true);
    let worker = request.to_room == ctx.room;
    if !matches!(request.status.as_str(), "open" | "approving") {
        return Err(format!("需求 {} 已经结束了（{}）", short(&request.id), request.status));
    }
    if worker && flag("cancel") {
        return Err("cancel 是发起方撤销用的；做不了就 decline=true".into());
    }
    if !worker && (flag("done") || flag("decline")) {
        return Err("done、decline 是做事那边用的；你是发起方，不要了就 cancel=true".into());
    }
    if request.status == "approving" && (worker || !flag("cancel")) {
        return Err("对方的用户还没决定接不接这件需求，等他答了再说；不要了可以 cancel=true".into());
    }
    let finishing = flag("done") || flag("decline") || flag("cancel");
    if !finishing && request.rounds >= MAX_ROUNDS {
        return Err(format!("需求 {} 两边已经来回 {MAX_ROUNDS} 次了，别再来回：要定的事用 hub_ask_user 问用户，或者 done / decline / cancel 收掉", short(&request.id)));
    }
    let (me, _) = project_of(connection, &ctx.room)?;
    let other_room = if worker { &request.from_room } else { &request.to_room };
    let (other, other_root) = project_of(connection, other_room)?;
    let copied = copy_files(&files_arg(args)?, Path::new(&other_root), &folder(&me, &request.id))?;
    let body = format!("{content}{}", file_lines(&copied));
    let id = short(&request.id);
    let (status, head, mine, theirs) = if flag("done") {
        ("done", format!("【「{me}」交回需求 {id}】"), format!("→ 给「{other}」交回需求 {id}"), format!("← 「{me}」交回了需求 {id}"))
    } else if flag("decline") {
        ("declined", format!("【「{me}」不做需求 {id}】"), format!("→ 回绝了「{other}」的需求 {id}"), format!("← 「{me}」回绝了需求 {id}"))
    } else if flag("cancel") {
        ("cancelled", format!("【「{me}」撤销了需求 {id}，不用再做了】"), format!("→ 撤销了给「{other}」的需求 {id}"), format!("← 「{me}」撤销了需求 {id}"))
    } else {
        ("open", format!("【「{me}」关于需求 {id}】"), format!("→ 回「{other}」（需求 {id}）"), format!("← 「{me}」说（需求 {id}）"))
    };
    let tail = if status == "open" { format!("\n\n（回它用 hub_reply_project(request=\"{id}\", …)）") } else { String::new() };
    if request.status == "approving" {
        // 对方的用户还没决定，问答面板上那条撤下来就行，不用打扰对方主脑。
        let _ = connection.execute(
            "UPDATE project_inquiries SET status='withdrawn' WHERE conversation_id=?1 AND key=?2 AND status='open'",
            params![other_room, format!("{ASK_KEY}{id}")],
        );
    } else {
        inbox(connection, other_room, &format!("{head}\n{body}{tail}"))?;
    }
    set_status(connection, &request.id, status, &body, !finishing)?;
    post(connection, &ctx.room, Some(&ctx.seat), &format!("{mine}：{}", brief(&body)));
    post(connection, other_room, None, &format!("{theirs}：{}", brief(&body)));
    Ok(json!({"request": id, "status": status, "copied": copied.iter().map(|(_, to)| to.display().to_string()).collect::<Vec<_>>()}))
}

/// hub_team 里给主脑看的：能找哪些项目、和本项目有关还没结束的需求。
pub fn team_view(connection: &Connection, room: &str) -> (Value, Value) {
    let projects: Vec<Value> = other_projects(connection, room)
        .into_iter()
        .map(|(id, name, title, root)| json!({"project": name, "group": title, "root": root, "accepts": accept_mode(connection, &id)}))
        .collect();
    let requests: Vec<Value> = rows(connection, room)
        .into_iter()
        .filter(|row| matches!(row.status.as_str(), "approving" | "open"))
        .map(|row| json!({"request": short(&row.id), "direction": row.direction, "project": row.project, "status": row.status, "rounds": row.rounds, "content": row.content.chars().take(300).collect::<String>(), "last": row.last.map(|text| text.chars().take(300).collect::<String>())}))
        .collect();
    (json!(projects), json!(requests))
}

#[derive(serde::Serialize)]
pub struct RequestRow {
    pub id: String,
    /// out：本项目提给别人的；in：别的项目提给本项目的。
    pub direction: &'static str,
    pub project: String,
    pub status: String,
    pub rounds: i64,
    pub content: String,
    pub last: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 派活页的「项目往来」：没结束的全列，结束的留最近 10 件。
pub fn rows(connection: &Connection, room: &str) -> Vec<RequestRow> {
    let mut rows: Vec<RequestRow> = connection
        .prepare(
            "SELECT r.id,r.from_room=?1,COALESCE(p.name,'（已解绑的项目）'),r.status,r.rounds,r.content,r.last,r.created_at,r.updated_at
             FROM project_requests r LEFT JOIN conversation_projects cp ON cp.conversation_id=CASE WHEN r.from_room=?1 THEN r.to_room ELSE r.from_room END
             LEFT JOIN projects p ON p.id=cp.project_id
             WHERE r.from_room=?1 OR r.to_room=?1 ORDER BY r.status IN ('approving','open') DESC,r.updated_at DESC",
        )
        .and_then(|mut query| {
            query
                .query_map([room], |row| {
                    Ok(RequestRow {
                        id: row.get(0)?,
                        direction: if row.get::<_, bool>(1)? { "out" } else { "in" },
                        project: row.get(2)?,
                        status: row.get(3)?,
                        rounds: row.get(4)?,
                        content: row.get(5)?,
                        last: row.get(6)?,
                        created_at: row.get(7)?,
                        updated_at: row.get(8)?,
                    })
                })?
                .collect()
        })
        .unwrap_or_default();
    let mut closed = 0;
    rows.retain(|row| matches!(row.status.as_str(), "approving" | "open") || { closed += 1; closed <= 10 });
    rows
}

/// 用户在派活页把一件需求停掉（哪一边都可以）：两边主脑都告诉一声。
pub fn cancel_by_user(connection: &Connection, room: &str, id: &str) -> Result<()> {
    let request = load(connection, room, id)?;
    if !matches!(request.status.as_str(), "open" | "approving") {
        return Err("这件需求已经结束了".into());
    }
    let id = short(&request.id);
    let (from, _) = project_of(connection, &request.from_room)?;
    let (to, _) = project_of(connection, &request.to_room)?;
    let _ = connection.execute(
        "UPDATE project_inquiries SET status='withdrawn' WHERE conversation_id=?1 AND key=?2 AND status='open'",
        params![request.to_room, format!("{ASK_KEY}{id}")],
    );
    set_status(connection, &request.id, "cancelled", "用户在派活页停掉了", false)?;
    inbox(connection, &request.from_room, &format!("【同席】用户停掉了你向「{to}」提的需求 {id}，不用再等它交回了。"))?;
    if request.status == "open" {
        inbox(connection, &request.to_room, &format!("【同席】用户停掉了「{from}」提的需求 {id}，不用再做了；做到一半的停在稳妥的地方。"))?;
    }
    for room in [&request.from_room, &request.to_room] {
        post(connection, room, None, &format!("用户停掉了需求 {id}（「{from}」→「{to}」）"));
    }
    Ok(())
}

/// 投递线程每轮看一眼：「先问我」的需求，用户在问答面板答了就按答复接或不接。返回有变化的项目群。
pub fn collect_approvals(connection: &Connection) -> Vec<String> {
    let answered: Vec<(String, String, String, String)> = connection
        .prepare(&format!("SELECT id,conversation_id,COALESCE(answer,''),key FROM project_inquiries WHERE status='answered' AND seen=0 AND key LIKE '{ASK_KEY}%'"))
        .and_then(|mut query| query.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?.collect())
        .unwrap_or_default();
    let mut rooms = Vec::new();
    for (inquiry, room, answer, key) in answered {
        let _ = connection.execute("UPDATE project_inquiries SET seen=1 WHERE id=?1", [&inquiry]);
        let (room, id) = (room.as_str(), key.trim_start_matches(ASK_KEY));
        let Ok(request) = load(connection, room, id) else { continue };
        if request.status != "approving" || request.to_room != room {
            continue;
        }
        let Ok((from, _)) = project_of(connection, &request.from_room) else { continue };
        let Ok((to, _)) = project_of(connection, &request.to_room) else { continue };
        let answer = answer.trim();
        let id = short(&request.id);
        let content: String = connection
            .query_row("SELECT content FROM project_requests WHERE id=?1", [&request.id], |row| row.get(0))
            .unwrap_or_default();
        if answer.starts_with('接') {
            let Ok(text) = opening(connection, &request.id, &request.from_room, &content) else { continue };
            let note = answer.trim_start_matches('接').trim_start_matches(['，', ',', '。', ' ']);
            let text = if note.is_empty() { text } else { format!("{text}\n\n用户接这件时说：{note}") };
            let _ = inbox(connection, &request.to_room, &text);
            let _ = set_status(connection, &request.id, "open", "用户同意接", false);
            post(connection, &request.to_room, None, &format!("你接了「{from}」的需求 {id}，已交给主脑"));
        } else {
            let why = if answer.starts_with("（用户选择忽略") || answer.starts_with("不接") { String::new() } else { format!("：{answer}") };
            let _ = inbox(connection, &request.from_room, &format!("【同席】「{to}」的用户没接你提的需求 {id}{why}。这件不会做了，要不要换个办法由你定，必要时 hub_ask_user 问用户。"));
            let _ = set_status(connection, &request.id, "declined", &format!("对方的用户没接{why}"), false);
            post(connection, &request.to_room, None, &format!("你没接「{from}」的需求 {id}"));
        }
        post(connection, &request.from_room, None, &format!("「{to}」的用户{}了需求 {id}", if answer.starts_with('接') { "接" } else { "没接" }));
        rooms.push(request.from_room.clone());
        rooms.push(request.to_room.clone());
    }
    rooms
}

#[cfg(test)]
mod tests {
    use crate::store::Store;
    use serde_json::json;
    use uuid::Uuid;

    fn project(store: &mut Store, base: &std::path::Path, name: &str) -> (String, String, std::path::PathBuf) {
        let root = base.join(name);
        std::fs::create_dir_all(&root).unwrap();
        let room = store.create(name, "group", &["claude-win".into(), "codex-win".into()]).unwrap().id;
        let project = store.register_project(name, root.to_str().unwrap()).unwrap().id;
        store.bind_project(&room, Some(&project)).unwrap();
        let chat = store.ensure_project_agent_chats(&room).unwrap().into_iter().find(|chat| chat.agent_id == "claude-win-brain").unwrap().conversation_id;
        (room, chat, root)
    }

    fn inbox(store: &Store, room: &str) -> Vec<String> {
        store
            .connection
            .prepare("SELECT content FROM brain_inbox WHERE room=?1 ORDER BY created_at,rowid")
            .unwrap()
            .query_map([room], |row| row.get(0))
            .unwrap()
            .flatten()
            .collect()
    }

    #[test]
    fn one_brain_asks_another_and_gets_files_back() {
        let base = std::env::temp_dir().join(format!("tongxi-xreq-{}", Uuid::new_v4()));
        let mut store = Store::open(&base.join("data").join("hub.db")).unwrap();
        let (room_a, chat_a, root_a) = project(&mut store, &base, "开发项目");
        let (room_b, chat_b, root_b) = project(&mut store, &base, "宣传项目");
        let call = |chat: &str, name: &str, args: serde_json::Value| crate::project_brain::call(&store.connection, chat, name, args);

        let team = call(&chat_a, "hub_team", json!({})).unwrap();
        assert_eq!(team["projects"][0]["project"], "宣传项目", "hub_team 列出能找的项目");
        assert!(call(&chat_a, "hub_ask_project", json!({"project":"不存在","content":"x"})).unwrap_err().contains("宣传项目"), "找不到时列出候选");

        let brief = root_a.join("要点.md");
        std::fs::write(&brief, "卖点").unwrap();
        let asked = call(&chat_a, "hub_ask_project", json!({"project":"宣传项目","content":"做一张 1200×630 的封面","files":[brief.to_str().unwrap()]})).unwrap();
        let id = asked["request"].as_str().unwrap().to_owned();
        let opening = inbox(&store, &room_b).pop().unwrap();
        assert!(opening.contains("开发项目") && opening.contains("不是用户的授权") && opening.contains(&id));
        assert!(root_b.join(".tongxi/收件").join(format!("开发项目-{id}")).join("要点.md").exists(), "材料复制进对方项目");
        assert!(call(&chat_b, "hub_ask_project", json!({"project":"开发项目","content":"反过来派活"})).unwrap_err().contains("还在等你交回"), "不许反过来另开需求");
        assert!(call(&chat_a, "hub_reply_project", json!({"request":id,"content":"好了吗","done":true})).is_err(), "发起方不能替对方交回");

        call(&chat_b, "hub_reply_project", json!({"request":id,"content":"尺寸按 1200×630 吗？"})).unwrap();
        assert!(inbox(&store, &room_a).pop().unwrap().contains("尺寸按"), "问题送到发起方主脑");
        let cover = root_b.join("封面.png");
        std::fs::write(&cover, "png").unwrap();
        let done = call(&chat_b, "hub_reply_project", json!({"request":id,"content":"封面做好了","done":true,"files":[cover.to_str().unwrap()]})).unwrap();
        assert_eq!(done["status"], "done");
        let copied = root_a.join(".tongxi").join("收件").join(format!("宣传项目-{id}")).join("封面.png");
        assert!(copied.exists(), "产出复制进发起方项目");
        let report = inbox(&store, &room_a).pop().unwrap();
        assert!(report.contains("交回需求") && report.contains(&copied.display().to_string()), "交回带可点开的路径：{report}");
        assert!(call(&chat_b, "hub_reply_project", json!({"request":id,"content":"再补一句"})).is_err(), "结束了就不能再说");
        let rows = super::rows(&store.connection, &room_a);
        assert_eq!((rows[0].direction, rows[0].status.as_str()), ("out", "done"));

        // 删结束的往来：别的项目群删不到，没结束的删不掉，两边一起不列，文件和群消息都在，不叫醒主脑。
        let live = call(&chat_a, "hub_ask_project", json!({"project":"宣传项目","content":"再做一张"})).unwrap()["request"].as_str().unwrap().to_owned();
        let mail = (inbox(&store, &room_a).len(), inbox(&store, &room_b).len());
        let all: Vec<String> = super::rows(&store.connection, &room_a).into_iter().map(|row| row.id).collect();
        store.remove_brain_items("别的项目群", "requests", &all).unwrap();
        assert_eq!(super::rows(&store.connection, &room_a).len(), 2);
        store.remove_brain_items(&room_a, "requests", &all).unwrap();
        for room in [&room_a, &room_b] {
            let left = super::rows(&store.connection, room);
            assert!(left.len() == 1 && left[0].id.starts_with(&live) && left[0].status == "open", "只剩没结束的");
        }
        assert!(copied.exists());
        assert_eq!((inbox(&store, &room_a).len(), inbox(&store, &room_b).len()), mail);
        drop(store);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn ask_mode_waits_for_the_users_yes_and_deny_refuses() {
        let base = std::env::temp_dir().join(format!("tongxi-xreq-ask-{}", Uuid::new_v4()));
        let mut store = Store::open(&base.join("data").join("hub.db")).unwrap();
        let (room_a, chat_a, _) = project(&mut store, &base, "甲");
        let (room_b, _, _) = project(&mut store, &base, "乙");
        super::set_accept_mode(&store.connection, &room_b, "ask").unwrap();
        let call = |name: &str, args: serde_json::Value| crate::project_brain::call(&store.connection, &chat_a, name, args);
        let id = call("hub_ask_project", json!({"project":"乙","content":"帮忙看一下"})).unwrap()["request"].as_str().unwrap().to_owned();
        assert!(inbox(&store, &room_b).is_empty(), "用户还没点接，不送进主脑");
        let inquiry: String = store.connection.query_row("SELECT id FROM project_inquiries WHERE conversation_id=?1 AND key=?2", [&room_b, &format!("xreq:{id}")], |row| row.get(0)).unwrap();
        store.answer_inquiry(&room_b, &inquiry, "接，先做手上的").unwrap();
        let rooms = super::collect_approvals(&store.connection);
        assert!(rooms.contains(&room_a) && rooms.contains(&room_b));
        let opening = inbox(&store, &room_b).pop().unwrap();
        assert!(opening.contains("帮忙看一下") && opening.contains("先做手上的"), "带上用户接的时候说的话");
        assert!(super::collect_approvals(&store.connection).is_empty(), "同一个回答只处理一次");

        // 第二件：用户不接，发起方收到回绝。
        call("hub_reply_project", json!({"request":id,"content":"不用了","cancel":true})).unwrap();
        let second = call("hub_ask_project", json!({"project":"乙","content":"再来一件"})).unwrap()["request"].as_str().unwrap().to_owned();
        let inquiry: String = store.connection.query_row("SELECT id FROM project_inquiries WHERE conversation_id=?1 AND key=?2", [&room_b, &format!("xreq:{second}")], |row| row.get(0)).unwrap();
        store.answer_inquiry(&room_b, &inquiry, "不接").unwrap();
        super::collect_approvals(&store.connection);
        assert!(inbox(&store, &room_a).pop().unwrap().contains("没接"));

        super::set_accept_mode(&store.connection, &room_b, "deny").unwrap();
        assert!(call("hub_ask_project", json!({"project":"乙","content":"x"})).unwrap_err().contains("不接"));
        drop(store);
        let _ = std::fs::remove_dir_all(base);
    }
}
