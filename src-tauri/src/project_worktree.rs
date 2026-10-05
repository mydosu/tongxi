//! Git 工作树管理：每个实现任务在独立工作树里开发，全部验收通过后由主仓库一次性合并。
//!
//! 安全默认（硬规则）：
//!  · 原仓库存在未提交改动时拒绝创建新工作树，避免把未完成的改动混进合并基线；
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

/// 在 `worktree_root/name` 建一个新工作树，检出到新分支 `branch`，返回工作树路径。
///
/// 安全默认：原仓库有未提交改动时拒绝开始。目标目录或分支已存在一律明确报错，不静默复用。
pub fn create(root: &Path, worktree_root: &Path, name: &str, branch: &str) -> Result<PathBuf> {
    if !is_repo(root) {
        return Err("目标目录不是 git 仓库".into());
    }
    if !git(root, &["status", "--porcelain"])?.is_empty() {
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

/// 删掉一个工作树（`--force`：任务分支上的改动已在别处保留，这里只清理现场）。
pub fn remove(root: &Path, path: &Path) -> Result<()> {
    if !is_repo(root) {
        return Err("目标目录不是 git 仓库".into());
    }
    let path_text = path.to_string_lossy().to_string();
    git(root, &["worktree", "remove", "--force", &path_text])?;
    Ok(())
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

/// 把 `branch` 合并进 root 当前分支，返回一句摘要。
///
/// 只在全部任务通过验收之后调用一次。冲突必须停下、保留现场、返回明确原因；
/// 绝不强推、绝不丢弃任何一边、绝不自动 abort。
pub fn merge(root: &Path, branch: &str) -> Result<String> {
    if !is_repo(root) {
        return Err("目标目录不是 git 仓库".into());
    }
    match git(root, &["merge", "--no-edit", branch]) {
        Ok(_) => Ok(format!("分支 {branch} 已合并进当前分支")),
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

        let _ = std::fs::remove_dir_all(&base);
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
}
