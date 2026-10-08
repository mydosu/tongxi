//! Shared, lease-checked file tools. Neither transport grants native shell access.
use crate::project_store::{key, protected_relative, task_path, EXECUTOR_AGENTS};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
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
        if version != 11 {
            return Err("项目工具与数据库版本不匹配".into());
        }
        Ok(Self {
            connection,
            attempt_id: attempt_id.into(),
            backups: db.parent().ok_or("数据目录无效")?.join("project-backups"),
        })
    }

    fn scope(connection: &Connection, attempt: &str) -> Result<Scope> {
        let row=connection.query_row("SELECT p.root,a.stage,a.task_id,t.files,t.worktree,w.plan,l.root_key,a.agent_id FROM project_attempts a JOIN workflows w ON w.id=a.workflow_id JOIN projects p ON p.id=w.project_id JOIN project_leases l ON l.workflow_id=w.id LEFT JOIN project_tasks t ON t.id=a.task_id WHERE a.id=?1 AND a.status IN ('starting','running') AND w.status IN ('planning','running','reviewing')", [attempt], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,Option<String>>(3)?,r.get::<_,Option<String>>(4)?,r.get::<_,Option<String>>(5)?,r.get::<_,String>(6)?,r.get::<_,String>(7)?))).optional().map_err(|_| "任务授权查询失败")?.ok_or("任务已停止或不再持有项目租约")?;
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
        let writable = EXECUTOR_AGENTS.contains(&row.7.as_str())
            && matches!(row.1.as_str(), "implement" | "repair");
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
        })
    }

    pub fn allowed_tool_specs(&self) -> Result<Vec<Value>> {
        let scope = Self::scope(&self.connection, &self.attempt_id)?;
        let mut tools = tool_specs();
        if !scope.writable {
            tools.retain(|tool| matches!(tool["name"].as_str(), Some("hub_list" | "hub_read")));
        }
        Ok(tools)
    }

    fn target(scope: &Scope, relative: &str, write: bool) -> Result<PathBuf> {
        let wanted = path_key(relative)?;
        if !scope
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
        // An IMMEDIATE transaction serializes admission with cancellation in the UI.
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| "项目工具当前忙，请重试")?;
        let scope = Self::scope(&tx, &self.attempt_id)?;
        let result = match tool {
            "hub_list" => {
                if args != json!({}) {
                    return Err("hub_list 不接收参数".into());
                }
                json!({"files":scope.files,"writable":scope.writable})
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
        ("hub_list","List files explicitly authorized for the current task.",json!({}),vec![]),
        ("hub_read","Read one authorized UTF-8 file and its SHA256.",json!({"path":string}),vec!["path"]),
        ("hub_write","Create or replace one authorized UTF-8 file. Existing files require the current SHA256.",json!({"path":string,"content":string,"expected_sha256":nullable_hash}),vec!["path","content","expected_sha256"]),
        ("hub_edit","Replace exactly one occurrence in an authorized file, guarded by its SHA256.",json!({"path":string,"old_text":string,"new_text":string,"expected_sha256":string}),vec!["path","old_text","new_text","expected_sha256"]),
        ("hub_delete","Delete one authorized file, guarded by its SHA256. The previous file is backed up.",json!({"path":string,"expected_sha256":string}),vec!["path","expected_sha256"]),
    ];
    definitions.into_iter().map(|(name,description,properties,required)|json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false}})).collect()
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
