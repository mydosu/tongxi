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



/// `branch` 是否已经并入当前分支（冲突处理后用来核实，而不是相信成员的口头汇报）。
pub fn merged(root: &Path, branch: &str) -> bool {
    git(root, &["merge-base", "--is-ancestor", branch, "HEAD"]).is_ok()
}







/// 多个任务同时开工、做完的任务同时在合并：开工前提交、建工作树、合并和查根目录状态
/// 在同一进程内串行，避免 git index.lock 冲突。
static CHECKPOINT: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    CHECKPOINT.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
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


/// 主脑派活用：从当前 HEAD 开一个工作树和新分支。主目录有没提交的改动也照开（它们不会带进工作树），由主脑自己掂量。
pub fn add(root: &Path, worktree_root: &Path, name: &str, branch: &str) -> Result<PathBuf> {
    if !is_repo(root) {
        return Err("项目目录不是 git 仓库，不能开工作树".into());
    }
    let _serial = serial();
    let path = worktree_root.join(name);
    if path.exists() {
        return Err(format!("工作树目录已存在：{name}"));
    }
    std::fs::create_dir_all(worktree_root).map_err(|_| "工作树根目录创建失败".to_string())?;
    let path_text = path.to_string_lossy().to_string();
    git(root, &["worktree", "add", &path_text, "-b", branch])?;
    Ok(path)
}

/// 在项目目录里跑一条 git 命令取输出（主脑工具查分支状态用）。
/// 工作树里有没提交的改动就整个提交进它的分支（成员在沙箱里提交不了时，合并前由同席补上）。
pub fn commit_all(worktree: &Path, message: &str) -> Result<()> {
    if git(worktree, &["status", "--porcelain"])?.trim().is_empty() {
        return Ok(());
    }
    git(worktree, &["add", "-A"])?;
    git(worktree, &["-c", "user.name=agent-hub", "-c", "user.email=hub@local", "commit", "--no-verify", "-q", "-m", message])?;
    Ok(())
}

pub fn git_output(root: &Path, args: &[&str]) -> Result<String> {
    git(root, args)
}



/// 删掉一个工作树（`--force`：任务分支上的改动已在别处保留，这里只清理现场）。
pub fn remove(root: &Path, path: &Path) -> Result<()> {
    if !is_repo(root) {
        return Err("目标目录不是 git 仓库".into());
    }
    // 成员可能在工作树里建了指向项目目录的链接（比如把 node_modules 连到原仓库免得重装）：
    // 先只拆链接本身，删除时就不会顺着链接动到原仓库，也不会因为链接删不掉而残留整棵树。
    unlink_links(path);
    let path_text = path.to_string_lossy().to_string();
    let outcome = remove_registered(root, path, &path_text);
    // 之前被进程占着（成员起的后台 shell 还停在里面）没删掉的空目录，这时多半已经放开了。
    if let Some(parent) = path.parent() {
        sweep_empty(root, parent);
    }
    outcome
}

fn remove_registered(root: &Path, path: &Path, path_text: &str) -> Result<()> {
    match git(root, &["worktree", "remove", "--force", path_text]) {
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
            if !registered && path.exists() {
                // Git 已经注销了工作树、只是没删干净目录：链接已拆，剩下的都是工作树自己的文件。
                let _ = std::fs::remove_dir_all(path);
            }
            // 只剩一个空目录（Windows 上还有进程把它当工作目录）不算失败：下次清理时再删。
            let empty = path.read_dir().is_ok_and(|mut entries| entries.next().is_none());
            if !registered && (!path.exists() || empty) {
                Ok(())
            } else {
                Err(error)
            }
        }
    }
}

/// 删掉工作树目录下 git 已不再登记的空目录；非空的、仍登记的都不碰。
fn sweep_empty(root: &Path, parent: &Path) {
    let Ok(listed) = git(root, &["worktree", "list", "--porcelain"]) else {
        return;
    };
    let registered = listed
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(|entry| entry.replace('\\', "/").to_lowercase())
        .collect::<Vec<_>>();
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let text = path.to_string_lossy().replace('\\', "/").to_lowercase();
        if path.is_dir() && !registered.contains(&text) {
            let _ = std::fs::remove_dir(&path);
        }
    }
}

/// 拆掉目录里所有符号链接和目录联接（只删链接，不进入、不动链接指向的内容）。
fn unlink_links(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            // Windows 的目录链接和联接要用 remove_dir 删，文件链接用 remove_file。
            if std::fs::remove_dir(&path).is_err() {
                let _ = std::fs::remove_file(&path);
            }
        } else if meta.is_dir() && entry.file_name() != ".git" {
            unlink_links(&path);
        }
    }
}



