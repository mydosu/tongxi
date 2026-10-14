//! Git 工作树管理：每个实现任务在独立工作树里开发，全部验收通过后由主仓库一次性合并。
//!
//! 安全默认（硬规则）：
//!  · 原仓库存在未提交改动时拒绝创建新工作树，避免把未完成的改动混进合并基线；
//!    协作派任务前由 `checkpoint` 以管家身份先把这些改动单独提交（凭据与生成目录除外），
//!    任务因此不会被未提交改动挡住；
//!  · 合并只在全部任务通过验收之后做一次，任何一次冲突都立即停下并保留现场，
//!    不强制推送、不丢弃任何一边、也不自动 abort。
//!
//! 惯例：工作树建在调用方给的 `worktree_root`（同席数据目录下的隔离目录，不塞进用户项目）里，
//! 目录名用任务名、分支名用 `hub/<任务>`。
use crate::process_scope::ProcessScope;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, String>;

/// 单条 git 命令的超时上限：超时即终止整棵子进程树，绝不让工作流卡死。
const GIT_TIMEOUT: Duration = Duration::from_secs(180);
/// 只保留必要事实，界面不需要完整命令回显。
const OUTPUT_CAP: usize = 16_384;

/// git 可执行文件：`AGENT_HUB_GIT_EXE` 覆盖优先，否则在 PATH 里找（Windows 上是 git.exe）。
fn discover() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("AGENT_HUB_GIT_EXE") {
        let path = PathBuf::from(value);
        return (path.is_absolute() && path.is_file()).then_some(path);
    }
    let name = if cfg!(windows) { "git.exe" } else { "git" };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|directory| directory.join(name))
        .find(|path| path.is_file())
}

fn capture(
    mut reader: impl Read + Send + 'static,
    buffer: Arc<Mutex<Vec<u8>>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        while let Ok(count) = reader.read(&mut chunk) {
            if count == 0 {
                break;
            }
            let mut data = buffer.lock().unwrap();
            let remaining = OUTPUT_CAP.saturating_sub(data.len());
            data.extend_from_slice(&chunk[..count.min(remaining)]);
            // 截断后继续排空，避免子进程写满管道后阻塞。
        }
    })
}

