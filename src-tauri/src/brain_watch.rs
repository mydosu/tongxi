//! 长程待机：主脑用 hub_schedule 挂的定时提醒和状态监控，到点由同席叫醒，主脑不用一直醒着轮询。
//! 到点时：纯定时的把 note 送进主脑；带 check 的先在项目目录跑一条 PowerShell，退出码或输出和上次不一样才叫醒；
//! 指定了 member 的交给那个成员去看（派一件活），成员交回「无变化」就不叫醒主脑，省主脑的额度。
//! 同席没开着时什么都不跑；开了以后错过的合成一次补上、注明晚了多久错过几次。
use crate::project_brain::{label, optional_arg, short, team_message, text_arg, Context};
use crate::project_store::base_agent;
use crate::store::{now, Store};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::Serialize;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::Emitter;
use uuid::Uuid;

type Result<T> = std::result::Result<T, String>;

const MINUTE: i64 = 60_000;
/// 纯定时会叫醒主脑或成员、花额度，最密 30 分钟一次；check 只在本机跑命令，最密 5 分钟。
const TIMER_FLOOR: i64 = 30;
const CHECK_FLOOR: i64 = 5;
const MAX_EVERY: i64 = 30 * 24 * 60;
const MAX_AHEAD: i64 = 60 * 24 * 60;
const MAX_ACTIVE: i64 = 20;
/// 一个待机一天最多叫醒这么多次，超了就暂停（多半是 check 的输出每次都变）；一个项目一天最多这么多次，超了往后推。
const WATCH_DAILY: i64 = 24;
const ROOM_DAILY: i64 = 60;
const CHECK_TIMEOUT: Duration = Duration::from_secs(60);
const CHECK_FAILURES: i64 = 3;
const OUTPUT_LIMIT: usize = 4000;
/// 成员交回只写这个，就是没新情况，不叫醒主脑。
pub const NOTHING_NEW: &str = "无变化";
/// 成员交回「无变化」的待机活记成这个类别，派活页不列、主脑不收。
pub const QUIET_CATEGORY: &str = "待机·无变化";

pub fn migrate(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS brain_watches (
           id TEXT PRIMARY KEY,room TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
           note TEXT NOT NULL,every_minutes INTEGER,next_at INTEGER NOT NULL,until_at INTEGER,
           check_cmd TEXT,member TEXT,once INTEGER NOT NULL DEFAULT 0,status TEXT NOT NULL,
           baseline TEXT,last_at INTEGER,last_result TEXT,fired INTEGER NOT NULL DEFAULT 0,failures INTEGER NOT NULL DEFAULT 0,
           created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL);
         CREATE INDEX IF NOT EXISTS brain_watch_due ON brain_watches(status,next_at);
         CREATE TABLE IF NOT EXISTS brain_watch_log (
           id INTEGER PRIMARY KEY AUTOINCREMENT,watch TEXT NOT NULL,room TEXT NOT NULL,at INTEGER NOT NULL);",
    )
    .map_err(|_| "待机结构迁移失败")?;
    // 哪个待机派的活：成员交回时据此决定叫不叫醒主脑。
    crate::project_store::add_column(tx, "brain_jobs", "watch", "TEXT")
}

pub fn tool_spec() -> (&'static str, &'static str, Value, Vec<&'static str>) {
    let string = json!({"type":"string"});
    let integer = json!({"type":"integer"});
    let boolean = json!({"type":"boolean"});
    (
        "hub_schedule",
        "Standby: timers and watches that wake you (or a member) later, so you never stay awake polling or sleep-waiting. Create: note (what to do when it fires; it comes back to you as a message) plus timing: at (local time 'YYYY-MM-DD HH:MM', or 'HH:MM' for its next occurrence) or in_minutes for the first firing, every_minutes to repeat (pure timers at least 30; with check at least 5), until (local time) to stop repeating. check: a PowerShell command run in the project directory each time (60 s limit, needs the full permission tier); it runs once right away and this call returns what it printed (check_now) as the baseline, so make sure that shows what you mean to watch, not an error or nothing; after that you are woken only when its exit code or output differs from the previous run, so make it print just what matters, no timestamps or counters; once=true ends the watch at its first change. member: hand each firing to that member as a delegated job instead of waking you (cheaper for routine looks); if its hand-back is just 无变化 you are not woken. Update: id plus the fields to change; pause=true, resume=true, remove=true. Remove a watch as soon as it has served its purpose; ended or removed ones leave the list and can be brought back by id with resume=true for 7 days. No arguments: list this project's watches with the current local time. Nothing runs while the app is closed; missed firings run once on restart, marked late.",
        json!({"id":string,"note":string,"at":string,"in_minutes":integer,"every_minutes":integer,"until":string,"check":string,"once":boolean,"member":string,"pause":boolean,"resume":boolean,"remove":boolean}),
        vec![],
    )
}

/// 本地时间「YYYY-MM-DD HH:MM」或「HH:MM」（下一次到这个钟点）→ 毫秒时间戳。
fn parse_local(connection: &Connection, text: &str) -> Result<i64> {
    let text = text.trim().replace('T', " ");
    let bad = || format!("时间写成「2026-11-18 09:00」或「09:00」这样的本地时间：{text}");
    let digits = |part: &str| part.chars().all(|ch| ch.is_ascii_digit());
    let full = match text.len() {
        5 if digits(&text[..2]) && &text[2..3] == ":" && digits(&text[3..]) => None,
        16 if text.is_char_boundary(10) && digits(&text[..4]) && digits(&text[5..7]) && digits(&text[8..10]) && digits(&text[11..13]) && digits(&text[14..]) => Some(text.clone()),
        _ => return Err(bad()),
    };
    let seconds: Option<String> = match &full {
        Some(full) => connection.query_row("SELECT strftime('%s',?1,'utc')", [full], |row| row.get(0)),
        None => connection.query_row("SELECT strftime('%s',date('now','localtime')||' '||?1,'utc')", [&text], |row| row.get(0)),
    }
    .map_err(|_| bad())?;
    let mut at = seconds.and_then(|text| text.parse::<i64>().ok()).ok_or_else(bad)? * 1000;
    if full.is_none() && at <= now() {
        at += 24 * 60 * MINUTE;
    }
    Ok(at)
}

