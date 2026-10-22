//! Shared, lease-checked file tools. Neither transport grants native shell access.
use crate::project_store::{base_agent, is_executor, key, protected_relative, task_path};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

type Result<T> = std::result::Result<T, String>;
const MAX_FILE: usize = 1_048_576;

pub struct Broker {
    connection: Connection,
    attempt_id: String,
    backups: PathBuf,
}
struct Scope {
    root: PathBuf,
    files: Vec<String>,
    writable: bool,
    whole_project: bool,
    project_id: String,
    workflow_id: String,
    conversation_id: String,
    agent_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileArg {
    path: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArg {
    path: String,
    content: String,
    expected_sha256: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArg {
    path: String,
    old_text: String,
    new_text: String,
    expected_sha256: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteArg {
    path: String,
    expected_sha256: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentMessageArg {
    to_agent: String,
    content: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BroadcastArg {
    content: String,
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn path_key(path: &str) -> Result<String> {
    let path = protected_relative(path)?;
    Ok(if cfg!(windows) {
        path.to_lowercase()
    } else {
        path
    })
}

// Hard links can refer to files outside the root without changing canonical paths.
fn regular_file(path: &Path) -> Result<File> {
    let metadata = fs::symlink_metadata(path).map_err(|_| "目标文件不可用")?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("工具仅支持普通文件".into());
    }
    let file = File::open(path).map_err(|_| "读取目标文件失败")?;
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::*;
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0
            || info.nNumberOfLinks != 1
            || info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        {
            return Err("不支持链接文件或无法确认文件身份".into());
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if file.metadata().map_err(|_| "读取文件信息失败")?.nlink() != 1 {
            return Err("不支持硬链接文件".into());
        }
    }
    Ok(file)
}
fn bytes(path: &Path) -> Result<Vec<u8>> {
    let file = regular_file(path)?;
    let mut data = Vec::new();
    file.take(MAX_FILE as u64 + 1)
        .read_to_end(&mut data)
        .map_err(|_| "读取目标文件失败")?;
    if data.len() > MAX_FILE {
        return Err("单个文件超过 1 MiB，请缩小任务".into());
    }
    Ok(data)
}
fn plain_parents(root: &Path, relative: &str) -> Result<()> {
    let mut path = root.to_path_buf();
    for part in protected_relative(relative)?.split('/') {
        path.push(part);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                #[cfg(windows)]
                let linked = {
                    use std::os::windows::fs::MetadataExt;
                    metadata.file_attributes() & 0x400 != 0
                };
                #[cfg(not(windows))]
                let linked = metadata.file_type().is_symlink();
                if linked {
                    return Err("任务路径不能经过链接或重解析点".into());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(_) => return Err("无法确认任务路径".into()),
        }
    }
    Ok(())
}

impl Broker {
    /// Opening a helper must never migrate or recover the owner's live database.
    pub fn open(db: &Path, attempt_id: &str) -> Result<Self> {
        Uuid::parse_str(attempt_id).map_err(|_| "项目运行 ID 无效")?;
        if !db.is_absolute() || !db.is_file() {
            return Err("项目数据库不可用".into());
        }
        let connection = Connection::open_with_flags(
            db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| "项目数据库打开失败")?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|_| "项目数据库不可用")?;
        connection
            .execute_batch("PRAGMA foreign_keys=ON;")
            .map_err(|_| "项目数据库不可用")?;
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(|_| "项目数据库版本不可用")?;
        if version != crate::store::SCHEMA_VERSION {
            return Err("项目工具与数据库版本不匹配".into());
        }
        Ok(Self {
            connection,
            attempt_id: attempt_id.into(),
            backups: db.parent().ok_or("数据目录无效")?.join("project-backups"),
        })
    }

    fn scope(connection: &Connection, attempt: &str) -> Result<Scope> {
        let manager: bool = connection
            .query_row(
                "SELECT COALESCE(activity,'')='coordinate' FROM project_attempts WHERE id=?1",
                [attempt],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if manager {
            return Err("管家只负责调度，不开放项目工具".into());
        }
        let row=connection.query_row("SELECT p.root,a.stage,a.task_id,t.files,t.worktree,w.plan,l.root_key,a.agent_id,w.id,w.conversation_id,w.project_id FROM project_attempts a JOIN workflows w ON w.id=a.workflow_id JOIN projects p ON p.id=w.project_id JOIN project_leases l ON l.workflow_id=w.id LEFT JOIN project_tasks t ON t.id=a.task_id WHERE a.id=?1 AND a.status IN ('starting','running') AND w.status IN ('planning','running','reviewing')", [attempt], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,Option<String>>(3)?,r.get::<_,Option<String>>(4)?,r.get::<_,Option<String>>(5)?,r.get::<_,String>(6)?,r.get::<_,String>(7)?,r.get::<_,String>(8)?,r.get::<_,String>(9)?,r.get::<_,String>(10)?))).optional().map_err(|_| "任务授权查询失败")?.ok_or("任务已停止或不再持有项目租约")?;
        let project_root = Path::new(&row.0)
            .canonicalize()
            .map_err(|_| "项目目录不可用")?;
        // 有独立工作树的任务（实现）在自己的工作树里改文件；其它阶段仍用主仓库根。
        let root = match row.4.as_deref() {
            Some(worktree) => PathBuf::from(worktree)
                .canonicalize()
                .map_err(|_| "任务工作树不可用")?,
            None => {
                if key(&project_root) != row.6 {
                    return Err("项目目录身份已经改变".into());
                }
                project_root
            }
        };
        let whole_project: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM coordination_routes WHERE task_id=?1)",
                [&row.2],
                |r| r.get(0),
            )
            .unwrap_or(false);
        let writable = is_executor(&row.7)
            && (whole_project || matches!(row.1.as_str(), "implement" | "repair"));
        let files: Vec<String> = if row.1 == "plan" && row.2.is_none() {
            crate::projects::manifest(&root)?
        } else if let Some(text) = row.3 {
            serde_json::from_str(&text).map_err(|_| "任务范围无效")?
        } else if matches!(row.1.as_str(), "repair" | "review") {
            let plan: crate::project_store::Plan =
                serde_json::from_str(row.5.as_deref().ok_or("任务范围缺失")?)
                    .map_err(|_| "任务范围无效")?;
            plan.tasks.into_iter().flat_map(|t| t.files).collect()
        } else {
            return Err("此阶段不开放文件工具".into());
        };
        Ok(Scope {
            root,
            files,
            writable,
            whole_project,
            project_id: row.10,
            workflow_id: row.8,
            conversation_id: row.9,
            agent_id: row.7,
        })
    }

    pub fn allowed_tool_specs(&self) -> Result<Vec<Value>> {
        let scope = Self::scope(&self.connection, &self.attempt_id)?;
        let mut tools = tool_specs();
        if !scope.writable {
            tools.retain(|tool| {
                matches!(
                    tool["name"].as_str(),
                    Some("hub_list" | "hub_read" | "hub_message" | "hub_inbox" | "hub_broadcast")
                )
            });
        }
        Ok(tools)
    }

    fn target(scope: &Scope, relative: &str, write: bool) -> Result<PathBuf> {
        let wanted = path_key(relative)?;
        if !scope.whole_project
            && !scope
                .files
                .iter()
                .any(|p| path_key(p).is_ok_and(|p| p == wanted))
        {
            return Err("文件未列入当前任务授权范围".into());
        }
        if write && !scope.writable {
            return Err("此阶段只允许读取".into());
        }
        plain_parents(&scope.root, relative)?;
        let target = task_path(&scope.root, relative)?;
        Ok(target)
    }

    pub fn call(&mut self, tool: &str, args: Value) -> Result<Value> {
        // 请教要等调度回答，不能放进下面的事务里等。
        if tool == "hub_ask_advisor" {
            Self::scope(&self.connection, &self.attempt_id)?;
            return crate::advisor::ask(&self.connection, &self.attempt_id, args);
        }
        if tool == "hub_ask_user" {
            Self::scope(&self.connection, &self.attempt_id)?;
            let path: String = self
                .connection
                .query_row(
                    "SELECT file FROM pragma_database_list WHERE name='main'",
                    [],
                    |row| row.get(0),
                )
                .map_err(|_| "问题数据库路径不可用")?;
            let prompt = args["question"].as_str().ok_or("请输入问题")?;
            let choices = args["options"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            return crate::project_questions::post(Path::new(&path),&self.attempt_id,prompt,choices).map(|id|json!({"question_id":id,"awaiting_user":true,"instruction":"问题已发到用户会话。停止当前操作并结束本轮回复，等待用户回答后会从同一项目会话继续。不要猜测答案，也不要批准未验收的任务。"}));
        }
        if tool.starts_with("hub_memory_") {
            let scope = Self::scope(&self.connection, &self.attempt_id)?;
            return crate::project_memory::call(&scope.project_id, base_agent(&scope.agent_id), tool, args);
        }
        // An IMMEDIATE transaction serializes admission with cancellation in the UI.
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| "项目工具当前忙，请重试")?;
        let scope = Self::scope(&tx, &self.attempt_id)?;
        let result = match tool {
            "hub_message" => {
                let arg: AgentMessageArg =
                    serde_json::from_value(args).map_err(|_| "成员消息参数无效")?;
                let recipient = arg.to_agent.trim();
                let content = arg.content.trim();
                if !is_executor(recipient)
                    || recipient == scope.agent_id
                    || content.is_empty()
                    || content.chars().count() > 4000
                    || content
                        .chars()
                        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
                {
                    return Err("收件成员或消息内容无效".into());
                }
                let belongs: bool = tx
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM members WHERE conversation_id=?1 AND agent_id=?2)",
                        params![scope.conversation_id,recipient],
                        |row| row.get(0),
                    )
                    .map_err(|_| "收件成员校验失败")?;
                if !belongs {
                    return Err("收件 Agent 不属于当前项目群".into());
                }
                let message_count: i64 = tx
                    .query_row(
                        "SELECT COUNT(*) FROM project_agent_messages WHERE workflow_id=?1",
                        [&scope.workflow_id],
                        |row| row.get(0),
                    )
                    .map_err(|_| "项目成员消息计数失败")?;
                if message_count >= 200 {
                    return Err("本轮项目成员直发消息已达到 200 条上限".into());
                }
                let id = Uuid::new_v4().to_string();
                let created_at = crate::store::now();
                tx.execute(
                    "INSERT INTO project_agent_messages(id,workflow_id,attempt_id,sender_agent,recipient_agent,content,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![id,scope.workflow_id,self.attempt_id,scope.agent_id,recipient,content,created_at],
                )
                .map_err(|_| "项目成员消息保存失败")?;
                json!({"sent":true,"message_id":id,"to_agent":recipient})
            }
            "hub_inbox" => {
                if args != json!({}) {
                    return Err("hub_inbox 不接收参数".into());
                }
                let mut statement = tx
                    .prepare("SELECT id,sender_agent,content,created_at FROM project_agent_messages WHERE workflow_id=?1 AND recipient_agent=?2 ORDER BY created_at,id LIMIT 100")
                    .map_err(|_| "项目消息收件箱读取失败")?;
                let mut messages = statement
                    .query_map(params![scope.workflow_id,scope.agent_id], |row| {
                        Ok(json!({"id":row.get::<_,String>(0)?,"from_agent":row.get::<_,String>(1)?,"content":row.get::<_,String>(2)?,"created_at":row.get::<_,i64>(3)?}))
                    })
                    .map_err(|_| "项目消息收件箱读取失败")?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|_| "项目消息收件箱读取失败")?;
                drop(statement);
                let mut private = tx.prepare("SELECT id,sender_agent,content,created_at FROM project_team_messages WHERE conversation_id=?1 AND recipient_agent=?2 ORDER BY created_at,id LIMIT 100").map_err(|_| "成员消息读取失败")?;
                messages.extend(private.query_map(params![scope.conversation_id,scope.agent_id], |row| Ok(json!({"id":row.get::<_,String>(0)?,"from_agent":row.get::<_,String>(1)?,"content":row.get::<_,String>(2)?,"created_at":row.get::<_,i64>(3)?}))).map_err(|_| "成员消息读取失败")?.collect::<std::result::Result<Vec<_>,_>>().map_err(|_| "成员消息读取失败")?);
                drop(private);
                let mut statement = tx.prepare("SELECT id,message_kind,sender_agent,content,created_at FROM project_team_messages WHERE conversation_id=?1 AND recipient_agent IS NULL ORDER BY created_at DESC,id DESC LIMIT 100")
                    .map_err(|_| "团队消息读取失败")?;
                let team_updates = statement.query_map([&scope.conversation_id], |row| Ok(json!({
                    "id":row.get::<_,String>(0)?,"kind":row.get::<_,String>(1)?,"from_agent":row.get::<_,Option<String>>(2)?,
                    "content":row.get::<_,String>(3)?,"created_at":row.get::<_,i64>(4)?
                }))).map_err(|_| "团队消息读取失败")?.collect::<std::result::Result<Vec<_>,_>>().map_err(|_|"团队消息读取失败")?;
                json!({"messages":messages,"team_updates":team_updates.into_iter().rev().collect::<Vec<_>>()})
            }
            "hub_broadcast" => {
                let arg: BroadcastArg =
                    serde_json::from_value(args).map_err(|_| "团队消息参数无效")?;
                let content = arg.content.trim();
                if content.is_empty()
                    || content.chars().count() > 4000
                    || content
                        .chars()
                        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
                {
                    return Err("团队消息需为 1–4000 个有效字符".into());
                }
                let count: i64 = tx
                    .query_row(
                        "SELECT COUNT(*) FROM project_team_messages WHERE conversation_id=?1",
                        [&scope.conversation_id],
                        |row| row.get(0),
                    )
                    .map_err(|_| "团队消息计数失败")?;
                if count >= 500 {
                    return Err("本轮团队消息已达到 500 条上限".into());
                }
                let id = Uuid::new_v4().to_string();
                let created_at = crate::store::now();
                tx.execute(
                    "INSERT INTO project_team_messages(id,project_id,conversation_id,workflow_id,attempt_id,sender_agent,content,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                    params![id,scope.project_id,scope.conversation_id,scope.workflow_id,self.attempt_id,scope.agent_id,content,created_at],
                ).map_err(|_| "团队消息保存失败")?;
                tx.execute(
                    "UPDATE workflows SET updated_at=?1 WHERE id=?2",
                    params![created_at, scope.workflow_id],
                )
                .map_err(|_| "团队消息更新时间失败")?;
                json!({"sent":true,"message_id":id,"audience":"project_group"})
            }
            "hub_list" => {
                if args != json!({}) {
                    return Err("hub_list 不接收参数".into());
                }
                let files = if scope.whole_project {
                    crate::projects::manifest(&scope.root)?
                } else {
                    scope.files.clone()
                };
                json!({"files":files,"writable":scope.writable,"scope":if scope.whole_project {"project"}else{"listed_files"}})
            }
            "hub_read" => {
                let arg: FileArg = serde_json::from_value(args).map_err(|_| "读取参数无效")?;
                let target = Self::target(&scope, &arg.path, false)?;
                let data = bytes(&target)?;
                let hash = digest(&data);
                let content = String::from_utf8(data).map_err(|_| "工具仅支持 UTF-8 文本文件")?;
                json!({"path":protected_relative(&arg.path)?,"content":content,"sha256":hash})
            }
            "hub_write" | "hub_edit" | "hub_delete" => {
                let (relative, expected, content) = match tool {
                    "hub_write" => {
                        let arg: WriteArg =
                            serde_json::from_value(args).map_err(|_| "写入参数无效")?;
                        (arg.path, arg.expected_sha256, Some(arg.content))
                    }
                    "hub_delete" => {
                        let arg: DeleteArg =
                            serde_json::from_value(args).map_err(|_| "删除参数无效")?;
                        (arg.path, Some(arg.expected_sha256), None)
                    }
                    _ => {
                        let arg: EditArg =
                            serde_json::from_value(args).map_err(|_| "编辑参数无效")?;
                        let target = Self::target(&scope, &arg.path, true)?;
                        let data = bytes(&target)?;
                        let text =
                            String::from_utf8(data).map_err(|_| "工具仅支持 UTF-8 文本文件")?;
                        if arg.old_text.is_empty() || text.matches(&arg.old_text).count() != 1 {
                            return Err("待替换文本必须恰好出现一次".into());
                        }
                        (
                            arg.path,
                            Some(arg.expected_sha256),
                            Some(text.replacen(&arg.old_text, &arg.new_text, 1)),
                        )
                    }
                };
                let wanted = path_key(&relative)?;
                let relative = scope
                    .files
                    .iter()
                    .find(|path| path_key(path).is_ok_and(|path| path == wanted))
                    .ok_or("文件未列入当前任务授权范围")?;
                let relative = protected_relative(relative)?;
                let target = Self::target(&scope, &relative, true)?;
                let before = if target.exists() {
                    Some(bytes(&target)?)
                } else {
                    None
                };
                let before_hash = before.as_deref().map(digest);
                if expected != before_hash {
                    return Err("文件已变化，请重新读取后再修改；新文件须使用空哈希".into());
                }
                if tool == "hub_delete" && before.is_none() {
                    return Err("待删除文件不存在".into());
                }
                if content.as_ref().is_some_and(|s| s.len() > MAX_FILE) {
                    return Err("单个文件超过 1 MiB，请缩小任务".into());
                }
                let after_hash = content.as_ref().map(|s| digest(s.as_bytes()));
                let backup_dir = self.backups.join(&self.attempt_id);
                fs::create_dir_all(&backup_dir).map_err(|_| "修改前备份目录创建失败")?;
                let base = backup_dir.join(digest(relative.as_bytes()));
                if !base.with_extension("json").exists() {
                    if let Some(data) = before.as_ref() {
                        let mut backup = OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(base.with_extension("bin"))
                            .map_err(|_| "修改前备份失败")?;
                        backup
                            .write_all(data)
                            .and_then(|_| backup.sync_all())
                            .map_err(|_| "修改前备份失败")?;
                    }
                    let mut journal = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(base.with_extension("json"))
                        .map_err(|_| "修改前记录失败")?;
                    serde_json::to_writer(&mut journal,&json!({"path":relative,"before_hash":before_hash,"existed":before.is_some()})).map_err(|_| "修改前记录失败")?;
                    journal.sync_all().map_err(|_| "修改前记录失败")?;
                }
                if let Some(text) = content {
                    let parent = target.parent().ok_or("目标路径无效")?;
                    fs::create_dir_all(parent).map_err(|_| "目标目录创建失败")?;
                    Self::target(&scope, &relative, true)?;
                    let temp = parent.join(format!(".agent-hub-{}.tmp", Uuid::new_v4()));
                    let write_result = (|| -> Result<()> {
                        let mut file = OpenOptions::new()
                            .create_new(true)
                            .write(true)
                            .open(&temp)
                            .map_err(|_| "临时文件创建失败")?;
                        file.write_all(text.as_bytes())
                            .and_then(|_| file.sync_all())
                            .map_err(|_| "临时文件写入失败")?;
                        fs::rename(&temp, &target).map_err(|_| "原子替换文件失败".to_string())
                    })();
                    if write_result.is_err() {
                        let _ = fs::remove_file(&temp);
                    }
                    write_result?;
                } else {
                    fs::remove_file(&target).map_err(|_| "删除目标文件失败")?;
                }
                tx.execute("INSERT INTO project_changes(attempt_id,path,operation,before_hash,after_hash) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(attempt_id,path) DO UPDATE SET operation=excluded.operation,after_hash=excluded.after_hash",params![self.attempt_id,relative,if after_hash.is_some(){"write"}else{"delete"},before_hash,after_hash]).map_err(|_| "记录项目改动失败，修改前备份已保留")?;
                json!({"path":relative,"sha256":after_hash,"saved":true})
            }
            _ => return Err("不支持的项目工具".into()),
        };
        tx.commit()
            .map_err(|_| "项目工具记录失败，修改前备份已保留")?;
        Ok(result)
    }
}