/// 跑一条 git 命令，返回裁剪后的 stdout；失败只给自有中文文案加退出码，绝不透传 stderr。
/// 超时或 `cancelled()` 命中即终止整棵子进程树。
fn run(
    exe: &Path,
    root: &Path,
    args: &[&str],
    timeout: Duration,
    cancelled: impl Fn() -> bool,
) -> Result<String> {
    let mut command = Command::new(exe);
    command
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command
        .spawn()
        .map_err(|_| "无法启动 git，请检查 PATH 或 AGENT_HUB_GIT_EXE".to_string())?;
    let scope = match ProcessScope::attach(&child) {
        Ok(scope) => scope,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err("git 进程生命周期绑定失败".into());
        }
    };
    let out = Arc::new(Mutex::new(Vec::new()));
    let err = Arc::new(Mutex::new(Vec::new()));
    let out_thread = capture(child.stdout.take().unwrap(), out.clone());
    let err_thread = capture(child.stderr.take().unwrap(), err.clone());
    let started = Instant::now();
    let mut failure = None;
    let status = loop {
        if cancelled() {
            failure = Some("git 命令已取消".to_string());
        } else if started.elapsed() >= timeout {
            failure = Some("git 命令超时".to_string());
        }
        if failure.is_some() {
            scope.terminate();
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => std::thread::sleep(Duration::from_millis(40)),
            Err(_) => {
                failure = Some("git 进程状态读取失败".to_string());
                scope.terminate();
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    // 先收掉 JobObject 再等读取线程：否则残留孙进程可能一直握着管道句柄。
    drop(scope);
    let _ = out_thread.join();
    let _ = err_thread.join();
    if let Some(reason) = failure {
        return Err(reason);
    }
    let status = status.ok_or_else(|| "git 命令未完成".to_string())?;
    if !status.success() {
        return Err(format!(
            "git 命令未成功完成（退出码 {}）",
            status
                .code()
                .map_or_else(|| "-".to_string(), |code| code.to_string())
        ));
    }
    let text = String::from_utf8_lossy(&out.lock().unwrap())
        .trim()
        .to_string();
    Ok(text)
}

/// 默认超时、不取消地跑一条 git 命令。
///
/// 提示：接入 `projects::Runtime` 调度时（下一批）改用 `run`，传入该工作流的
/// `native.cancelled(id)` 与剩余 deadline，git 调用就能随整体一起被打断。
fn git(root: &Path, args: &[&str]) -> Result<String> {
    let exe = discover().ok_or("未找到 git 可执行文件，请检查 PATH 或 AGENT_HUB_GIT_EXE")?;
    run(&exe, root, args, GIT_TIMEOUT, || false)
}

/// root 本身就是一个 git 仓库的顶层工作树。
///
/// 刻意要求 `root` 就是工作树顶层：嵌套在更大仓库里的子目录不算——否则会把整棵外层仓库
/// 当成项目仓库来建工作树/合并，既会误判外层仓库的未提交改动，也会污染项目外的文件。
pub fn is_repo(root: &Path) -> bool {
    let Ok(top) = git(root, &["rev-parse", "--show-toplevel"]) else {
        return false;
    };
    let Ok(root) = root.canonicalize() else {
        return false;
    };
    crate::project_store::key(Path::new(&top)) == crate::project_store::key(&root)
}

/// Initialize a local project repository for a newly bound folder without staging secrets,
/// generated output, nested repositories, links, or files outside the selected root.
pub fn initialize_safe_repository(root: &Path) -> Result<()> {
    let root = root
        .canonicalize()
        .map_err(|_| "项目目录不可用".to_string())?;
    if !root.is_dir() || root.parent() == Some(root.as_path()) {
        return Err("不能在文件系统根目录创建项目仓库".into());
    }
    let exe = discover().ok_or("未找到 git 可执行文件，请检查 PATH 或 AGENT_HUB_GIT_EXE")?;
    let existing_top = git(&root, &["rev-parse", "--show-toplevel"]).ok();
    match existing_top {
        Some(top)
            if crate::project_store::key(Path::new(&top)) == crate::project_store::key(&root) =>
        {
            if git(&root, &["rev-parse", "--verify", "HEAD"]).is_ok() {
                return Ok(());
            }
        }
        Some(_) => return Err("项目目录位于其他 Git 仓库中，请选择仓库顶层目录".into()),
        None => {
            git(&root, &["init", "-q"])?;
        }
    }
    let nested = git(&root, &["rev-parse", "--show-toplevel"])?;
    if crate::project_store::key(Path::new(&nested)) != crate::project_store::key(&root) {
        return Err("无法在所选项目目录建立独立 Git 仓库".into());
    }

    fn collect(
        root: &Path,
        path: &Path,
        files: &mut Vec<String>,
        excluded: &mut Vec<String>,
    ) -> Result<()> {
        let mut entries = std::fs::read_dir(path)
            .map_err(|_| "无法扫描项目目录".to_string())?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| "项目目录扫描中断".to_string())?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .map_err(|_| "项目目录越界".to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            let metadata =
                std::fs::symlink_metadata(&path).map_err(|_| "无法检查项目目录项".to_string())?;
            #[cfg(windows)]
            let linked = {
                use std::os::windows::fs::MetadataExt;
                metadata.file_attributes() & 0x400 != 0
            };
            #[cfg(not(windows))]
            let linked = metadata.file_type().is_symlink();
            if linked {
                excluded.push(relative);
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if metadata.is_dir() {
                if is_excluded_dir(&name) {
                    excluded.push(format!("{relative}/"));
                    continue;
                }
                collect(root, &path, files, excluded)?;
            } else if metadata.is_file() {
                if crate::project_store::protected_relative(&relative).is_ok()
                    && !is_secret_filename(&relative)
                {
                    files.push(relative);
                } else {
                    excluded.push(relative);
                }
            }
        }
        Ok(())
    }

    let mut files = Vec::new();
    let mut excluded = Vec::new();
    collect(&root, &root, &mut files, &mut excluded)?;
    // Keep ignored credentials/generated folders out of every subsequent worktree too.
    let exclude = root.join(".git").join("info").join("exclude");
    let mut exclude_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&exclude)
        .map_err(|_| "无法保护项目中的凭据与生成目录".to_string())?;
    for path in excluded {
        use std::io::Write as _;
        writeln!(exclude_file, "/{path}")
            .map_err(|_| "无法保护项目中的凭据与生成目录".to_string())?;
    }
    drop(exclude_file);

    for batch in files.chunks(128) {
        let mut args = vec!["add".to_string(), "--".to_string()];
        args.extend(batch.iter().cloned());
        let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
        run(&exe, &root, &refs, GIT_TIMEOUT, || false)?;
    }
    run(
        &exe,
        &root,
        &[
            "-c",
            "user.name=agent-hub",
            "-c",
            "user.email=hub@local",
            "commit",
            "--allow-empty",
            "--no-verify",
            "-q",
            "-m",
            "Initial safe project baseline",
        ],
        GIT_TIMEOUT,
        || false,
    )?;
    if !git(&root, &["status", "--porcelain"])?.is_empty() {
        return Err("安全文件基线已创建，但项目仍有未跟踪改动，请检查 .git/info/exclude".into());
    }
    Ok(())
}

/// 不进项目提交的目录：凭据配置与生成/依赖目录（安全基线与开工前提交共用）。
fn is_excluded_dir(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        ".git"
            | ".codex"
            | ".hermes"
            | ".ssh"
            | ".aws"
            | ".azure"
            | ".kube"
            | "target"
            | "dist"
            | "node_modules"
            | ".venv"
            | "venv"
            | "vendor"
            | ".cache"
            | "__pycache__"
    )
}

fn is_link(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| {
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            metadata.file_attributes() & 0x400 != 0
        }
        #[cfg(not(windows))]
        {
            metadata.file_type().is_symlink()
        }
    })
}

/// 原仓库是否卡在合并/变基中途：返回给管家汇报的一句说明（含冲突文件），正常返回 `None`。
pub fn midway_state(root: &Path) -> Result<Option<String>> {
    let git_dir = PathBuf::from(git(root, &["rev-parse", "--absolute-git-dir"])?.trim());
    let operation =
        if git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists() {
            Some("变基中途")
        } else if git_dir.join("MERGE_HEAD").exists() {
            Some("合并中途")
        } else if git_dir.join("CHERRY_PICK_HEAD").exists() {
            Some("拣选提交中途")
        } else {
            None
        };
    let mut conflicts = git(root, &["diff", "--name-only", "--diff-filter=U", "-z"])?
        .split('\0')
        .filter(|file| !file.is_empty())
        .map(String::from)
        .collect::<Vec<_>>();
    if operation.is_none() && conflicts.is_empty() {
        return Ok(None);
    }
    conflicts.truncate(20);
    let mut text = format!("原仓库处于{}", operation.unwrap_or("未解决的冲突状态"));
    if !conflicts.is_empty() {
        text.push_str(&format!("，冲突文件：{}", conflicts.join("、")));
    }
    Ok(Some(text))
}