fn local(connection: &Connection, at: i64) -> String {
    connection
        .query_row("SELECT strftime('%Y-%m-%d %H:%M',?1/1000,'unixepoch','localtime')", [at], |row| row.get(0))
        .unwrap_or_default()
}

fn minutes_arg(args: &Value, name: &str, low: i64, high: i64) -> Result<Option<i64>> {
    match &args[name] {
        Value::Null => Ok(None),
        value => match value.as_i64() {
            Some(minutes) if (low..=high).contains(&minutes) => Ok(Some(minutes)),
            _ => Err(format!("{name} 要填 {low}–{high} 的整数分钟")),
        },
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Watch {
    pub id: String,
    #[serde(skip)]
    pub room: String,
    pub note: String,
    pub every_minutes: Option<i64>,
    pub next_at: i64,
    pub until_at: Option<i64>,
    pub check_cmd: Option<String>,
    pub member: Option<String>,
    pub member_name: Option<String>,
    pub once: bool,
    pub status: String,
    #[serde(skip)]
    pub baseline: Option<String>,
    pub last_at: Option<i64>,
    pub last_result: Option<String>,
    pub fired: i64,
    #[serde(skip)]
    pub failures: i64,
    pub created_at: i64,
}

const COLUMNS: &str = "id,room,note,every_minutes,next_at,until_at,check_cmd,member,once,status,baseline,last_at,last_result,fired,failures,created_at";

fn row(connection: &Connection) -> impl Fn(&rusqlite::Row<'_>) -> rusqlite::Result<Watch> + '_ {
    move |row| {
        let room: String = row.get(1)?;
        let member: Option<String> = row.get(7)?;
        Ok(Watch {
            id: row.get(0)?,
            member_name: member.as_deref().map(|seat| label(connection, &room, seat)),
            room,
            note: row.get(2)?,
            every_minutes: row.get(3)?,
            next_at: row.get(4)?,
            until_at: row.get(5)?,
            check_cmd: row.get(6)?,
            member,
            once: row.get(8)?,
            status: row.get(9)?,
            baseline: row.get(10)?,
            last_at: row.get(11)?,
            last_result: row.get(12)?,
            fired: row.get(13)?,
            failures: row.get(14)?,
            created_at: row.get(15)?,
        })
    }
}

/// 还挂着的待机。结束的、主脑撤掉的不列，留 7 天给主脑按编号挂回来。
pub fn list(connection: &Connection, room: &str) -> Vec<Watch> {
    connection
        .prepare(&format!("SELECT {COLUMNS} FROM brain_watches WHERE room=?1 AND status<>'done' ORDER BY status='paused',next_at"))
        .and_then(|mut query| query.query_map([room], row(connection))?.collect())
        .unwrap_or_default()
}

/// 待机叫醒主脑的消息末尾带上还挂着哪些，让它做完顺手清掉用不着的，不然越挂越多。ended：这次刚结束的，不列。
pub fn tidy_hint(connection: &Connection, room: &str, ended: Option<&str>) -> String {
    let left: Vec<String> = list(connection, room)
        .iter()
        .filter(|watch| ended != Some(watch.id.as_str()))
        .map(|watch| {
            let state = if watch.status == "paused" { "已暂停".into() } else { cadence(connection, watch.every_minutes, watch.next_at, watch.check_cmd.is_some()) };
            format!("- {}（{state}，触发过 {} 次）{}", short(&watch.id), watch.fired, brief(&watch.note).chars().take(40).collect::<String>())
        })
        .collect();
    if left.is_empty() {
        return String::new();
    }
    format!("\n\n【同席】做完这次的事，顺手把用不着的待机撤掉（hub_schedule 给 id 和 remove=true）：要等的等到了、事情已经结束、被后来挂的代替了的。还用得着的、用户暂停的、拿不准的留着。现在挂着：\n{}", left.join("\n"))
}

fn load(connection: &Connection, room: &str, id: &str) -> Result<Watch> {
    let id = id.trim();
    let hits: Vec<Watch> = connection
        .prepare(&format!("SELECT {COLUMNS} FROM brain_watches WHERE room=?1 AND id LIKE ?2 || '%'"))
        .and_then(|mut query| query.query_map(params![room, id], row(connection))?.collect())
        .map_err(|_| "待机读取失败")?;
    match (id.len() >= 4, hits.len()) {
        (true, 1) => Ok(hits.into_iter().next().unwrap()),
        (true, 0) | (false, _) => Err(format!("没有这个待机：{id}（不带参数调 hub_schedule 看全部）")),
        _ => Err(format!("编号 {id} 对得上好几个，多写几位")),
    }
}

/// 往后数：错过的整步跳过，下一次落在现在之后、保持原来的节拍。返回 (下一次, 错过几次)。
fn advance(next_at: i64, every: i64, at: i64) -> (i64, i64) {
    let step = every * MINUTE;
    let missed = (at - next_at).max(0) / step;
    (next_at + (missed + 1) * step, missed)
}

fn describe(connection: &Connection, watch: &Watch) -> Value {
    json!({
        "id": short(&watch.id),
        "note": watch.note,
        "status": watch.status,
        "next": (watch.status == "active").then(|| local(connection, watch.next_at)),
        "every_minutes": watch.every_minutes,
        "until": watch.until_at.map(|at| local(connection, at)),
        "check": watch.check_cmd,
        "once": watch.once,
        "member": watch.member_name,
        "fired": watch.fired,
        "last": watch.last_at.map(|at| local(connection, at)),
        "last_result": watch.last_result,
    })
}

pub fn call(connection: &Connection, ctx: &Context, args: &Value) -> Result<Value> {
    let given = |name: &str| !args[name].is_null();
    let flag = |name: &str| args[name].as_bool() == Some(true);
    let Some(id) = optional_arg(args, "id") else {
        if ["note", "at", "in_minutes", "every_minutes", "check", "member", "until"].iter().any(|name| given(name)) {
            return create(connection, ctx, args);
        }
        let watches: Vec<Value> = list(connection, &ctx.room).iter().map(|watch| describe(connection, watch)).collect();
        return Ok(json!({"now": local(connection, now()), "watches": watches}));
    };
    let watch = load(connection, &ctx.room, &id)?;
    if flag("remove") {
        if watch.status != "done" {
            connection.execute("UPDATE brain_watches SET status='done',last_result='主脑撤掉了',updated_at=?1 WHERE id=?2", params![now(), watch.id]).map_err(|_| "待机删除失败")?;
            post(connection, &ctx.room, &format!("主脑撤掉了待机：{}", brief(&watch.note)));
        }
        return Ok(json!({"removed": short(&watch.id)}));
    }
    update(connection, ctx, watch, args)
}

/// 新挂或改出来的一份设定，存之前统一检查。
struct Plan {
    note: String,
    every: Option<i64>,
    next_at: i64,
    until_at: Option<i64>,
    check: Option<String>,
    member: Option<String>,
    once: bool,
}

fn validate(connection: &Connection, ctx: &Context, plan: &Plan) -> Result<()> {
    if let Some(every) = plan.every {
        let floor = if plan.check.is_some() { CHECK_FLOOR } else { TIMER_FLOOR };
        if every < floor {
            return Err(if plan.check.is_some() {
                format!("带 check 的最密 {CHECK_FLOOR} 分钟一次")
            } else {
                format!("纯定时最密 {TIMER_FLOOR} 分钟一次（每次都会叫醒人、花额度）；要盯得更勤就配 check，只在有变化时叫醒")
            });
        }
    } else if plan.check.is_some() {
        return Err("check 要配 every_minutes：隔多久跑一次".into());
    }
    if plan.next_at > now() + MAX_AHEAD * MINUTE {
        return Err("最远只能挂到 60 天以后".into());
    }
    if plan.until_at.is_some_and(|until| until <= plan.next_at) {
        return Err("until 要晚于第一次".into());
    }
    if plan.check.is_some() && crate::project_brain::tier(connection, &ctx.room) != "full" {
        return Err("项目权限档不是全权，同席不替主脑在项目里跑命令；改用 member，让成员到点去看".into());
    }
    Ok(())
}

fn create(connection: &Connection, ctx: &Context, args: &Value) -> Result<Value> {
    let note = text_arg(args, "note", 2000)?;
    let active: i64 = connection
        .query_row("SELECT COUNT(*) FROM brain_watches WHERE room=?1 AND status<>'done'", [&ctx.room], |row| row.get(0))
        .map_err(|_| "待机读取失败")?;
    if active >= MAX_ACTIVE {
        return Err(format!("本项目已经挂了 {active} 个待机，先撤掉用不着的（remove=true）"));
    }
    let every = minutes_arg(args, "every_minutes", 1, MAX_EVERY)?;
    let check = optional_arg(args, "check").map(|text| text.chars().take(2000).collect::<String>());
    let next_at = match (optional_arg(args, "at"), minutes_arg(args, "in_minutes", 1, MAX_AHEAD)?) {
        (Some(_), Some(_)) => return Err("at 和 in_minutes 只给一个".into()),
        (Some(at), None) => parse_local(connection, &at)?,
        (None, Some(minutes)) => now() + minutes * MINUTE,
        // 带 check 的马上跑一次记下现状；纯定时的一个周期后第一次。
        (None, None) => match every {
            Some(_) if check.is_some() => now(),
            Some(every) => now() + every * MINUTE,
            None => return Err("要给 at、in_minutes 或 every_minutes：什么时候叫你".into()),
        },
    };
    if next_at < now() - MINUTE {
        return Err(format!("{} 已经过去了", local(connection, next_at)));
    }
    let member = match optional_arg(args, "member") {
        Some(member) => Some(crate::project_brain::find_member(connection, ctx, &member)?.0),
        None => None,
    };
    let plan = Plan {
        note,
        until_at: optional_arg(args, "until").map(|text| parse_local(connection, &text)).transpose()?,
        once: every.is_none() || args["once"].as_bool() == Some(true),
        every,
        next_at,
        check,
        member,
    };
    validate(connection, ctx, &plan)?;
    let (baseline, check_now) = plan.check.as_deref().map(|command| first_run(command, &ctx.root)).unzip();
    let baseline = baseline.flatten();
    let mut next_at = plan.next_at;
    if baseline.is_some() && next_at <= now() {
        next_at = now() + plan.every.unwrap_or(0) * MINUTE;
    }
    let id = Uuid::new_v4().to_string();
    connection
        .execute(
            "INSERT INTO brain_watches(id,room,note,every_minutes,next_at,until_at,check_cmd,member,once,status,baseline,last_at,last_result,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'active',?10,?12,?13,?11,?11)",
            params![id, ctx.room, plan.note, plan.every, next_at, plan.until_at, plan.check, plan.member, plan.once, baseline, now(), baseline.as_ref().map(|_| now()), baseline.as_ref().map(|_| "记下了现状，之后有变化才叫醒")],
        )
        .map_err(|_| "待机保存失败")?;
    post(connection, &ctx.room, &format!("主脑挂了待机（{}）：{}", cadence(connection, plan.every, next_at, plan.check.is_some()), brief(&plan.note)));
    Ok(json!({
        "id": short(&id),
        "next": local(connection, next_at),
        "check_now": check_now,
        "note": if plan.check.is_some() { "到点在项目目录跑 check，和 check_now 里记下的现状不一样才叫你；不用等也不用查" } else { "到点会作为新消息发给你；不用等也不用查" },
    }))
}

/// 挂上或改了 check 时当场跑一次记成现状，结果当场回给主脑：命令写错了（没输出、报错、没对上）每次都一样，会被当成「没变化」永远不叫醒。
fn first_run(command: &str, root: &Path) -> (Option<String>, String) {
    match run_check(command, root) {
        Ok(text) => {
            let empty = text.lines().skip(1).all(|line| line.trim().is_empty());
            let hint = if empty { "没有任何输出，之后只有退出码变了才会叫醒你；多半是命令没对上。" } else { "" };
            let seen = format!("check 现在跑出来是：\n```\n{text}\n```\n{hint}这就是记下的现状，之后和它不一样才叫醒你。不是你要盯的东西就马上用 id 改 check。");
            (Some(text), seen)
        }
        Err(error) => (None, format!("check 现在跑不成：{error}。到点会再跑，连续 {CHECK_FAILURES} 次跑不成就暂停；命令写错了就马上用 id 改。")),
    }
}

fn update(connection: &Connection, ctx: &Context, watch: Watch, args: &Value) -> Result<Value> {
    let flag = |name: &str| args[name].as_bool() == Some(true);
    if flag("pause") && flag("resume") {
        return Err("pause 和 resume 只给一个".into());
    }
    let check = match &args["check"] {
        Value::Null => watch.check_cmd.clone(),
        value => value.as_str().map(str::trim).filter(|text| !text.is_empty()).map(|text| text.chars().take(2000).collect()),
    };
    let every = match &args["every_minutes"] {
        Value::Null => watch.every_minutes,
        _ => minutes_arg(args, "every_minutes", 1, MAX_EVERY)?,
    };
    let retimed = !args["at"].is_null() || !args["in_minutes"].is_null() || !args["every_minutes"].is_null();
    let mut next_at = match (optional_arg(args, "at"), minutes_arg(args, "in_minutes", 1, MAX_AHEAD)?) {
        (Some(_), Some(_)) => return Err("at 和 in_minutes 只给一个".into()),
        (Some(at), None) => parse_local(connection, &at)?,
        (None, Some(minutes)) => now() + minutes * MINUTE,
        (None, None) if retimed => now() + every.unwrap_or(0) * MINUTE,
        (None, None) => watch.next_at,
    };
    let mut status = watch.status.clone();
    if flag("pause") {
        status = "paused".into();
    }
    if flag("resume") {
        status = "active".into();
    }
    // 停着的时候过了点：重复的跳到下一拍，一次性的马上提醒。
    if status == "active" && next_at <= now() && !retimed {
        next_at = every.map_or(now(), |every| advance(next_at, every, now()).0);
    }
    let member = match &args["member"] {
        Value::Null => watch.member.clone(),
        value => match value.as_str().map(str::trim).filter(|text| !text.is_empty()) {
            Some(member) => Some(crate::project_brain::find_member(connection, ctx, member)?.0),
            None => None,
        },
    };
    let plan = Plan {
        note: if args["note"].is_null() { watch.note.clone() } else { text_arg(args, "note", 2000)? },
        until_at: match &args["until"] {
            Value::Null => watch.until_at,
            value => value.as_str().map(str::trim).filter(|text| !text.is_empty()).map(|text| parse_local(connection, text)).transpose()?,
        },
        once: if args["once"].is_null() { watch.once && every.is_some() || every.is_none() } else { flag("once") || every.is_none() },
        every,
        next_at,
        check,
        member,
    };
    if status == "done" && !flag("resume") {
        return Err("这个待机已经结束了；要接着用就 resume=true 并给新的时间".into());
    }
    validate(connection, ctx, &plan)?;
    let (baseline, check_now) = match &plan.check {
        Some(command) if plan.check != watch.check_cmd => {
            let (baseline, seen) = first_run(command, &ctx.root);
            (baseline, Some(seen))
        }
        _ => (watch.baseline.clone().filter(|_| plan.check.is_some()), None),
    };
    connection
        .execute(
            "UPDATE brain_watches SET note=?1,every_minutes=?2,next_at=?3,until_at=?4,check_cmd=?5,member=?6,once=?7,status=?8,baseline=?9,failures=0,updated_at=?10 WHERE id=?11",
            params![plan.note, plan.every, plan.next_at, plan.until_at, plan.check, plan.member, plan.once, status, baseline, now(), watch.id],
        )
        .map_err(|_| "待机保存失败")?;
    let mut updated = describe(connection, &load(connection, &ctx.room, &watch.id)?);
    if let Some(seen) = check_now {
        updated["check_now"] = seen.into();
    }
    Ok(updated)
}

/// 用户在派活页点的：暂停、继续、现在跑一次、删除。
pub fn user_action(connection: &Connection, room: &str, id: &str, action: &str) -> Result<()> {
    let watch = load(connection, room, id)?;
    let sql = match action {
        "pause" => "UPDATE brain_watches SET status='paused',updated_at=?2 WHERE id=?1",
        "resume" => "UPDATE brain_watches SET status='active',failures=0,updated_at=?2,next_at=MAX(next_at,?2) WHERE id=?1",
        "run" => "UPDATE brain_watches SET status='active',next_at=?2,updated_at=?2 WHERE id=?1",
        "remove" => "DELETE FROM brain_watches WHERE id=?1 AND ?2>0",
        _ => return Err("不认识的操作".into()),
    };
    if watch.status == "done" && action != "remove" {
        return Err("这个待机已经结束了".into());
    }
    connection.execute(sql, params![watch.id, now()]).map_err(|_| "待机保存失败")?;
    // 用户动了主脑挂的东西，告诉主脑一声，免得它以为还在。
    let what = match action {
        "pause" => "暂停了",
        "resume" => "恢复了",
        "run" => "让它现在跑一次",
        _ => "删掉了",
    };
    let _ = connection.execute(
        "INSERT INTO brain_inbox(id,room,content,created_at,sender) VALUES(?1,?2,?3,?4,'user')",
        params![Uuid::new_v4().to_string(), room, format!("【同席】用户在派活页把待机 {}「{}」{what}。", short(&watch.id), brief(&watch.note)), now()],
    );
    Ok(())
}

fn cadence(connection: &Connection, every: Option<i64>, next_at: i64, check: bool) -> String {
    let first = local(connection, next_at);
    match every {
        Some(every) if check => format!("每 {} 看一次有没有变化", minutes(every)),
        Some(every) => format!("{first} 起每 {}", minutes(every)),
        None => first,
    }
}

fn minutes(value: i64) -> String {
    match value {
        value if value % (24 * 60) == 0 => format!("{} 天", value / (24 * 60)),
        value if value % 60 == 0 => format!("{} 小时", value / 60),
        value => format!("{value} 分钟"),
    }
}

fn brief(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > 80 {
        format!("{}…", line.chars().take(80).collect::<String>())
    } else {
        line
    }
}

fn post(connection: &Connection, room: &str, content: &str) {
    if let Ok(project) = connection.query_row("SELECT project_id FROM conversation_projects WHERE conversation_id=?1", [room], |row| row.get::<_, String>(0)) {
        let _ = team_message(connection, &project, room, None, content);
    }
}

// ---------- 主程序这一侧：到点的待机 ----------

pub fn start(app: tauri::AppHandle, store: Arc<Mutex<Store>>) {
    std::thread::spawn(move || loop {
        for room in tick(&store) {
            let _ = app.emit("brain-state", room);
        }
        std::thread::sleep(Duration::from_secs(15));
    });
}

/// 跑一遍到点的待机，返回有变化的项目群。跑 check 时不占着数据库锁。
pub fn tick(store: &Arc<Mutex<Store>>) -> Vec<String> {
    let due: Vec<Watch> = {
        let store = store.lock().unwrap();
        let connection = &store.connection;
        let week_ago = now() - 7 * 24 * 60 * MINUTE;
        let _ = connection.execute("DELETE FROM brain_watch_log WHERE at<?1", [week_ago]);
        let _ = connection.execute("DELETE FROM brain_watches WHERE status='done' AND updated_at<?1", [week_ago]);
        connection
            .prepare(&format!("SELECT {COLUMNS} FROM brain_watches WHERE status='active' AND next_at<=?1 ORDER BY next_at"))
            .and_then(|mut query| query.query_map([now()], row(connection))?.collect())
            .unwrap_or_default()
    };
    let mut rooms = Vec::new();
    for watch in due {
        let output = match &watch.check_cmd {
            Some(command) => {
                let root = {
                    let store = store.lock().unwrap();
                    store.conversation_project(&watch.room).ok().flatten().map(|project| project.root)
                };
                Some(root.ok_or_else(|| "项目没绑目录".to_owned()).and_then(|root| run_check(command, Path::new(&root))))
            }
            None => None,
        };
        let store = store.lock().unwrap();
        if fire(&store, &watch, output, now()) {
            rooms.push(watch.room.clone());
        }
    }
    rooms.dedup();
    rooms
}

/// 一个到点的待机该怎么办。output：带 check 的这次跑出来的结果。返回界面要不要刷新。
pub(crate) fn fire(store: &Store, watch: &Watch, output: Option<Result<String>>, at: i64) -> bool {
    let connection = &store.connection;
    let set = |sql: &str, values: &[&dyn rusqlite::ToSql]| {
        let _ = connection.execute(sql, values);
    };
    let id = watch.id.as_str();
    // 跑 check 的工夫里被撤掉或暂停了：这一拍不叫醒，也别在下面把它写回待机中。
    if !connection.query_row("SELECT status='active' FROM brain_watches WHERE id=?1", [id], |row| row.get(0)).unwrap_or(false) {
        return false;
    }
    if watch.until_at.is_some_and(|until| at > until) {
        set("UPDATE brain_watches SET status='done',last_result='到了 until，结束',updated_at=?1 WHERE id=?2", &[&at, &id]);
        post(connection, &watch.room, &format!("待机到了截止时间，已结束：{}", brief(&watch.note)));
        return true;
    }
    let late = at - watch.next_at;
    let (next, missed) = match watch.every_minutes {
        Some(every) => advance(watch.next_at, every, at),
        None => (at, 0),
    };
    // 往后推、不算这一拍：check 的现状也不记，下次再看到变化照样叫醒。
    let defer = |minutes: i64, why: &str| {
        set("UPDATE brain_watches SET next_at=?1,last_result=?2,updated_at=?3 WHERE id=?4", &[&(at + minutes * MINUTE), &why, &at, &id]);
        true
    };
    let mut changed = None;
    if let Some(output) = output {
        match output {
            Err(error) => {
                let failures = watch.failures + 1;
                if failures >= CHECK_FAILURES {
                    set("UPDATE brain_watches SET status='paused',failures=?1,last_at=?2,last_result=?3,updated_at=?2 WHERE id=?4", &[&failures, &at, &format!("check 连续 {failures} 次跑不成，已暂停：{error}"), &id]);
                    inbox(connection, &watch.room, &format!("【待机 {}】check 连续 {failures} 次跑不成，已暂停：{error}\n命令：{}\n修好用 hub_schedule(id=\"{}\", check=…, resume=true)，用不着就 remove=true。", short(id), watch.check_cmd.as_deref().unwrap_or(""), short(id)));
                } else {
                    set("UPDATE brain_watches SET next_at=?1,failures=?2,last_at=?3,last_result=?4,updated_at=?3 WHERE id=?5", &[&next, &failures, &at, &format!("check 没跑成：{error}"), &id]);
                }
                return true;
            }
            Ok(text) if watch.baseline.is_none() => {
                set("UPDATE brain_watches SET next_at=?1,baseline=?2,failures=0,last_at=?3,last_result='记下了现状，之后有变化才叫醒',updated_at=?3 WHERE id=?4", &[&next, &text, &at, &id]);
                // 挂上时当场没跑成，主脑还没见过它的输出：补给它看一眼，命令写错了别一直当成「没变化」。
                inbox(connection, &watch.room, &format!("【待机 {}】check 这回才跑成，记成了现状，之后和它不一样才叫醒你：\n```\n{text}\n```\n这正是要盯的现状就不用管；是命令写错了就用 hub_schedule(id=\"{}\", check=…) 改。", short(id), short(id)));
                return true;
            }
            Ok(text) if watch.baseline.as_deref() == Some(text.as_str()) => {
                set("UPDATE brain_watches SET next_at=?1,failures=0,last_at=?2,last_result=?3,updated_at=?2 WHERE id=?4", &[&next, &at, &NOTHING_NEW, &id]);
                return false;
            }
            Ok(text) => changed = Some(text),
        }
    }
    let count = |sql: &str, key: &str| connection.query_row(sql, params![key, at - 24 * 60 * MINUTE], |row| row.get::<_, i64>(0)).unwrap_or(0);
    let today = count("SELECT COUNT(*) FROM brain_watch_log WHERE watch=?1 AND at>?2", id);
    if today >= WATCH_DAILY {
        set("UPDATE brain_watches SET status='paused',last_result=?1,updated_at=?2 WHERE id=?3", &[&format!("一天叫醒了 {today} 次，已暂停"), &at, &id]);
        inbox(connection, &watch.room, &format!("【待机 {}】「{}」一天里叫醒了 {today} 次，同席先停了它。多半是 check 的输出每次都变（带了时间、计数），改成只打印要盯的东西再 resume=true；用不着就 remove=true。", short(id), brief(&watch.note)));
        post(connection, &watch.room, &format!("待机「{}」一天叫醒 {today} 次，已暂停", brief(&watch.note)));
        return true;
    }
    if count("SELECT COUNT(*) FROM brain_watch_log WHERE room=?1 AND at>?2", &watch.room) >= ROOM_DAILY {
        return defer(30, &format!("本项目一天的待机已经叫醒 {ROOM_DAILY} 次，推迟半小时"));
    }
    // 要叫醒的那位额度快见底了：往后推，等额度回来再叫，别把剩下的额度花在例行查看上。
    let seat = watch.member.clone().or_else(|| crate::project_brain::brain_seat(store, &watch.room).ok());
    if let Some(used) = seat.as_deref().and_then(|seat| five_hour_used(connection, base_agent(seat))) {
        if used >= 95.0 {
            return defer(30, &format!("{} 的 5 小时额度用了 {used:.0}%，推迟半小时", base_agent(seat.as_deref().unwrap_or(""))));
        }
    }
    let mut head = format!("【待机 {}】{}", short(id), watch.note);
    if late > 10 * MINUTE {
        head.push_str(&format!("\n（本该 {} 跑，晚了 {}：同席当时没开着", local(connection, watch.next_at), minutes(late / MINUTE)));
        if missed > 0 {
            head.push_str(&format!("，期间错过 {missed} 次，只补这一次"));
        }
        head.push('）');
    }
    if let Some(text) = &changed {
        head.push_str(&format!("\ncheck 的结果变了：\n```\n{text}\n```"));
    }
    let done = watch.once && (changed.is_some() || watch.check_cmd.is_none()) || watch.every_minutes.is_none() || watch.until_at.is_some_and(|until| next > until);
    let member_chat = watch.member.as_deref().and_then(|seat| seat_chat(connection, &watch.room, seat).map(|chat| (seat, chat)));
    let result = match member_chat {
        Some((seat, chat)) => {
            let busy: bool = connection
                .query_row("SELECT EXISTS(SELECT 1 FROM brain_jobs WHERE chat=?1 AND kind='task' AND status IN ('pending','running'))", [&chat], |row| row.get(0))
                .unwrap_or(true);
            if busy {
                return defer(2, "成员手上有活，过两分钟再派");
            }
            let task = format!(
                "{head}\n\n这是主脑挂的待机，到点同席自动派给你。照上面说的去看、去做；做完照常写「## 交回」。\n没有值得主脑知道的新情况时，「## 交回」下面只写「{NOTHING_NEW}」三个字，主脑就不会被叫醒；有新情况就写清楚是什么、要不要主脑处理。"
            );
            let _ = connection.execute(
                "INSERT INTO brain_jobs(id,room,seat,chat,kind,content,category,watch,status,created_at) VALUES(?1,?2,?3,?4,'task',?5,'待机',?6,'pending',?7)",
                params![Uuid::new_v4().to_string(), watch.room, seat, chat, task, id, at],
            );
            format!("交给 {} 去看", label(connection, &watch.room, seat))
        }
        None => {
            if let Some(seat) = &watch.member {
                head.push_str(&format!("\n（原本交给 {seat}，它已经不在项目里了，改送给你）"));
            }
            head.push_str(&tidy_hint(connection, &watch.room, done.then_some(id)));
            inbox(connection, &watch.room, &head);
            "叫醒了主脑".to_owned()
        }
    };
    let _ = connection.execute("INSERT INTO brain_watch_log(watch,room,at) VALUES(?1,?2,?3)", params![id, watch.room, at]);
    set(
        "UPDATE brain_watches SET next_at=?1,status=?2,baseline=COALESCE(?3,baseline),failures=0,fired=fired+1,last_at=?4,last_result=?5,updated_at=?4 WHERE id=?6",
        &[&next, &if done { "done" } else { "active" }, &changed, &at, &result, &id],
    );
    true
}

/// 项目里某个席位现在的会话（群换绑过项目的只认当前项目）。
fn seat_chat(connection: &Connection, room: &str, seat: &str) -> Option<String> {
    connection
        .query_row(
            "SELECT conversation_id FROM project_agent_chats WHERE group_conversation_id=?1 AND agent_id=?2 AND project_id=(SELECT project_id FROM conversation_projects WHERE conversation_id=?1)",
            params![room, seat],
            |row| row.get(0),
        )
        .ok()
}

fn inbox(connection: &Connection, room: &str, content: &str) {
    let _ = connection.execute(
        "INSERT INTO brain_inbox(id,room,content,created_at,sender) VALUES(?1,?2,?3,?4,'hub')",
        params![Uuid::new_v4().to_string(), room, content, now()],
    );
}

fn five_hour_used(connection: &Connection, agent: &str) -> Option<f64> {
    let text: String = connection.query_row("SELECT value FROM metadata WHERE key='brain_quota'", [], |row| row.get(0)).optional().ok()??;
    serde_json::from_str::<Value>(&text).ok()?[agent]["five_hour"]["used_percent"].as_f64()
}

/// 待机派的活交回了：成员回复里「## 交回」那段只有「无变化」，就记在待机上、不叫醒主脑，返回 true。
pub fn quiet_hand_back(connection: &Connection, job: &str, watch: &str, hand_back: &str) -> bool {
    let body = hand_back.rfind("## 交回").map_or(hand_back, |at| &hand_back[at + "## 交回".len()..]).trim();
    let quiet = body.trim_matches(['。', '.', '！', ' ', '\n', '*', '`', '「', '」']) == NOTHING_NEW;
    let result = if quiet { "成员看过：无变化".to_owned() } else { "成员交回了新情况，已转给主脑".to_owned() };
    let _ = connection.execute("UPDATE brain_watches SET last_result=?1,updated_at=?2 WHERE id=?3", params![result, now(), watch]);
    if quiet {
        let _ = connection.execute("UPDATE brain_jobs SET category=?1 WHERE id=?2", params![QUIET_CATEGORY, job]);
    }
    quiet
}

/// 在项目目录跑 check：PowerShell，60 秒，不弹窗。结果是「exit 退出码」加输出（出错时带上错误输出）。
fn run_check(command: &str, root: &Path) -> Result<String> {
    use std::io::Read;
    // Windows PowerShell 5.1 调外部程序会吞掉参数里的双引号（gh --jq '… "x" …' 就坏了），有 pwsh 就用 pwsh。
    let mut process = std::process::Command::new(crate::cli_open::shell());
    process
        // 管道默认按系统代码页输出，中文会乱码，先切成 UTF-8。
        .args(["-NoProfile", "-NonInteractive", "-Command", &format!("[Console]::OutputEncoding=[Text.Encoding]::UTF8; {command}")])
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        process.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = process.spawn().map_err(|error| format!("启动不了 PowerShell：{error}"))?;
    let read = |mut pipe: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            String::from_utf8_lossy(&bytes).trim().to_owned()
        })
    };
    let stdout = read(Box::new(child.stdout.take().ok_or("拿不到输出")?));
    let stderr = read(Box::new(child.stderr.take().ok_or("拿不到输出")?));
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            break status;
        }
        if started.elapsed() > CHECK_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err("跑了 60 秒还没完".into());
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let (stdout, stderr) = (stdout.join().unwrap_or_default(), stderr.join().unwrap_or_default());
    let code = status.code().unwrap_or(-1);
    let mut text = format!("exit {code}\n{stdout}");
    if code != 0 && !stderr.is_empty() {
        text.push_str(&format!("\n{stderr}"));
    }
    Ok(text.chars().take(OUTPUT_LIMIT).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brain_project(name: &str) -> (std::path::PathBuf, Store, String, String) {
        let base = std::env::temp_dir().join(format!("tongxi-watch-{name}-{}", Uuid::new_v4()));
        let root = base.join("project");
        std::fs::create_dir_all(&root).unwrap();
        let mut store = Store::open(&base.join("data").join("hub.db")).unwrap();
        let room = store.create(name, "group", &["claude-win".into(), "codex-win".into()]).unwrap().id;
        let project = store.register_project(name, root.to_str().unwrap()).unwrap().id;
        store.bind_project(&room, Some(&project)).unwrap();
        let chat = store.ensure_project_agent_chats(&room).unwrap().into_iter().find(|chat| chat.agent_id == "claude-win-brain").unwrap().conversation_id;
        (base, store, room, chat)
    }

    fn only(store: &Store, room: &str) -> Watch {
        list(&store.connection, room).into_iter().next().unwrap()
    }

    fn inbox(store: &Store, room: &str) -> Vec<String> {
        store.connection.prepare("SELECT content FROM brain_inbox WHERE room=?1 ORDER BY created_at,rowid").unwrap().query_map([room], |row| row.get(0)).unwrap().flatten().collect()
    }

    #[test]
    fn check_watch_wakes_only_on_change_and_member_watch_delegates() {
        let (base, store, room, chat) = brain_project("待机");
        let call = |args: Value| crate::project_brain::call(&store.connection, &chat, "hub_schedule", args);
        assert!(call(json!({"note":"看看","every_minutes":10})).unwrap_err().contains("30"), "纯定时不能太密");
        assert!(call(json!({"note":"看看","check":"git status"})).unwrap_err().contains("every_minutes"));
        let made = call(json!({"note":"CI 挂了就修","every_minutes":5,"check":"Write-Output 通过"})).unwrap();
        assert!(made["check_now"].as_str().unwrap().contains("exit 0\n通过"), "挂上就当场跑一次、输出回给主脑：{made}");
        let watch = only(&store, &room);
        assert_eq!(watch.baseline.as_deref(), Some("exit 0\n通过"));
        assert!(watch.next_at > now(), "当场跑过了，下一次隔一个周期");

        let at = now();
        fire(&store, &watch, Some(Ok("exit 0\n通过".into())), at + 5 * MINUTE);
        assert!(inbox(&store, &room).is_empty(), "没变化不叫醒");
        // 额度快见底：推迟，不记新现状，下次照样能看到变化。
        store.connection.execute("INSERT INTO metadata(key,value) VALUES('brain_quota',?1)", [json!({"claude-win":{"five_hour":{"used_percent":97}}}).to_string()]).unwrap();
        fire(&store, &only(&store, &room), Some(Ok("exit 1\n失败".into())), at + 10 * MINUTE);
        assert!(inbox(&store, &room).is_empty() && only(&store, &room).baseline.as_deref() == Some("exit 0\n通过"));
        store.connection.execute("DELETE FROM metadata WHERE key='brain_quota'", []).unwrap();
        fire(&store, &only(&store, &room), Some(Ok("exit 1\n失败".into())), at + 15 * MINUTE);
        let woke = inbox(&store, &room).pop().expect("变了就叫醒主脑");
        assert!(woke.contains("CI 挂了就修") && woke.contains("exit 1"));
        assert_eq!(only(&store, &room).fired, 1);

        // 关了两个多小时再开：只补一次，注明错过几次，下一次落回原节拍。
        let watch = only(&store, &room);
        fire(&store, &watch, Some(Ok("exit 0\n通过".into())), watch.next_at + 125 * MINUTE);
        let late = inbox(&store, &room).pop().unwrap();
        assert!(late.contains("晚了") && late.contains("错过 25 次"), "{late}");
        assert_eq!((only(&store, &room).next_at - watch.next_at) % (5 * MINUTE), 0);

        // 用户在派活页撤掉，主脑收到一声。
        user_action(&store.connection, &room, short(&watch.id), "remove").unwrap();
        assert!(list(&store.connection, &room).is_empty());
        assert!(inbox(&store, &room).pop().unwrap().contains("删掉了"));

        // 交给成员的：到点派一件活，成员手上有活就往后推。
        let made = call(json!({"note":"看一眼 issue","every_minutes":60,"member":"codex-win"})).unwrap();
        assert!(made["next"].is_string());
        let watch = only(&store, &room);
        fire(&store, &watch, None, watch.next_at);
        let (job, content): (String, String) = store.connection.query_row("SELECT id,content FROM brain_jobs WHERE watch=?1", [&watch.id], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
        assert!(content.contains("看一眼 issue") && content.contains(NOTHING_NEW));
        let watch = only(&store, &room);
        fire(&store, &watch, None, watch.next_at);
        assert_eq!(only(&store, &room).last_result.as_deref(), Some("成员手上有活，过两分钟再派"));
        assert!(quiet_hand_back(&store.connection, &job, &watch.id, "看了。\n## 交回\n无变化"));
        assert_eq!(only(&store, &room).last_result.as_deref(), Some("成员看过：无变化"));

        // 一次性提醒到点就结束。
        call(json!({"id":short(&watch.id),"remove":true})).unwrap();
        call(json!({"note":"提醒用户续费","in_minutes":30})).unwrap();
        let watch = only(&store, &room);
        fire(&store, &watch, None, watch.next_at);
        assert!(list(&store.connection, &room).is_empty(), "一次性的到点就不再列着");
        assert!(!inbox(&store, &room).pop().unwrap().contains("现在挂着"), "别的都没挂，不用提醒清理");
        assert!(call(json!({})).unwrap()["now"].is_string(), "不带参数列出待机和现在时间");
        drop(store);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn ended_watches_clear_and_the_brain_sees_what_is_left() {
        let (base, store, room, chat) = brain_project("清理");
        let call = |args: Value| crate::project_brain::call(&store.connection, &chat, "hub_schedule", args);
        let add = |args: Value| load(&store.connection, &room, call(args).unwrap()["id"].as_str().unwrap()).unwrap();
        let patrol = add(json!({"note":"每天巡查","every_minutes":1440}));
        let reply = add(json!({"note":"等客户回话","every_minutes":60}));
        call(json!({"id":reply.id,"pause":true})).unwrap();
        let once = add(json!({"note":"提醒续费","in_minutes":30}));

        // 叫醒主脑时带上还挂着的；这次刚结束的不列。
        fire(&store, &once, None, once.next_at);
        let woke = inbox(&store, &room).pop().unwrap();
        let (_, left) = woke.split_once("现在挂着").expect("带上清理提醒");
        assert!(left.contains("每天巡查") && left.contains("已暂停") && left.contains("等客户回话") && !left.contains("提醒续费"), "{woke}");

        // 主脑撤掉的不再列着；跑 check 的工夫里被撤的，这一拍不叫醒、也不复活。
        call(json!({"id":patrol.id,"remove":true})).unwrap();
        let before = inbox(&store, &room).len();
        assert!(!fire(&store, &patrol, None, patrol.next_at));
        assert_eq!((inbox(&store, &room).len(), only(&store, &room).note.as_str()), (before, "等客户回话"));
        // 撤错了按原来的 id 挂得回来。
        call(json!({"id":patrol.id,"resume":true})).unwrap();
        assert_eq!(load(&store.connection, &room, &patrol.id).unwrap().status, "active");

        // 到了 until 的也清掉；结束满 7 天才真删。
        store.connection.execute("UPDATE brain_watches SET until_at=1 WHERE id=?1", [&patrol.id]).unwrap();
        fire(&store, &load(&store.connection, &room, &patrol.id).unwrap(), None, now());
        assert_eq!(only(&store, &room).note, "等客户回话");
        let store = Arc::new(Mutex::new(store));
        let count = || store.lock().unwrap().connection.query_row("SELECT COUNT(*) FROM brain_watches", [], |row| row.get::<_, i64>(0)).unwrap();
        tick(&store);
        assert_eq!(count(), 3);
        store.lock().unwrap().connection.execute("UPDATE brain_watches SET updated_at=1", []).unwrap();
        tick(&store);
        assert_eq!(count(), 1, "没结束的不动");
        drop(store);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn local_times_parse_both_forms() {
        let (base, store, _, _) = brain_project("时间");
        let next = parse_local(&store.connection, "09:00").unwrap();
        assert!(next > now() && next <= now() + 24 * 60 * MINUTE, "钟点取下一次");
        let full = parse_local(&store.connection, "2030-01-02 03:04").unwrap();
        assert_eq!(local(&store.connection, full), "2030-01-02 03:04");
        assert!(parse_local(&store.connection, "明天早上").is_err());
        assert!(parse_local(&store.connection, "25:00").is_err());
        drop(store);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn missed_firings_keep_the_beat() {
        assert_eq!(advance(1_000, 30, 1_000), (1_000 + 30 * MINUTE, 0));
        // 关了 2 小时多：错过 4 拍，下一次落在原节拍上。
        let (next, missed) = advance(0, 30, 130 * MINUTE);
        assert_eq!((next, missed), (150 * MINUTE, 4));
    }

    #[test]
    fn quiet_hand_back_is_only_the_bare_phrase() {
        let store = Store::open(&std::env::temp_dir().join(format!("tongxi-watch-{}", Uuid::new_v4())).join("hub.db")).unwrap();
        for (text, quiet) in [("## 交回\n无变化", true), ("## 交回\n**无变化。**", true), ("## 交回\n无变化，但 CI 挂了", false), ("做了一些事", false)] {
            assert_eq!(quiet_hand_back(&store.connection, "j", "w", text), quiet, "{text}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn check_reports_exit_code_and_chinese_output() {
        let out = run_check("Write-Output '构建通过'; exit 3", &std::env::temp_dir()).unwrap();
        assert_eq!(out, "exit 3\n构建通过");
        // 外部程序拿到的参数里双引号要还在（5.1 会吞掉，gh --jq 就坏了）。
        if crate::cli_open::shell() == "pwsh.exe" {
            let quoted = run_check(r#"pwsh -NoProfile -Command 'Write-Output "a b"'"#, &std::env::temp_dir()).unwrap();
            assert_eq!(quoted, "exit 0\na b");
        }
    }

    #[test]
    fn broken_check_is_shown_to_the_brain_right_away() {
        let (base, store, room, chat) = brain_project("坏命令");
        let call = |args: Value| crate::project_brain::call(&store.connection, &chat, "hub_schedule", args);
        // gh run list --commit 写短 SHA 就是这样：什么都不打印、退出码 0，每次都一样，主脑永远等不到变化。
        let made = call(json!({"note":"CI 出结果","every_minutes":5,"check":"exit 0"})).unwrap();
        assert!(made["check_now"].as_str().unwrap().contains("没有任何输出"), "{made}");
        // 改了 check 当场重跑、换现状。
        let fixed = call(json!({"id":made["id"],"check":"Write-Output 'in_progress'; exit 1"})).unwrap();
        assert!(fixed["check_now"].as_str().unwrap().contains("exit 1\nin_progress"), "{fixed}");
        assert_eq!(only(&store, &room).baseline.as_deref(), Some("exit 1\nin_progress"));
        assert!(call(json!({"id":made["id"],"note":"CI 出结果了"})).unwrap()["check_now"].is_null(), "check 没改不重跑");
        assert!(inbox(&store, &room).is_empty());
        // 当场没跑成的：之后第一次跑成时补给主脑看一眼，再一样就不打扰。
        store.connection.execute("UPDATE brain_watches SET baseline=NULL", []).unwrap();
        fire(&store, &only(&store, &room), Some(Ok("exit 0\n".into())), now());
        assert!(inbox(&store, &room).pop().unwrap().contains("这回才跑成"));
        fire(&store, &only(&store, &room), Some(Ok("exit 0\n".into())), now() + 5 * MINUTE);
        assert_eq!(inbox(&store, &room).len(), 1, "之后一样的输出不再打扰");
        drop(store);
        let _ = std::fs::remove_dir_all(base);
    }
}