/// 把 `branch` 合并进 root 当前分支，返回一句摘要。
///
/// 只在全部任务通过验收之后调用一次。冲突必须停下、保留现场、返回明确原因；
/// 绝不强推、绝不丢弃任何一边、绝不自动 abort。
pub fn merge(root: &Path, branch: &str) -> Result<String> {
    if !is_repo(root) {
        return Err("目标目录不是 git 仓库".into());
    }
    let _serial = serial();
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
    fn remove_sweeps_leftover_empty_dirs_but_keeps_other_content() {
        let Some(exe) = git_exe() else { return };
        let base = temp_base();
        let repo = base.join("repo");
        let worktree_root = base.join("wt");
        assert!(init_repo(&exe, &repo));
        let path = add(&repo, &worktree_root, "task-sweep", "hub/task-sweep").unwrap();
        let leftover = worktree_root.join("old-empty");
        let other = worktree_root.join("other-project");
        std::fs::create_dir_all(&leftover).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("keep.txt"), "keep").unwrap();
        remove(&repo, &path).unwrap();
        assert!(!path.exists() && !leftover.exists(), "工作树和残留空目录都删掉");
        assert!(other.join("keep.txt").exists(), "非空目录不碰");
    }

    #[cfg(windows)]
    #[test]
    fn remove_unlinks_junctions_without_touching_their_targets() {
        let Some(exe) = git_exe() else { return };
        let base = temp_base();
        let repo = base.join("repo");
        let worktree_root = base.join("wt");
        assert!(init_repo(&exe, &repo));
        let deps = base.join("deps");
        std::fs::create_dir_all(deps.join("pkg")).unwrap();
        std::fs::write(deps.join("pkg").join("index.js"), "keep
").unwrap();

        let path = add(&repo, &worktree_root, "task-junction", "hub/task-junction").unwrap();
        let link = path.join("node_modules");
        let made = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&deps)
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(made.success());
        remove(&repo, &path).unwrap();
        assert!(!path.exists(), "工作树删干净");
        assert_eq!(std::fs::read_to_string(deps.join("pkg").join("index.js")).unwrap(), "keep
", "链接指向的原仓库内容不动");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn remove_is_idempotent_after_native_cleanup_already_deleted_worktree() {
        let Some(exe) = git_exe() else { return };
        let base = temp_base();
        let repo = base.join("repo");
        let worktree_root = base.join("wt");
        assert!(init_repo(&exe, &repo));

        let path = add(
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
        let error = add(&base, &base.join("wt"), "task", "hub/task").unwrap_err();
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
        let worktree = add(&base, &base.join("worktrees"), "task", "hub/task").unwrap();
        assert!(worktree.join("src/main.rs").is_file());
        remove(&base, &worktree).unwrap();
        let _ = std::fs::remove_dir_all(&base);
    }



    #[test]
    fn merge_conflict_stops_and_keeps_the_scene() {
        let Some(exe) = git_exe() else { return };
        let base = temp_base();
        let repo = base.join("repo");
        let worktree_root = base.join("wt");
        assert!(init_repo(&exe, &repo));

        let path = add(&repo, &worktree_root, "task-c", "hub/task-c").unwrap();
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

        let path = add(&repo, &worktree_root, "task-d", "hub/task-d").unwrap();
        std::fs::write(path.join("base.txt"), "same\n").unwrap();
        assert!(git_ok(&exe, &path, &["commit", "-q", "-am", "task change"]));
        std::fs::write(repo.join("base.txt"), "same\n").unwrap();
        merge(&repo, "hub/task-d").unwrap();
        assert_eq!(std::fs::read_to_string(repo.join("base.txt")).unwrap(), "same\n");
        assert!(git(&repo, &["status", "--porcelain"]).unwrap().is_empty());
        assert!(git(&repo, &["stash", "list"]).unwrap().is_empty());

        let path = add(&repo, &worktree_root, "task-e", "hub/task-e").unwrap();
        std::fs::write(path.join("base.txt"), "task\n").unwrap();
        assert!(git_ok(&exe, &path, &["commit", "-q", "-am", "task change"]));
        std::fs::write(repo.join("base.txt"), "mine\n").unwrap();
        let error = merge(&repo, "hub/task-e").unwrap_err();
        assert!(error.contains("base.txt"));
        assert_eq!(std::fs::read_to_string(repo.join("base.txt")).unwrap(), "mine\n");

        let _ = std::fs::remove_dir_all(&base);
    }
}