pub fn tool_specs() -> Vec<Value> {
    let string = json!({"type":"string"});
    let nullable_hash = json!({"type":["string","null"],"description":"Current SHA256 from hub_read; null only for a new file."});
    let definitions=[
        ("hub_ask_user","Ask the user a clarification question and wait for their reply. The question is shown in the team conversation; the same native turn continues after the answer.",json!({"question":string,"options":{"type":"array","items":string}}),vec!["question"]),
        ("hub_ask_advisor","Consult the project coordinator (a strong model) about a key decision you are unsure of in your current task: a design trade-off, a hard bug you cannot locate, how to approach a tricky part. Write the question clearly, say what you already tried, and put the relevant code excerpts or errors in context. Blocks until the answer arrives (usually within a minute). The number of calls per task is limited by the coordinator; do not use it for small things.",json!({"question":string,"context":string}),vec!["question"]),
        ("hub_memory_search","Search this project's memory: the shared team pool plus your own private notes (scope: all | shared | private, default all). Use before work to recall decisions and known pitfalls.",json!({"query":string,"scope":{"type":"string","enum":["all","shared","private"]}}),vec!["query"]),
        ("hub_memory_write","Save a concise note. scope=private (default) keeps it to yourself; scope=shared publishes it to every member of this project. Shared notes can only be updated by their author. Do not store private chats, raw reasoning or credentials.",json!({"name":string,"content":string,"scope":{"type":"string","enum":["private","shared"]}}),vec!["name","content"]),
        ("hub_memory_read","Read a note URI from this project's shared pool or your own private notes.",json!({"uri":string}),vec!["uri"]),
        ("hub_memory_list","List note URIs in this project's shared pool and/or your private notes (scope: all | shared | private).",json!({"scope":{"type":"string","enum":["all","shared","private"]}}),vec![]),
        ("hub_memory_share","Choose one of your private notes to share with the whole project team. The private original is kept; the shared copy records you as author and its source. Optional name renames the shared copy; optional note adds a short preface.",json!({"uri":string,"name":string,"note":string}),vec!["uri"]),
        ("hub_list","List files explicitly authorized for the current task.",json!({}),vec![]),
        ("hub_read","Read one authorized UTF-8 file and its SHA256.",json!({"path":string}),vec!["path"]),
        ("hub_message","Send a direct message to another project agent in this group. The recipient can read it from their project inbox.",json!({"to_agent":string,"content":string}),vec!["to_agent","content"]),
        ("hub_inbox","Read direct messages sent to you by other agents in this project workflow.",json!({}),vec![]),
        ("hub_broadcast","Post a short message for the user and all agents in the shared project room. Only publish information the team should see; keep reasoning and tool details in your own agent session.",json!({"content":string}),vec!["content"]),
        ("hub_write","Create or replace one authorized UTF-8 file. Existing files require the current SHA256.",json!({"path":string,"content":string,"expected_sha256":nullable_hash}),vec!["path","content","expected_sha256"]),
        ("hub_edit","Replace exactly one occurrence in an authorized file, guarded by its SHA256.",json!({"path":string,"old_text":string,"new_text":string,"expected_sha256":string}),vec!["path","old_text","new_text","expected_sha256"]),
        ("hub_delete","Delete one authorized file, guarded by its SHA256. The previous file is backed up.",json!({"path":string,"expected_sha256":string}),vec!["path","expected_sha256"]),
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
        project_chat_tool_specs()
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

fn project_chat_tool_specs() -> Vec<Value> {
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
    let (project, group, agent) = chat_context(connection, conversation_id)?;
    let active: Option<String> = connection.query_row("SELECT a.id FROM project_attempts a JOIN workflows w ON w.id=a.workflow_id WHERE w.project_id=?1 AND w.conversation_id=?2 AND a.agent_id=?3 AND COALESCE(a.activity,'')!='coordinate' AND a.status IN ('starting','running') AND w.status IN ('planning','running','reviewing') ORDER BY a.rowid DESC LIMIT 1", params![project,group,agent], |row|row.get(0)).optional().map_err(|_|"当前成员任务读取失败")?;
    if let Some(attempt) = active {
        let path: String = connection
            .query_row(
                "SELECT file FROM pragma_database_list WHERE name='main'",
                [],
                |row| row.get(0),
            )
            .map_err(|_| "项目工具数据库路径不可用")?;
        if !path.is_empty() {
            return Broker::open(Path::new(&path), &attempt)?.call(name, args);
        }
    }
    match name {
        "hub_memory_search" | "hub_memory_write" | "hub_memory_read" | "hub_memory_list"
        | "hub_memory_share" => crate::project_memory::call(&project, base_agent(&agent), name, args),
        "hub_broadcast" => {
            let arg: BroadcastArg = serde_json::from_value(args).map_err(|_| "团队消息参数无效")?;
            chat_broadcast(connection, conversation_id, &arg.content)
        }
        "hub_message" => {
            let arg: AgentMessageArg =
                serde_json::from_value(args).map_err(|_| "成员消息参数无效")?;
            let recipient = arg.to_agent.trim();
            let content = arg.content.trim();
            if !is_executor(recipient)
                || recipient == agent
                || content.is_empty()
                || content.chars().count() > 4000
                || content
                    .chars()
                    .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
            {
                return Err("收件成员或消息内容无效".into());
            }
            let belongs: bool = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM members WHERE conversation_id=?1 AND agent_id=?2)",
                    params![group, recipient],
                    |row| row.get(0),
                )
                .map_err(|_| "收件成员校验失败")?;
            if !belongs {
                return Err("收件 Agent 不属于当前项目群".into());
            }
            let count: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM project_team_messages WHERE conversation_id=?1",
                    [&group],
                    |row| row.get(0),
                )
                .map_err(|_| "成员消息计数失败")?;
            if count >= 500 {
                return Err("项目消息已达到500条上限".into());
            }
            let id = Uuid::new_v4().to_string();
            connection.execute("INSERT INTO project_team_messages(id,project_id,conversation_id,sender_agent,recipient_agent,message_kind,content,created_at) VALUES(?1,?2,?3,?4,?5,'agent',?6,?7)", params![id,project,group,agent,recipient,content,crate::store::now()]).map_err(|_| "成员消息保存失败")?;
            Ok(json!({"sent":true,"message_id":id,"to_agent":recipient,"audience":"project_agent"}))
        }
        "hub_inbox" => {
            if args != json!({}) {
                return Err("hub_inbox 不接收参数".into());
            }
            let mut query = connection.prepare("SELECT id,sender_agent,content,created_at FROM project_team_messages WHERE conversation_id=?1 AND recipient_agent=?2 UNION ALL SELECT m.id,m.sender_agent,m.content,m.created_at FROM project_agent_messages m JOIN workflows w ON w.id=m.workflow_id WHERE w.conversation_id=?1 AND m.recipient_agent=?2 ORDER BY created_at,id LIMIT 100").map_err(|_| "成员收件箱不可用")?;
            let messages = query.query_map(params![group,agent], |row| Ok(json!({"id":row.get::<_,String>(0)?,"from_agent":row.get::<_,String>(1)?,"content":row.get::<_,String>(2)?,"created_at":row.get::<_,i64>(3)?}))).map_err(|_| "成员收件箱读取失败")?.collect::<std::result::Result<Vec<_>,_>>().map_err(|_| "成员收件箱读取失败")?;
            let mut query = connection.prepare("SELECT id,sender_agent,message_kind,content,created_at FROM project_team_messages WHERE conversation_id=?1 AND recipient_agent IS NULL ORDER BY created_at DESC,id DESC LIMIT 100").map_err(|_| "团队消息不可用")?;
            let updates = query.query_map([&group], |row| Ok(json!({"id":row.get::<_,String>(0)?,"from_agent":row.get::<_,Option<String>>(1)?,"kind":row.get::<_,String>(2)?,"content":row.get::<_,String>(3)?,"created_at":row.get::<_,i64>(4)?}))).map_err(|_| "团队消息读取失败")?.collect::<std::result::Result<Vec<_>,_>>().map_err(|_| "团队消息读取失败")?;
            Ok(
                json!({"messages":messages,"team_updates":updates.into_iter().rev().collect::<Vec<_>>()}),
            )
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

#[cfg(test)]
pub fn broadcast_from_project_chat(
    db: &Path,
    conversation_id: &str,
    content: &str,
) -> Result<Value> {
    call_from_project_chat(
        db,
        conversation_id,
        "hub_broadcast",
        json!({"content":content}),
    )
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
            params![group, agent],
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
                json!({"result":{"tools":if linked {project_chat_tool_specs()} else {tool_specs().into_iter().filter(|tool|tool["name"]=="hub_ask_user").collect::<Vec<_>>()}}})
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

/// Minimal stdio MCP transport used only by dedicated project harness sessions.
pub fn serve(db: &Path, attempt: &str) -> Result<()> {
    use std::io::BufRead;
    let mut broker = Broker::open(db, attempt)?;
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    let mut initialized = false;
    // Read one bounded line at a time; never echo malformed input or native errors.
    let mut input = stdin.lock();
    loop {
        let mut line = String::new();
        let count = std::io::Read::by_ref(&mut input)
            .take((MAX_FILE * 7) as u64)
            .read_line(&mut line)
            .map_err(|_| "项目工具输入中断")?;
        if count == 0 {
            break;
        }
        if !line.ends_with('\n') {
            return Err("项目工具请求过长或不完整".into());
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
                json!({"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"agent-hub-project-tools","version":env!("CARGO_PKG_VERSION")}}})
            }
            "ping" => json!({"result":{}}),
            "tools/list" if initialized => match broker.allowed_tool_specs() {
                Ok(tools) => json!({"result":{"tools":tools}}),
                Err(_) => json!({"error":{"code":-32001,"message":"项目授权范围不可用"}}),
            },
            "tools/call" if initialized => {
                let result = broker.call(
                    value["params"]["name"].as_str().unwrap_or(""),
                    value["params"]["arguments"].clone(),
                );
                let (text, error) = match result {
                    Ok(v) => (v.to_string(), false),
                    Err(e) => (e, true),
                };
                json!({"result":{"content":[{"type":"text","text":text}],"isError":error}})
            }
            _ => json!({"error":{"code":-32601,"message":"Unsupported project tool request"}}),
        };
        let mut response = response;
        response["id"] = id.clone();
        response["jsonrpc"] = json!("2.0");
        serde_json::to_writer(&mut stdout, &response).map_err(|_| "项目工具输出中断")?;
        stdout
            .write_all(b"\n")
            .and_then(|_| stdout.flush())
            .map_err(|_| "项目工具输出中断")?;
    }
    Ok(())
}

#[cfg(test)]
mod agent_message_tests {
    use super::*;
    use crate::project_store::Roles;

    #[test]
    fn extra_seats_pass_the_membership_check_as_their_base_agent() {
        let base = std::env::temp_dir().join(format!("hub-seat-tools-{}", Uuid::new_v4()));
        let root = base.join("project");
        std::fs::create_dir_all(&root).unwrap();
        let mut store = crate::store::Store::open(&base.join("data").join("hub.db")).unwrap();
        let room = store.create("项目群", "group", &["codex-win".into()]).unwrap();
        let project = store.register_project("测试项目", root.to_str().unwrap()).unwrap();
        store.bind_project(&room.id, Some(&project.id)).unwrap();
        let seat = store.add_project_seat(&room.id, "codex-win", None).unwrap();
        let chats = store.ensure_project_agent_chats(&room.id).unwrap();
        let chat = chats.iter().find(|chat| chat.agent_id == seat).unwrap();
        assert_eq!(chat_context(&store.connection, &chat.conversation_id).unwrap().2, seat);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn project_agents_can_direct_message_each_other_through_the_scoped_inbox() {
        let base = std::env::temp_dir().join(format!("hub-agent-messages-{}", Uuid::new_v4()));
        let root = base.join("project");
        std::fs::create_dir_all(&root).unwrap();
        let mut store = crate::store::Store::open(&base.join("data").join("hub.db")).unwrap();
        let room = store
            .create(
                "项目群",
                "group",
                &["codex-win".into(), "dsh-win".into(), "hermes-win".into()],
            )
            .unwrap();
        let project = store
            .register_project("测试项目", root.to_str().unwrap())
            .unwrap();
        store.bind_project(&room.id, Some(&project.id)).unwrap();
        let chats = store.ensure_project_agent_chats(&room.id).unwrap();
        let codex_chat = chats
            .iter()
            .find(|chat| chat.agent_id == "codex-win")
            .unwrap();
        let mcp = project_chat_mcp_server(&store.path, &codex_chat.conversation_id)
            .unwrap()
            .unwrap();
        assert_eq!(mcp["args"][0], "--project-chat-tools");
        assert_eq!(
            broadcast_from_project_chat(
                &store.path,
                &codex_chat.conversation_id,
                "Codex 分享团队结论"
            )
            .unwrap()["sent"],
            true
        );
        assert_eq!(store.team_messages(&room.id).unwrap().len(), 1);
        let hermes_chat = chats
            .iter()
            .find(|chat| chat.agent_id == "hermes-win")
            .unwrap();
        let dsh_chat = chats
            .iter()
            .find(|chat| chat.agent_id == "dsh-win")
            .unwrap();
        let direct = call_from_project_chat(
            &store.path,
            &codex_chat.conversation_id,
            "hub_message",
            json!({"to_agent":"hermes-win","content":"只给验收成员的项目消息"}),
        )
        .unwrap();
        assert_eq!(direct["audience"], "project_agent");
        let tools = project_chat_dynamic_tools(&store.path, &codex_chat.conversation_id).unwrap();
        assert!(["hub_memory_list", "hub_memory_share"]
            .iter()
            .all(|name| tools.iter().any(|tool| tool["name"] == *name)));
        assert_eq!(tools.len(), 15);
        assert_eq!(store.team_messages(&room.id).unwrap().len(), 1);
        assert_eq!(
            store
                .detail(&hermes_chat.conversation_id)
                .unwrap()
                .project_agent_inbox
                .len(),
            1
        );
        assert!(store
            .detail(&dsh_chat.conversation_id)
            .unwrap()
            .project_agent_inbox
            .is_empty());
        let inbox = call_from_project_chat(
            &store.path,
            &hermes_chat.conversation_id,
            "hub_inbox",
            json!({}),
        )
        .unwrap();
        assert_eq!(inbox["messages"].as_array().unwrap().len(), 1);
        assert_eq!(inbox["team_updates"].as_array().unwrap().len(), 1);
        assert!(call_from_project_chat(
            &store.path,
            &codex_chat.conversation_id,
            "hub_message",
            json!({"to_agent":"albion-wsl","content":"不能加入项目"})
        )
        .is_err());
        assert!(call_from_project_chat(
            &store.path,
            &codex_chat.conversation_id,
            "hub_message",
            json!({"to_agent":"codex-win","content":"不能给自己"})
        )
        .is_err());
        let (workflow, _) = store
            .start_workflow(&room.id, &Uuid::new_v4().to_string(), "实现一个功能")
            .unwrap();
        assert!(store.acquire_project(&workflow.id).unwrap());
        let attempt = store
            .begin_attempt(
                &workflow.id,
                None,
                &Roles::defaults().plan.agent,
                "plan",
                None,
                None,
            )
            .unwrap();

        let mut sender = Broker::open(&store.path, &attempt.id).unwrap();
        let tools = sender.allowed_tool_specs().unwrap();
        assert!(tools.iter().any(|tool| tool["name"] == "hub_message"));
        assert!(tools.iter().any(|tool| tool["name"] == "hub_inbox"));
        assert!(tools.iter().any(|tool| tool["name"] == "hub_broadcast"));
        let sent = sender
            .call(
                "hub_message",
                json!({"to_agent":"dsh-win","content":"请优先检查这个函数的边界情况。"}),
            )
            .unwrap();
        assert_eq!(sent["sent"], true);
        let published = sender
            .call(
                "hub_broadcast",
                json!({"content":"我会把最终进展发在团队会话中。"}),
            )
            .unwrap();
        assert_eq!(published["audience"], "project_group");
        let inbox = sender.call("hub_inbox", json!({})).unwrap();
        assert_eq!(inbox["team_updates"].as_array().unwrap().len(), 2);
        assert_eq!(inbox["team_updates"][0]["kind"], "agent");
        assert!(sender
            .call(
                "hub_message",
                json!({"to_agent":"albion-wsl","content":"项目消息"}),
            )
            .is_err());
        drop(sender);

        store
            .connection
            .execute(
                "UPDATE project_attempts SET agent_id='dsh-win' WHERE id=?1",
                [&attempt.id],
            )
            .unwrap();
        let mut recipient = Broker::open(&store.path, &attempt.id).unwrap();
        let inbox = recipient.call("hub_inbox", json!({})).unwrap();
        assert_eq!(inbox["messages"].as_array().unwrap().len(), 1);
        assert_eq!(inbox["messages"][0]["from_agent"], "codex-win");
        assert!(inbox["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("边界情况"));
        drop(recipient);
        let messages = store.workflow(&workflow.id).unwrap().agent_messages;
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].sender_agent, "codex-win");
        assert_eq!(messages[0].recipient_agent, "dsh-win");
        let team_messages = store.team_messages(&room.id).unwrap();
        assert_eq!(team_messages.len(), 2);
        assert!(team_messages
            .iter()
            .any(|message| message.content.contains("最终进展")));
        drop(store);
        let _ = std::fs::remove_dir_all(base);
    }
}