/// `branch` 是否已经并入当前分支（冲突处理后用来核实，而不是相信成员的口头汇报）。
pub fn merged(root: &Path, branch: &str) -> bool {
    git(root, &["merge-base", "--is-ancestor", branch, "HEAD"]).is_ok()
}

/// 原仓库当前的未提交改动（路径集合，含未跟踪文件；Python 字节码缓存除外）。
/// 任务前后各取一次，用来发现成员绕过任务工作树、直接改了项目根目录的情况。
pub fn root_changes(root: &Path) -> Result<std::collections::BTreeSet<String>> {
    let status = git(
        root,
        &["status", "--porcelain", "-z", "--untracked-files=all"],
    )?;
    Ok(status
        .split(' ')
        // 记录形如 "XY 路径"；整段输出首尾空白会被裁掉，所以不按固定列取，而在状态码后的空格处切开。
        .filter_map(|entry| {
            entry
                .trim_start()
                .split_once(' ')
                .map(|(_, path)| path.to_owned())
        })
        .filter(|path| !path.split('/').any(|part| part == "__pycache__"))
        .collect())
}

/// 多个任务可能同时开工：开工前提交在同一进程内串行，避免 git index.lock 冲突。
static CHECKPOINT: Mutex<()> = Mutex::new(());

/// 开工前提交原仓库里的未提交改动，返回给团队看的一句说明；仓库本来就干净返回 `None`。
///
/// 已跟踪文件的改动全部提交；新增的未跟踪文件沿用安全基线的过滤——凭据文件、生成/依赖目录、
/// 链接与受保护路径不提交，改为写进 `.git/info/exclude`，以后也不会进入任务工作树。
/// 仓库正处于合并或变基中途时拒绝自动提交，交给用户处理。
pub fn checkpoint(root: &Path) -> Result<Option<String>> {
    if !is_repo(root) {
        return Err("目标目录不是 git 仓库".into());
    }
    let _serial = CHECKPOINT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let git_dir = PathBuf::from(git(root, &["rev-parse", "--absolute-git-dir"])?.trim());
    if let Some(problem) = midway_state(root)? {
        return Err(format!("{problem}，无法自动提交"));
    }
    let tracked = if git(root, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_ok() {
        git(root, &["diff", "HEAD", "--name-only", "--no-renames", "-z"])?
    } else {
        git(
            root,
            &["diff", "--cached", "--name-only", "--no-renames", "-z"],
        )?
    };
    let untracked = git(root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    let mut files = tracked
        .split('\0')
        .filter(|file| !file.is_empty())
        .map(String::from)
        .collect::<Vec<_>>();
    let mut excluded = Vec::new();
    for file in untracked.split('\0').filter(|file| !file.is_empty()) {
        let parts = file.split('/').collect::<Vec<_>>();
        let generated = parts[..parts.len() - 1]
            .iter()
            .position(|part| is_excluded_dir(part));
        if let Some(index) = generated {
            excluded.push(format!("{}/", parts[..=index].join("/")));
        } else if is_link(&root.join(file))
            || is_secret_filename(file)
            || crate::project_store::protected_relative(file).is_err()
        {
            excluded.push(file.to_owned());
        } else {
            files.push(file.to_owned());
        }
    }
    excluded.sort();
    excluded.dedup();
    if !excluded.is_empty() {
        use std::io::Write as _;
        std::fs::create_dir_all(git_dir.join("info"))
            .map_err(|_| "无法保护项目中的凭据与生成目录".to_string())?;
        let mut exclude = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(git_dir.join("info").join("exclude"))
            .map_err(|_| "无法保护项目中的凭据与生成目录".to_string())?;
        for path in &excluded {
            writeln!(exclude, "/{path}")
                .map_err(|_| "无法保护项目中的凭据与生成目录".to_string())?;
        }
    }
    let skipped = (!excluded.is_empty())
        .then(|| format!("{} 个凭据或生成文件未提交，已加入本地忽略", excluded.len()));
    if files.is_empty() {
        return Ok(skipped);
    }
    let exe = discover().ok_or("未找到 git 可执行文件，请检查 PATH 或 AGENT_HUB_GIT_EXE")?;
    for batch in files.chunks(128) {
        let mut args = vec!["add", "-A", "--"];
        args.extend(batch.iter().map(String::as_str));
        run(&exe, root, &args, GIT_TIMEOUT, || false)?;
    }
    run(
        &exe,
        root,
        &[
            "-c",
            "user.name=agent-hub",
            "-c",
            "user.email=hub@local",
            "commit",
            "--no-verify",
            "-q",
            "-m",
            "同席管家：开始任务前提交原仓库中未提交的改动",
        ],
        GIT_TIMEOUT,
        || false,
    )?;
    let head = git(root, &["rev-parse", "--short", "HEAD"])?;
    let mut summary = format!("已提交 {} 个文件（提交 {}）", files.len(), head.trim());
    if let Some(skipped) = skipped {
        summary.push('；');
        summary.push_str(&skipped);
    }
    Ok(Some(summary))
}

fn is_secret_filename(name: &str) -> bool {
    name.rsplit('/').next().is_some_and(|leaf| {
        leaf == ".npmrc"
            || leaf == ".pypirc"
            || leaf == ".netrc"
            || leaf.starts_with("credentials.")
            || leaf.starts_with("auth.")
            || leaf == "secrets.json"
            || leaf == "token.json"
            || leaf.ends_with(".pem")
            || leaf.ends_with(".key")
            || leaf.ends_with(".p12")
            || leaf.ends_with(".pfx")
            || leaf.ends_with(".jks")
            || leaf.ends_with(".keystore")
    })
}

/// 在 `worktree_root/name` 建一个新工作树，检出到新分支 `branch`，返回工作树路径。
///
/// 安全默认：原仓库有未提交改动时拒绝开始。目标目录或分支已存在一律明确报错，不静默复用。
pub fn create(root: &Path, worktree_root: &Path, name: &str, branch: &str) -> Result<PathBuf> {
    if !is_repo(root) {
        return Err("目标目录不是 git 仓库".into());
    }
    let status = git(
        root,
        &["status", "--porcelain", "-z", "--untracked-files=all"],
    )?;
    let source_dirty = status.len() >= OUTPUT_CAP.saturating_sub(8)
        || status
            .split('\0')
            .filter(|entry| !entry.is_empty())
            .any(|entry| {
                let Some(path) = entry.strip_prefix("?? ") else {
                    return true;
                };
                !(path.split('/').any(|part| part == "__pycache__")
                    && (path.ends_with(".pyc") || path.ends_with(".pyo")))
            });
    if source_dirty {
        return Err("原仓库存在未提交改动，请先提交或暂存后再开始任务".into());
    }
    let path = worktree_root.join(name);
    if path.exists() {
        return Err(format!("工作树目录已存在：{name}"));
    }
    let reference = format!("refs/heads/{branch}");
    if git(root, &["rev-parse", "--verify", "--quiet", &reference]).is_ok() {
        return Err(format!("分支已存在：{branch}"));
    }
    std::fs::create_dir_all(worktree_root).map_err(|_| "工作树根目录创建失败".to_string())?;
    let path_text = path.to_string_lossy().to_string();
    git(root, &["worktree", "add", &path_text, "-b", branch])?;
    Ok(path)
}

/// Reattach a completed task branch after a failed workflow released its worktree directory.
pub fn attach(root: &Path, worktree_root: &Path, name: &str, branch: &str) -> Result<PathBuf> {
    if !is_repo(root) {
        return Err("目标目录不是 git 仓库".into());
    }
    let path = worktree_root.join(name);
    if path.exists() {
        return Err(format!("工作树目录已存在：{name}"));
    }
    let reference = format!("refs/heads/{branch}");
    git(root, &["rev-parse", "--verify", "--quiet", &reference])
        .map_err(|_| "已完成任务分支不存在，不能从原方案继续".to_string())?;
    std::fs::create_dir_all(worktree_root).map_err(|_| "工作树根目录创建失败".to_string())?;
    let path_text = path.to_string_lossy().to_string();
    git(root, &["worktree", "add", &path_text, branch])?;
    Ok(path)
}

/// Reuse an interrupted task's existing worktree without discarding uncommitted partial work.
pub fn reuse(root: &Path, worktree_root: &Path, path: &Path, branch: &str) -> Result<PathBuf> {
    let root = root.canonicalize().map_err(|_| "项目目录不可用")?;
    let worktree_root = worktree_root
        .canonicalize()
        .map_err(|_| "工作树目录不可用")?;
    let path = path.canonicalize().map_err(|_| "中断任务的工作树不存在")?;
    if path.parent() != Some(worktree_root.as_path()) {
        return Err("中断任务工作树不在同席授权目录内".into());
    }
    let top = git(&path, &["rev-parse", "--show-toplevel"])?;
    let top = PathBuf::from(top.trim())
        .canonicalize()
        .map_err(|_| "工作树身份无效")?;
    if top != path {
        return Err("中断任务工作树身份不匹配".into());
    }
    let current = git(&path, &["branch", "--show-current"])?;
    if current.trim() != branch {
        return Err("中断任务工作树分支不匹配".into());
    }
    let listed = git(&root, &["worktree", "list", "--porcelain"])?;
    if !listed.lines().any(|line| {
        line.strip_prefix("worktree ").is_some_and(|entry| {
            Path::new(entry)
                .canonicalize()
                .is_ok_and(|entry| entry == path)
        })
    }) {
        return Err("Git 未登记此中断任务工作树".into());
    }
    Ok(path)
}

/// 删掉一个工作树（`--force`：任务分支上的改动已在别处保留，这里只清理现场）。
pub fn remove(root: &Path, path: &Path) -> Result<()> {
    if !is_repo(root) {
        return Err("目标目录不是 git 仓库".into());
    }
    let path_text = path.to_string_lossy().to_string();
    match git(root, &["worktree", "remove", "--force", &path_text]) {
        Ok(_) => Ok(()),
        Err(error) => {
            // Git can finish removal before reporting a Windows cleanup error,
            // and native tools may already have removed a completed worktree.
            let listed = git(root, &["worktree", "list", "--porcelain"])?;
            let target = path_text.replace('\\', "/");
            let registered = listed
                .lines()
                .filter_map(|line| line.strip_prefix("worktree "))
                .any(|entry| {
                    let entry = entry.replace('\\', "/");
                    if cfg!(windows) {
                        entry.eq_ignore_ascii_case(&target)
                    } else {
                        entry == target
                    }
                });
            if !path.exists() && !registered {
                Ok(())
            } else {
                Err(error)
            }
        }
    }
}

/// 提交工作树里**任务授权文件**的改动，供随后合并进主分支。
///
/// 只提交 `files` 里的改动：检查/构建产生的产物（如 `__pycache__`）不进任务分支，
/// 否则多个任务会因同名产物在合并时冲突。显式给定提交身份，不依赖仓库或全局 git 配置；
/// 没有任何改动时返回 Ok（不产生空提交）。
pub fn commit(path: &Path, message: &str, files: &[String]) -> Result<String> {
    let exe = discover().ok_or("未找到 git 可执行文件，请检查 PATH 或 AGENT_HUB_GIT_EXE")?;
    let mut authorized = Vec::new();
    for file in files {
        let tracked = run(
            &exe,
            path,
            &["ls-files", "--error-unmatch", "--", file],
            GIT_TIMEOUT,
            || false,
        )
        .is_ok();
        if path.join(file).exists() || tracked {
            authorized.push(file.as_str());
        }
    }
    if authorized.is_empty() {
        return Ok(message.to_owned());
    }
    let mut args = vec!["add", "-A", "--"];
    args.extend(authorized);
    run(&exe, path, &args, GIT_TIMEOUT, || false)?;
    if run(
        &exe,
        path,
        &["diff", "--cached", "--name-only"],
        GIT_TIMEOUT,
        || false,
    )?
    .is_empty()
    {
        return Ok(message.to_owned());
    }
    run(
        &exe,
        path,
        &[
            "-c",
            "user.name=agent-hub",
            "-c",
            "user.email=hub@local",
            "commit",
            "--no-verify",
            "-q",
            "-m",
            message,
        ],
        GIT_TIMEOUT,
        || false,
    )?;
    Ok(message.to_owned())
}

/// Dynamic tasks commit their actual changes, retaining the existing protected-path rules.
pub fn coordinated_files(path: &Path) -> Result<Vec<String>> {
    let staged = git(path, &["diff", "--cached", "--name-only", "-z"])?;
    for file in staged.split('\0').filter(|file| !file.is_empty()) {
        crate::project_store::protected_relative(file)
            .map_err(|_| "原生任务暂存了受保护路径，不能自动提交")?;
    }
    let tracked = git(path, &["diff", "HEAD", "--name-only", "-z"])?;
    let untracked = git(path, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    let mut files = tracked
        .split('\0')
        .chain(untracked.split('\0'))
        .filter(|file| !file.is_empty())
        .filter_map(|file| crate::project_store::protected_relative(file).ok())
        .collect::<Vec<_>>();
    files.sort();
    files.dedup();
    Ok(files)
}

/// 把 `branch` 合并进 root 当前分支，返回一句摘要。
///
/// 只在全部任务通过验收之后调用一次。冲突必须停下、保留现场、返回明确原因；
/// 绝不强推、绝不丢弃任何一边、绝不自动 abort。
pub fn merge(root: &Path, branch: &str) -> Result<String> {
    if !is_repo(root) {
        return Err("目标目录不是 git 仓库".into());
    }
    // 主仓库里未提交的改动碰到了任务分支改过的文件时，git 会拒绝合并（怕覆盖本地改动）。
    // 常见情形是成员先在主目录试改、再把同样的改动提交进任务分支：这些文件内容和合并结果逐字相同，
    // 就先把它们暂存起来再合并，合并成功后内容本就一致，丢掉暂存；合并失败就原样放回。
    // 内容不一致的不碰，明确列出文件，等人处理。
    let local: Vec<String> = git(root, &["diff", "--name-only", "HEAD"])?
        .lines()
        .chain(git(root, &["ls-files", "--others", "--exclude-standard"])?.lines())
        .map(String::from)
        .collect();
    let range = format!("HEAD...{branch}");
    let touched: Vec<String> = git(root, &["diff", "--name-only", &range])?
        .lines()
        .map(String::from)
        .collect();
    let overlap: Vec<String> = local
        .into_iter()
        .filter(|path| touched.contains(path))
        .collect();
    let mut stashed = false;
    if !overlap.is_empty() {
        let tree = git(root, &["merge-tree", "--write-tree", "HEAD", branch]).ok();
        let differs: Vec<&String> = overlap
            .iter()
            .filter(|path| {
                let ours = git(root, &["hash-object", "--", path.as_str()]).ok();
                let merged = tree
                    .as_ref()
                    .and_then(|tree| git(root, &["rev-parse", &format!("{tree}:{path}")]).ok());
                ours.is_none() || ours != merged
            })
            .collect();
        if !differs.is_empty() {
            let names = differs
                .iter()
                .map(|path| path.as_str())
                .collect::<Vec<_>>()
                .join("、");
            return Err(format!(
                "合并 {branch} 未完成：主仓库有未提交的改动和任务分支改了同样的文件（{names}），内容不一致；请先提交或还原这些改动，再点“继续协作”"
            ));
        }
        let message = format!("tongxi: 合并 {branch} 前暂存与任务分支相同的本地改动");
        let mut args = vec!["stash", "push", "--include-untracked", "-m", &message, "--"];
        args.extend(overlap.iter().map(String::as_str));
        git(root, &args).map_err(|_| {
            format!("合并 {branch} 未完成：主仓库有与任务分支相同的未提交改动，暂存失败")
        })?;
        stashed = true;
    }
    match git(root, &["merge", "--no-edit", branch]) {
        Ok(_) => {
            if stashed {
                let _ = git(root, &["stash", "drop"]);
            }
            Ok(format!("分支 {branch} 已合并进当前分支"))
        }
        Err(error) if stashed => {
            let restored = git(root, &["stash", "pop"]).is_ok();
            Err(format!(
                "合并 {branch} 未完成：{error}；{}",
                if restored {
                    "合并前暂存的本地改动已放回"
                } else {
                    "合并前暂存的本地改动还在 git stash 里，请人工取回"
                }
            ))
        }
        Err(error) => {
            let conflicted = git(root, &["ls-files", "-u"])
                .map(|text| !text.is_empty())
                .unwrap_or(false);
            if conflicted {
                Err(format!(
                    "合并 {branch} 出现冲突，已保留现场（未自动解决，也未回退），请人工处理"
                ))
            } else {
                Err(format!("合并 {branch} 未完成：{error}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn git_ok(exe: &Path, root: &Path, args: &[&str]) -> bool {
        run(exe, root, args, GIT_TIMEOUT, || false).is_ok()
    }

    /// 只在本机真的装了 git 时跑真实操作；没有 git 就跳过，别把环境问题报成功能失败。
    fn git_exe() -> Option<PathBuf> {
        discover()
    }

    fn temp_base() -> PathBuf {
        std::env::temp_dir().join(format!("hub-worktree-{}", Uuid::new_v4()))
    }

    fn init_repo(exe: &Path, repo: &Path) -> bool {
        std::fs::create_dir_all(repo).is_ok()
            && git_ok(exe, repo, &["init", "-q"])
            && git_ok(exe, repo, &["config", "user.email", "hub@test.local"])
            && git_ok(exe, repo, &["config", "user.name", "hub-test"])
            && git_ok(exe, repo, &["config", "commit.gpgsign", "false"])
            && git_ok(exe, repo, &["config", "core.autocrlf", "false"])
            && std::fs::write(repo.join("base.txt"), "base\n").is_ok()
            && git_ok(exe, repo, &["add", "base.txt"])
            && git_ok(exe, repo, &["commit", "-q", "-m", "init"])
    }

    #[test]
    fn checkpoint_commits_dirty_work_and_keeps_secrets_and_generated_files_out() {
        let Some(exe) = git_exe() else { return };
        let base = temp_base();
        let repo = base.join("repo");
        assert!(init_repo(&exe, &repo));
        assert_eq!(checkpoint(&repo).unwrap(), None);

        std::fs::write(repo.join("base.txt"), "changed\n").unwrap();
        std::fs::write(repo.join("new.py"), "print(1)\n").unwrap();
        std::fs::write(repo.join("server.key"), "secret\n").unwrap();
        std::fs::create_dir_all(repo.join("node_modules/pkg")).unwrap();
        std::fs::write(repo.join("node_modules/pkg/index.js"), "x\n").unwrap();
        assert!(create(&repo, &base.join("wt"), "blocked", "hub/blocked").is_err());

        let summary = checkpoint(&repo).unwrap().unwrap();
        assert!(summary.contains("已提交 2 个文件"), "{summary}");
        assert!(summary.contains("2 个凭据或生成文件未提交"), "{summary}");
        let committed = git(&repo, &["show", "--name-only", "--format=", "HEAD"]).unwrap();
        assert!(committed.contains("base.txt") && committed.contains("new.py"));
        assert!(!committed.contains("server.key") && !committed.contains("node_modules"));
        // 提交后仓库干净，任务工作树可以照常创建并带上刚提交的改动。
        let path = create(&repo, &base.join("wt"), "task", "hub/task").unwrap();
        assert_eq!(
            std::fs::read_to_string(path.join("base.txt")).unwrap(),
            "changed\n"
        );
        assert!(!path.join("server.key").exists());
        remove(&repo, &path).unwrap();
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn root_changes_lists_modified_and_new_files_but_not_bytecode() {
        let Some(exe) = git_exe() else { return };
        let base = temp_base();
        let repo = base.join("repo");
        assert!(init_repo(&exe, &repo));
        assert!(root_changes(&repo).unwrap().is_empty());
        std::fs::write(
            repo.join("base.txt"),
            "changed
",
        )
        .unwrap();
        std::fs::create_dir_all(repo.join("tests/pi").join("__pycache__")).unwrap();
        std::fs::write(
            repo.join("tests/pi/probe.sh"),
            "echo
",
        )
        .unwrap();
        std::fs::write(repo.join("tests/pi/__pycache__/x.pyc"), "x").unwrap();
        let changes = root_changes(&repo).unwrap();
        assert_eq!(
            changes.into_iter().collect::<Vec<_>>(),
            vec!["base.txt".to_string(), "tests/pi/probe.sh".to_string()]
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn checkpoint_refuses_a_repository_in_the_middle_of_a_merge() {
        let Some(exe) = git_exe() else { return };
        let base = temp_base();
        let repo = base.join("repo");
        assert!(init_repo(&exe, &repo));
        assert!(git_ok(&exe, &repo, &["checkout", "-q", "-b", "side"]));
        std::fs::write(repo.join("base.txt"), "side\n").unwrap();
        assert!(git_ok(&exe, &repo, &["commit", "-qam", "side"]));
        assert!(git_ok(&exe, &repo, &["checkout", "-q", "-"]));
        std::fs::write(repo.join("base.txt"), "main\n").unwrap();
        assert!(git_ok(&exe, &repo, &["commit", "-qam", "main"]));
        assert!(!git_ok(&exe, &repo, &["merge", "-q", "side"]));
        let problem = midway_state(&repo).unwrap().unwrap();
        assert!(
            problem.contains("合并中途") && problem.contains("base.txt"),
            "{problem}"
        );
        assert!(checkpoint(&repo).unwrap_err().contains("无法自动提交"));
        assert!(!merged(&repo, "side"));
        std::fs::write(repo.join("base.txt"), "both\n").unwrap();
        assert!(git_ok(&exe, &repo, &["commit", "-qam", "resolved"]));
        assert_eq!(midway_state(&repo).unwrap(), None);
        assert!(merged(&repo, "side"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn worktree_lifecycle_merges_task_content_into_main() {
        let Some(exe) = git_exe() else { return };
        let base = temp_base();
        let repo = base.join("repo");
        let worktree_root = base.join("wt");
        assert!(init_repo(&exe, &repo));

        let path = create(&repo, &worktree_root, "task-a", "hub/task-a").unwrap();
        assert_eq!(path, worktree_root.join("task-a"));
        assert!(path.join("base.txt").is_file());

        // 在工作树里真实改文件并提交，主分支此时还看不到。
        std::fs::write(path.join("feature.txt"), "hello\n").unwrap();
        assert!(git_ok(&exe, &path, &["add", "feature.txt"]));
        assert!(git_ok(&exe, &path, &["commit", "-q", "-m", "feature"]));
        assert!(!repo.join("feature.txt").exists());

        let summary = merge(&repo, "hub/task-a").unwrap();
        assert!(summary.contains("hub/task-a"));
        assert!(repo.join("feature.txt").is_file());
        assert_eq!(
            std::fs::read_to_string(repo.join("feature.txt")).unwrap(),
            "hello\n"
        );

        remove(&repo, &path).unwrap();
        assert!(!path.exists());

        let resumed = attach(&repo, &worktree_root, "task-a-resumed", "hub/task-a").unwrap();
        assert!(resumed.join("feature.txt").is_file());
        assert!(merge(&repo, "hub/task-a").is_ok());
        remove(&repo, &resumed).unwrap();

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn remove_is_idempotent_after_native_cleanup_already_deleted_worktree() {
        let Some(exe) = git_exe() else { return };
        let base = temp_base();
        let repo = base.join("repo");
        let worktree_root = base.join("wt");
        assert!(init_repo(&exe, &repo));

        let path = create(
            &repo,
            &worktree_root,
            "task-native-cleanup",
            "hub/task-native-cleanup",
        )
        .unwrap();
        std::fs::write(path.join("feature.txt"), "saved before cleanup\n").unwrap();
        assert!(git_ok(&exe, &path, &["add", "feature.txt"]));
        assert!(git_ok(&exe, &path, &["commit", "-q", "-m", "feature"]));
        merge(&repo, "hub/task-native-cleanup").unwrap();

        let path_text = path.to_string_lossy().to_string();
        assert!(git_ok(
            &exe,
            &repo,
            &["worktree", "remove", "--force", &path_text]
        ));
        assert!(!path.exists());
        let expected_path = path.to_string_lossy().replace('\\', "/");
        assert!(!git(&repo, &["worktree", "list", "--porcelain"])
            .unwrap()
            .lines()
            .filter_map(|line| line.strip_prefix("worktree "))
            .any(|registered| registered
                .replace('\\', "/")
                .eq_ignore_ascii_case(&expected_path)));
        remove(&repo, &path).unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.join("feature.txt")).unwrap(),
            "saved before cleanup\n"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn interrupted_worktree_reuse_preserves_partial_files_and_checks_identity() {
        let Some(exe) = git_exe() else { return };
        let base = temp_base();
        let repo = base.join("repo");
        let worktree_root = base.join("wt");
        assert!(init_repo(&exe, &repo));
        let path = create(&repo, &worktree_root, "task-resume", "hub/task-resume").unwrap();
        std::fs::write(path.join("feat.txt"), "partial edit\n").unwrap();
        let reused = reuse(&repo, &worktree_root, &path, "hub/task-resume").unwrap();
        assert_eq!(reused, path.canonicalize().unwrap());
        assert_eq!(
            std::fs::read_to_string(reused.join("feat.txt")).unwrap(),
            "partial edit\n"
        );
        assert!(reuse(&repo, &worktree_root, &path, "hub/wrong-branch").is_err());
        remove(&repo, &reused).unwrap();
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn worktree_commit_records_changes_for_merge_and_noops_when_clean() {
        let Some(exe) = git_exe() else { return };
        let base = temp_base();
        let repo = base.join("repo");
        let worktree_root = base.join("wt");
        assert!(init_repo(&exe, &repo));

        let path = create(&repo, &worktree_root, "task-d", "hub/task-d").unwrap();
        // 只提交授权文件：构建/检查产物（如 __pycache__）不该进任务分支。
        std::fs::create_dir_all(path.join("__pycache__")).unwrap();
        std::fs::write(path.join("__pycache__/junk.pyc"), b"junk").unwrap();
        // 干净工作树：提交是空操作，不报错，也不产生空提交。
        commit(&path, "hub: 无改动", &["feat.txt".to_string()]).unwrap();
        assert!(git_ok(&exe, &path, &["diff", "--quiet", "HEAD"]));
        assert!(std::fs::metadata(path.join("__pycache__/junk.pyc")).is_ok());

        std::fs::write(path.join("feat.txt"), "x\n").unwrap();
        commit(&path, "hub: task-d", &["feat.txt".to_string()]).unwrap();
        merge(&repo, "hub/task-d").unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.join("feat.txt")).unwrap(),
            "x\n"
        );
        // 未授权产物没有被提交、也没有被合并进主分支。
        assert!(!repo.join("__pycache__/junk.pyc").exists());

        remove(&repo, &path).unwrap();
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn nested_directory_inside_a_repo_is_not_a_project_repo() {
        let Some(exe) = git_exe() else { return };
        let base = temp_base();
        let repo = base.join("repo");
        assert!(init_repo(&exe, &repo));
        let nested = repo.join("sub");
        std::fs::create_dir_all(&nested).unwrap();
        assert!(is_repo(&repo));
        // 嵌套在外层仓库里的子目录不算 git 项目，避免把整棵外层仓库当项目仓库。
        assert!(!is_repo(&nested));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn non_repository_is_rejected() {
        let base = temp_base();
        std::fs::create_dir_all(&base).unwrap();
        assert!(!is_repo(&base));
        let error = create(&base, &base.join("wt"), "task", "hub/task").unwrap_err();
        assert!(error.contains("不是 git 仓库"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn binding_a_plain_folder_creates_a_safe_idempotent_git_baseline() {
        if git_exe().is_none() {
            return;
        }
        let base = temp_base();
        std::fs::create_dir_all(base.join("src")).unwrap();
        std::fs::write(base.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(base.join(".env"), "TOKEN=private-value\n").unwrap();
        std::fs::write(
            base.join("credentials.json"),
            "{\"secret\":\"private-value\"}\n",
        )
        .unwrap();
        std::fs::create_dir_all(base.join("target/debug")).unwrap();
        std::fs::write(base.join("target/debug/generated.bin"), b"generated").unwrap();

        initialize_safe_repository(&base).unwrap();
        assert!(is_repo(&base));
        let committed = git(&base, &["ls-tree", "-r", "--name-only", "HEAD"]).unwrap();
        assert!(committed.lines().any(|path| path == "src/main.rs"));
        assert!(!committed.lines().any(|path| matches!(
            path,
            ".env" | "credentials.json" | "target/debug/generated.bin"
        )));
        assert!(git(&base, &["status", "--porcelain"]).unwrap().is_empty());
        let first_head = git(&base, &["rev-parse", "HEAD"]).unwrap();
        initialize_safe_repository(&base).unwrap();
        assert_eq!(git(&base, &["rev-parse", "HEAD"]).unwrap(), first_head);
        let worktree = create(&base, &base.join("worktrees"), "task", "hub/task").unwrap();
        assert!(worktree.join("src/main.rs").is_file());
        remove(&base, &worktree).unwrap();
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn dirty_repository_is_rejected() {
        let Some(exe) = git_exe() else { return };
        let base = temp_base();
        let repo = base.join("repo");
        assert!(init_repo(&exe, &repo));
        std::fs::write(repo.join("base.txt"), "changed\n").unwrap();
        let error = create(&repo, &base.join("wt"), "task-b", "hub/task-b").unwrap_err();
        assert!(error.contains("未提交"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn generated_python_bytecode_does_not_block_the_next_task_but_source_does() {
        let Some(exe) = git_exe() else { return };
        let base = temp_base();
        let repo = base.join("repo");
        assert!(init_repo(&exe, &repo));
        std::fs::create_dir_all(repo.join("__pycache__")).unwrap();
        std::fs::write(repo.join("__pycache__/calc.cpython-313.pyc"), "cache").unwrap();
        let tree = create(&repo, &base.join("wt"), "cache-ok", "hub/cache-ok").unwrap();
        remove(&repo, &tree).unwrap();
        std::fs::write(repo.join("__pycache__/important.py"), "source").unwrap();
        assert!(create(
            &repo,
            &base.join("wt"),
            "source-denied",
            "hub/source-denied"
        )
        .unwrap_err()
        .contains("未提交"));
        std::fs::remove_file(repo.join("__pycache__/important.py")).unwrap();
        assert!(git_ok(
            &exe,
            &repo,
            &["add", "-f", "__pycache__/calc.cpython-313.pyc"]
        ));
        assert!(git_ok(
            &exe,
            &repo,
            &["commit", "-q", "-m", "tracked cache"]
        ));
        std::fs::write(repo.join("__pycache__/calc.cpython-313.pyc"), "changed").unwrap();
        assert!(create(
            &repo,
            &base.join("wt"),
            "tracked-denied",
            "hub/tracked-denied"
        )
        .unwrap_err()
        .contains("未提交"));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn merge_conflict_stops_and_keeps_the_scene() {
        let Some(exe) = git_exe() else { return };
        let base = temp_base();
        let repo = base.join("repo");
        let worktree_root = base.join("wt");
        assert!(init_repo(&exe, &repo));

        let path = create(&repo, &worktree_root, "task-c", "hub/task-c").unwrap();
        std::fs::write(path.join("base.txt"), "from-task\n").unwrap();
        assert!(git_ok(&exe, &path, &["commit", "-q", "-am", "task change"]));
        std::fs::write(repo.join("base.txt"), "from-main\n").unwrap();
        assert!(git_ok(&exe, &repo, &["commit", "-q", "-am", "main change"]));

        let error = merge(&repo, "hub/task-c").unwrap_err();
        assert!(error.contains("冲突"));
        // 现场保留：合并仍在进行，两边的改动都还在（带冲突标记），也没有被自动 abort。
        assert!(repo.join(".git/MERGE_HEAD").is_file());
        let content = std::fs::read_to_string(repo.join("base.txt")).unwrap();
        assert!(content.contains("from-main") && content.contains("from-task"));
        assert!(content.contains("<<<<<<<"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn merge_sets_aside_identical_local_edits_but_stops_on_different_ones() {
        let Some(exe) = git_exe() else { return };
        let base = temp_base();
        let repo = base.join("repo");
        let worktree_root = base.join("wt");
        assert!(init_repo(&exe, &repo));

        let path = create(&repo, &worktree_root, "task-d", "hub/task-d").unwrap();
        std::fs::write(path.join("base.txt"), "same\n").unwrap();
        assert!(git_ok(&exe, &path, &["commit", "-q", "-am", "task change"]));
        std::fs::write(repo.join("base.txt"), "same\n").unwrap();
        merge(&repo, "hub/task-d").unwrap();
        assert_eq!(std::fs::read_to_string(repo.join("base.txt")).unwrap(), "same\n");
        assert!(git(&repo, &["status", "--porcelain"]).unwrap().is_empty());
        assert!(git(&repo, &["stash", "list"]).unwrap().is_empty());

        let path = create(&repo, &worktree_root, "task-e", "hub/task-e").unwrap();
        std::fs::write(path.join("base.txt"), "task\n").unwrap();
        assert!(git_ok(&exe, &path, &["commit", "-q", "-am", "task change"]));
        std::fs::write(repo.join("base.txt"), "mine\n").unwrap();
        let error = merge(&repo, "hub/task-e").unwrap_err();
        assert!(error.contains("base.txt"));
        assert_eq!(std::fs::read_to_string(repo.join("base.txt")).unwrap(), "mine\n");

        let _ = std::fs::remove_dir_all(&base);
    }
}
