//! 管理 DSH / Codex 的 npm 元数据、隔离候选安装与版本校验。
//!
//! 明确不包含：apply / rollback、任何 npm 或全局配置改写、任何真实模型调用。
//! prepare 只产出待确认的 UpdatePlan，不写 state、不切换 client、也不改动当前安装。
//! 该模块暂未在 main/mod.rs 接线，下一批再接入。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::process_scope::ProcessScope;
use crate::service_install;
use crate::service_plans;

/// Node helper 脚本随二进制编译内联，避免外部文件被替换。
const HELPER_SCRIPT: &str = include_str!("service_update.mjs");
/// helper stdout 上限：超过即拒绝，不向调用方泄露任何原始输出。
const MAX_STDOUT_BYTES: usize = 65_536;
/// 等待退出轮询间隔（与超时判断共用，避免 pipe 满导致等待挂起）。
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
/// `hermes update --plan` 不联网，给短超时即可。
const PLAN_TIMEOUT: Duration = Duration::from_secs(20);
/// `hermes update --check` 会真去 fetch origin（本机实测十几秒），给足。
const CHECK_UPDATE_TIMEOUT: Duration = Duration::from_secs(180);
// DSH 的默认包名；调用方按受支持成员的 Layout 显式提供包名，Codex 也走同一套受管流程。
pub(crate) const EXPECTED_PACKAGE: &str = "@deepseek-ai/dsh";
/// Codex 的 npm 包名，用于远程版本比较和受管候选更新。
pub(crate) const CODEX_PACKAGE: &str = "@openai/codex";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PackageMetadata {
    pub name: String,
    pub version: String,
    pub tarball: String,
    pub integrity: String,
}

/// 取 Store 所在目录（即 data 目录），要求为绝对路径。
///
/// 仅在极短的临界区内克隆 `path.parent()`，不在持锁期间做任何 IO。
pub(crate) fn data_directory(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    use tauri::Manager;

    let directory = {
        let state = app.state::<crate::AppState>();
        let store = state
            .store
            .lock()
            .map_err(|_| "本地状态当前不可用".to_string())?;
        let parent = store
            .path
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| "本地数据目录不可用".to_string())?;
        drop(store); // 立刻释放锁，后续校验/返回都不持锁
        parent
    };

    if !directory.is_absolute() {
        return Err("本地数据目录不是绝对路径".to_string());
    }
    Ok(directory)
}

/// 运行 Node helper 并返回其 JSON 输出。
///
/// 成功条件严格为：进程正常退出（exit code 0）且 JSON 顶层 `ok == true`。
/// 其余情况一律返回中文受控错误，绝不回传 stderr、nativeError、config 或原始 stdout。
pub(crate) fn run_helper(
    data: &Path,
    action: &str,
    args: &[String],
    timeout: Duration,
) -> Result<serde_json::Value, String> {
    let program = crate::dsh::discover().ok_or_else(|| "未找到可用的 DSH 运行时".to_string())?;

    let mut command = Command::new(program);
    command
        .arg("--input-type=module")
        .arg("-e")
        .arg(HELPER_SCRIPT)
        .arg(action)
        .args(args)
        .current_dir(data)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = command
        .spawn()
        .map_err(|_| "无法启动检查进程".to_string())?;

    // spawn 成功后立刻纳入 ProcessScope（kill-on-close），失败则自己收尾自己的子进程。
    let scope = match ProcessScope::attach(&child) {
        Ok(scope) => scope,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err("无法托管检查进程".to_string());
        }
    };

    let reader = spawn_reader(&mut child);

    let deadline = Instant::now() + timeout;
    let mut status = None;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(exited)) => {
                status = Some(exited);
                break;
            }
            Ok(None) => thread::sleep(POLL_INTERVAL),
            Err(_) => break,
        }
    }

    // 超时或状态读取失败：终止整棵进程树并回收，任何路径都不遗留子进程。
    let Some(status) = status else {
        scope.terminate();
        let _ = child.kill();
        let _ = child.wait();
        if let Some(handle) = reader {
            let _ = handle.join();
        }
        return Err("检查进程超时未结束".to_string());
    };

    // child 已被 try_wait 回收；顺序 join 读取线程，保证 stdout 被完整排空且无残留写入端。
    let payload = match reader {
        Some(handle) => match handle.join() {
            Ok(Ok(bytes)) => bytes,
            _ => return Err("检查进程输出无效".to_string()),
        },
        None => return Err("检查进程输出不可用".to_string()),
    };

    if !status.success() {
        return Err("检查进程执行失败".to_string());
    }

    let text = String::from_utf8(payload).map_err(|_| "检查进程输出无效".to_string())?;
    let value: serde_json::Value =
        serde_json::from_str(text.trim()).map_err(|_| "检查进程输出无法解析".to_string())?;

    if value.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err("检查进程未返回有效结果".to_string());
    }
    Ok(value)
}

/// 独立线程持续排空 stdout，最多保留 `MAX_STDOUT_BYTES`；超限则丢弃并标记失败。
///
/// 始终读到 EOF 才结束，因此子进程不会因 pipe 写满而卡住等待。
fn spawn_reader(child: &mut Child) -> Option<JoinHandle<Result<Vec<u8>, ()>>> {
    let stdout = child.stdout.take()?;
    Some(thread::spawn(move || {
        let mut reader = stdout;
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 8192];
        let mut overflow = false;
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    if buffer.len() + read > MAX_STDOUT_BYTES {
                        overflow = true;
                        buffer.clear();
                    }
                    if !overflow {
                        buffer.extend_from_slice(&chunk[..read]);
                    }
                }
                Err(_) => {
                    overflow = true;
                    break;
                }
            }
        }
        if overflow {
            Err(())
        } else {
            Ok(buffer)
        }
    }))
}

/// 只读检查：当前已安装版本（可选）与公开包元数据。
///
/// 元数据 shape 由 Node helper 完整校验（含包名、版本、tarball、integrity），
/// Rust 侧再复核包名；`latest_is_newer` 缺失或非布尔时返回 `None`。
pub(crate) fn check_release(
    data: &Path,
    package: &str,
    installed: Option<&str>,
) -> Result<(PackageMetadata, Option<bool>), String> {
    // 两个位置参数都传：argv[1]=已安装版本（未知传空串），argv[2]=包名。缺一个会被当成另一个。
    let args: Vec<String> = vec![
        installed.unwrap_or_default().to_string(),
        package.to_string(),
    ];

    let value = run_helper(data, "check", &args, CHECK_TIMEOUT)?;

    let metadata: PackageMetadata = serde_json::from_value(
        value
            .get("metadata")
            .cloned()
            .ok_or_else(|| "检查结果缺少元数据".to_string())?,
    )
    .map_err(|_| "检查结果元数据格式无效".to_string())?;

    if metadata.name != package {
        return Err("检查结果包名不匹配".to_string());
    }

    let latest_is_newer = value
        .get("latest_is_newer")
        .and_then(serde_json::Value::as_bool);

    Ok((metadata, latest_is_newer))
}

/// 本机 Hermes 的源码检出（`hermes update` 的 InstallRoot）：`AGENT_HUB_HERMES_REPO` 优先，
/// 否则按默认布局 `%LOCALAPPDATA%/hermes/hermes-agent`；都没有返回 `None`（调用方降级为只做本地检测）。
/// ponytail: 只认这一个布局 + 环境变量覆盖，够了；真要多布局再加候选列表。
pub(crate) fn hermes_repo() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("AGENT_HUB_HERMES_REPO") {
        let path = PathBuf::from(path);
        if path.is_dir() {
            return Some(path);
        }
    }
    let local = std::env::var_os("LOCALAPPDATA")?;
    let candidate = PathBuf::from(local).join("hermes").join("hermes-agent");
    candidate.is_dir().then_some(candidate)
}

/// 问 Hermes 自己的更新通道，全程只读。
///
/// **只调 `hermes update --plan` 与 `--check`，绝不调 `hermes update` 本体**：真正的更新会等
/// Electron 桌面端退出再换文件，而同席助手就活在那个桌面端里（`--plan` 的输出会如实列出
/// `serve [default] — desktop` 这个待重启进程）。`--check` 会真去 fetch origin，所以给足了超时。
pub(crate) fn check_hermes(repo: &Path) -> Result<(Option<String>, Option<bool>, String), String> {
    let exe = repo.join(".venv").join("Scripts").join("hermes.exe");
    if !exe.is_file() {
        return Err("未找到 Hermes 的虚拟环境入口".to_string());
    }

    // --plan 只读且不联网：拿到安装形态、版本与短提交号。
    let plan = run_capture(&exe, &["update", "--plan"], PLAN_TIMEOUT).unwrap_or_default();
    let installed = plan
        .lines()
        .find(|line| line.contains("Install:"))
        .and_then(|line| line.split_once('v').map(|(_, rest)| rest))
        .and_then(|rest| {
            rest.split(|c: char| !c.is_ascii_digit() && c != '.')
                .next()
                .map(str::to_string)
        })
        .filter(|version| version.chars().any(|c| c == '.'));

    // --check 会 fetch origin（实测本机十几秒），只读、不安装任何东西。
    let check = run_capture(&exe, &["update", "--check"], CHECK_UPDATE_TIMEOUT)?;
    let line = check.lines().map(str::trim).find(|line| {
        line.contains("Update available")
            || line.contains("up to date")
            || line.contains("已是最新")
    });
    let available = if check.contains("Update available") || check.contains("commits behind") {
        Some(true)
    } else if check.contains("up to date") || check.contains("已是最新") {
        Some(false)
    } else {
        None
    };
    let note = match (available, line) {
        (Some(true), Some(line)) => {
            format!("已问 origin：{}", line.trim_start_matches(['→', '⚕', ' ']))
        }
        (Some(false), _) => "已问 origin：已是最新".to_string(),
        _ => "已问 origin，但没有得到可判读的结论".to_string(),
    };
    Ok((installed, available, note))
}

/// 跑一个只读子进程并取回 stdout 文本：带超时，超时杀掉整棵进程树，失败只给中文受控错误。
fn run_capture(program: &Path, args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = command
        .spawn()
        .map_err(|_| "无法启动检查进程".to_string())?;
    let scope = match ProcessScope::attach(&child) {
        Ok(scope) => scope,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err("无法托管检查进程".to_string());
        }
    };
    let reader = spawn_reader(&mut child);

    let deadline = Instant::now() + timeout;
    let mut status = None;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(exited)) => {
                status = Some(exited);
                break;
            }
            Ok(None) => thread::sleep(POLL_INTERVAL),
            Err(_) => break,
        }
    }

    let Some(status) = status else {
        scope.terminate();
        let _ = child.kill();
        let _ = child.wait();
        if let Some(handle) = reader {
            let _ = handle.join();
        }
        return Err("检查进程超时未结束".to_string());
    };

    let payload = match reader {
        Some(handle) => match handle.join() {
            Ok(Ok(bytes)) => bytes,
            _ => return Err("检查进程输出无效".to_string()),
        },
        None => return Err("检查进程输出不可用".to_string()),
    };
    if !status.success() {
        return Err("检查进程执行失败".to_string());
    }
    Ok(String::from_utf8_lossy(&payload).into_owned())
}

// ---------------------------------------------------------------------------
// 第二批：DSH 预演（preflight）。只验证 ACP 会话与配置选择能力：不发送
// session/prompt，因此不触发任何真实模型调用，也不宣称认证/工具头已验证。
// ---------------------------------------------------------------------------

/// ACP 桥脚本与认证桥文件同样内联进二进制，避免外部文件被替换。
const DSH_BRIDGE_SCRIPT: &str = include_str!("dsh_bridge.mjs");
const DSH_HERMES_AUTH_BRIDGE: &str = include_str!("dsh_hermes_auth.py");
/// 预演桥进程独占的数据目录环境变量（每次预演都是全新目录，绝不复用 dsh-native 或用户 history）。
const DSH_DATA_DIR_ENV: &str = "AGENT_HUB_DSH_DATA_DIR";
/// 两条预演 route 的原始 (provider, model) 二元组。model 选项的真实 value 是两者
/// 的 JSON 编码完整二元组，因此必须能递归收集到（例如）
/// ["command-code-daily","deepseek/deepseek-v4.1-flash"] 与
/// ["command-code-daily-2","deepseek/deepseek-v4.1-flash"] 这两个 value。形如
/// `["command-code-daily","command-code-daily"]` 的取值不匹配任何 route。
const PREFLIGHT_ROUTES: [(&str, &str); 2] = [
    ("command-code-daily", "deepseek/deepseek-v4.1-flash"),
    ("command-code-daily-2", "deepseek/deepseek-v4.1-flash"),
];
/// 预演期望的推理强度取值。
const PREFLIGHT_REASONING_EFFORT: &str = "off";

/// 递归收集选项（含 group 嵌套数组）里的字符串 value；非字符串 value 一律忽略。
fn collect_option_values(option: &serde_json::Value, collected: &mut Vec<String>) {
    if let Some(value) = option.get("value").and_then(serde_json::Value::as_str) {
        collected.push(value.to_string());
    }
    if let Some(nested) = option.get("options").and_then(serde_json::Value::as_array) {
        for child in nested {
            collect_option_values(child, collected);
        }
    }
}

/// 在 configOptions 顶层数组里按 id 取选项；缺失即失败，不做任何猜测。
fn find_config_option<'a>(
    config_options: &'a serde_json::Value,
    id: &str,
) -> Result<&'a serde_json::Value, String> {
    config_options
        .as_array()
        .and_then(|options| {
            options
                .iter()
                .find(|option| option.get("id").and_then(serde_json::Value::as_str) == Some(id))
        })
        .ok_or_else(|| "预演会话未提供模型选项".to_string())
}

/// 单次 ACP 请求：任何失败都折叠为固定中文错误，绝不回传原生文本或 payload。
fn preflight_rpc(
    client: &crate::rpc::Client,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    client
        .rpc(method, params)
        .map_err(|_| "预演请求未成功".to_string())
}

/// 预演：在独立 owned 数据目录上跑 ACP 会话，核对 model 选项并逐 route 选择配置。
///
/// stdout/stderr、超时与进程范围都由 `Client`（自带进程 Scope）托管，native stderr
/// 受控；本函数只做 ACP 能力预演，不发送 session/prompt，也不读取私有 profile 文本。
pub(crate) fn preflight_dsh(data: &Path, installation: &Path) -> Result<(), String> {
    let program = crate::dsh::discover().ok_or_else(|| "未找到可用的 DSH 运行时".to_string())?;

    // 每次预演都新建独立目录，绝不复用已有 dsh-native 或用户 history。
    let workspace = data
        .join("managed-services")
        .join("dsh-win")
        .join("preflight")
        .join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir_all(&workspace).map_err(|_| "无法创建预演数据目录".to_string())?;

    let mut command = Command::new(program);
    command
        .arg("--input-type=module")
        .arg("-e")
        .arg(DSH_BRIDGE_SCRIPT)
        .arg(installation)
        // bridge 的第二个位置参数是它自己的数据目录（argv[2]）；缺失会让 bridge 在
        // resolve(undefined) 处启动失败，预演因此永远是「请求未成功」。
        .arg(&workspace)
        .current_dir(installation)
        .env("AGENT_HUB_DSH_AUTH_BRIDGE", DSH_HERMES_AUTH_BRIDGE)
        .env(DSH_DATA_DIR_ENV, &workspace)
        .env_remove("PROJECT_MODE");

    let client = crate::rpc::Client::spawn_command(command, |_| {}, || {})
        .map_err(|_| "无法启动预演子进程".to_string())?;

    // 用闭包收集结果：无论 Ok/Err 都在闭包外 stop() 回收本函数自己拥有的进程。
    let result = (|| -> Result<(), String> {
        preflight_rpc(
            &client,
            "initialize",
            serde_json::json!({
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": { "readTextFile": false, "writeTextFile": false },
                    "terminal": false
                }
            }),
        )?;

        let session = preflight_rpc(
            &client,
            "session/new",
            serde_json::json!({
                "cwd": installation.to_string_lossy(),
                "mcpServers": [],
            }),
        )?;
        let session_id = session
            .get("sessionId")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "预演会话未返回会话标识".to_string())?
            .to_string();

        let config_options = session
            .get("configOptions")
            .ok_or_else(|| "预演会话未提供配置选项".to_string())?;
        let mut values = Vec::new();
        collect_option_values(find_config_option(config_options, "model")?, &mut values);

        // model 选项的真实 value 是 JSON 编码的完整二元组（provider 与 model 两个
        // 原始字符串合在一起），例如 ["command-code-daily","deepseek/deepseek-v4.1-flash"]。
        // 因此每个 route 都要先编码出该 value_id，再确认它确实出现在递归收集到的取值里。
        let mut route_values: Vec<String> = Vec::with_capacity(PREFLIGHT_ROUTES.len());
        for (provider, model) in PREFLIGHT_ROUTES {
            let value_id = serde_json::to_string(&[provider, model])
                .map_err(|_| "预演模型取值编码失败".to_string())?;
            if !values.iter().any(|value| value == &value_id) {
                return Err("预演模型取值与预期不符".to_string());
            }
            route_values.push(value_id);
        }

        // 逐 route 先选模型、再选推理强度；两次 RPC 都必须成功。
        // 发送模型时同样使用上面校验过的完整 JSON value_id，不做任何拆分或降级。
        for value_id in &route_values {
            preflight_rpc(
                &client,
                "session/set_config_option",
                serde_json::json!({
                    "sessionId": session_id,
                    "configId": "model",
                    "value": value_id,
                }),
            )?;
            preflight_rpc(
                &client,
                "session/set_config_option",
                serde_json::json!({
                    "sessionId": session_id,
                    "configId": "reasoning_effort",
                    "value": PREFLIGHT_REASONING_EFFORT,
                }),
            )?;
        }
        Ok(())
    })();

    client.stop();
    result
}

// ---------------------------------------------------------------------------
// 更新准备（prepare）。只做「下载到全新 slot + 成员原生预检 + 重复封印」，并
// 产出一份待确认的 UpdatePlan。绝不写 state、绝不切换 client、绝不改动当前安装；
// 任何失败都保留候选目录、也不声称 ready，交由下一次重试。
// ---------------------------------------------------------------------------

/// 当前安装清单（package.json）允许读取的大小上限：1 MiB。
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
/// 槽位内 node_modules 目录名（与受管布局一致）。
const NODE_MODULES_DIR: &str = "node_modules";
/// SHA-256 指纹的十六进制长度。
const TREE_HASH_HEX_LEN: usize = 64;
/// 真实下载/安装/指纹准备的超时。Codex 的 npm 平台二进制另外几十 MB，所以给足。
const PREPARE_TIMEOUT: Duration = Duration::from_secs(900);
/// 候选原生预检（跑候选槽里的 exe 报版本）的超时。
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(60);
/// 预演之后重复封印校验的超时。
const VERIFY_TIMEOUT: Duration = Duration::from_secs(90);

/// 严格按十六进制字符校验指纹：长度必须是 64 且全部为 hex，不接受截断或前缀。
fn is_tree_hash(value: &str) -> bool {
    value.len() == TREE_HASH_HEX_LEN && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// 受控读取指定安装目录的 package.json：最多 1 MiB、包名必须精确匹配、version 必须是字符串。
///
/// 返回 `(version, 原始字节的 SHA-256 小写十六进制)`；任何失败都不回传原始字节内容。
pub(crate) fn manifest_at(installation: &Path, package: &str) -> Result<(String, String), String> {
    let path = installation.join("package.json");
    let file = std::fs::File::open(&path).map_err(|_| "无法读取当前安装的包描述".to_string())?;

    let mut bytes = Vec::new();
    file.take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "无法读取当前安装的包描述".to_string())?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err("当前安装的包描述过大".to_string());
    }

    let manifest: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| "当前安装的包描述无法解析".to_string())?;
    if manifest.get("name").and_then(serde_json::Value::as_str) != Some(package) {
        return Err("当前安装的包名不匹配".to_string());
    }
    let version = manifest
        .get("version")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "当前安装的包版本无效".to_string())?
        .to_string();

    let mut digest = Sha256::new();
    digest.update(&bytes);
    Ok((version, format!("{:x}", digest.finalize())))
}

/// 读取当前安装的 package.json（按布局）。
fn read_manifest_for(
    data: &Path,
    layout: &service_install::Layout,
) -> Result<(String, String), String> {
    manifest_at(
        &service_install::package_path_for(data, layout)?,
        layout.package,
    )
}

/// 当前安装 package.json 的内容指纹（按布局）。
pub(crate) fn current_manifest_hash_for(
    data: &Path,
    layout: &service_install::Layout,
) -> Result<String, String> {
    read_manifest_for(data, layout).map(|(_, hash)| hash)
}

/// 复核指定计划对应的候选槽位：helper `verify` 复算的指纹必须与计划相同，且候选
/// package.json 的版本（包名由 `manifest_at` 校验）必须与发布元数据一致，成功返回候选目录。
///
/// 与 `verify_plan` 不同：这里**不**比对当前 InstallState 三元组、也**不**比对当前清单
/// 指纹，因为 receipt 可能是已经用过的或 `previous` 旧槽位，其原始 base 早已变化。
/// receipt 的严格 schema / UUID / 期限规则由调用方判定，本函数不重读或改写计划，
/// 不写 state、不启动原生会话、也不回传任何原始输出。
pub(crate) fn verify_receipt_for(
    data: &Path,
    layout: &service_install::Layout,
    plan: &service_plans::UpdatePlan,
) -> Result<PathBuf, String> {
    let verify_args = vec![
        service_install::root_for(data, layout)?
            .to_string_lossy()
            .into_owned(),
        plan.id.clone(),
        plan.tree_sha256.clone(),
    ];
    let verified = run_helper(data, "verify", &verify_args, VERIFY_TIMEOUT)?;
    if verified
        .get("tree_sha256")
        .and_then(serde_json::Value::as_str)
        != Some(plan.tree_sha256.as_str())
    {
        return Err("候选内容与计划不符".to_string());
    }

    let candidate = service_install::slot_package_for(data, layout, &plan.id)?;
    if manifest_at(&candidate, layout.package)?.0 != plan.metadata.version {
        return Err("候选槽位版本与计划不符".to_string());
    }
    Ok(candidate)
}

/// 执行 apply/rollback 之前的严格复核：状态三元组、当前清单指纹、helper `verify` 复算的
/// 候选指纹、候选 package.json 版本（包名由 `manifest_at` 校验）都必须与计划完全一致。
/// 任何失败都返回受控中文错误：不写 state、不启动原生会话、不回传原始输出。
pub(crate) fn verify_plan_for(
    data: &Path,
    layout: &service_install::Layout,
    plan: &service_plans::UpdatePlan,
) -> Result<(), String> {
    let current = service_install::read_state_for(data, layout)?;
    if current.revision != plan.base_state.revision
        || current.active != plan.base_state.active
        || current.previous != plan.base_state.previous
    {
        return Err("本地安装状态已变化".to_string());
    }
    if current_manifest_hash_for(data, layout)? != plan.base_manifest_sha256 {
        return Err("当前安装内容已变化".to_string());
    }

    // 当前 state 与 base 指纹校验绝不可省；候选指纹/版本复用 receipt 校验，不重复实现。
    verify_receipt_for(data, layout, plan)?;
    Ok(())
}

/// 当前安装的版本（按布局）。
pub(crate) fn installed_version_for(
    data: &Path,
    layout: &service_install::Layout,
) -> Result<String, String> {
    read_manifest_for(data, layout).map(|(version, _)| version)
}

/// 预检目标：受管槽位给槽位根；外部安装给包目录（回退到外部安装时才用）。
pub(crate) enum PreflightTarget {
    Slot(PathBuf),
    External(PathBuf),
}

/// 外部安装里的可执行文件，两种布局都认：本地 prefix 是平的
/// （`<prefix>/node_modules/@openai/codex-win32-x64/…`），`npm i -g` 把平台包嵌在主包里面
/// （`<包目录>/node_modules/@openai/codex-win32-x64/…`）。先平后嵌——只认平的那种，
/// 「回退到外部安装」在全局安装上会误报「缺少可执行文件」。
pub(crate) fn local_or_nested_executable(
    installation: &Path,
    layout: &service_install::Layout,
) -> Option<PathBuf> {
    let relative = layout.executable?;
    let flat = install_prefix(installation, layout)
        .map(|prefix| prefix.join(relative))
        .filter(|path| path.is_file());
    if flat.is_some() {
        return flat;
    }
    let nested = installation.join(relative);
    nested.is_file().then_some(nested)
}

/// 从包目录推出安装前缀（`<prefix>/node_modules/<package>`，package 可能带 scope）：
/// 按「node_modules + 包名自己的分量数」砍，不靠数层数。
fn install_prefix(package_dir: &Path, layout: &service_install::Layout) -> Option<PathBuf> {
    let mut prefix = package_dir.to_path_buf();
    for _ in 0..(1 + layout.package.split('/').count()) {
        prefix = prefix.parent()?.to_path_buf();
    }
    Some(prefix)
}

/// apply/rollback 在切换前再跑一次的候选预检。
pub(crate) fn preflight_target(
    data: &Path,
    layout: &service_install::Layout,
    target: PreflightTarget,
    version: &str,
) -> Result<(), String> {
    // 没有独立可执行文件的成员（DSH）：走 ACP 预演桥，喂给它包目录。
    let Some(relative) = layout.executable else {
        let package = match &target {
            PreflightTarget::Slot(slot) => slot.join(NODE_MODULES_DIR).join(layout.package),
            PreflightTarget::External(directory) => directory.clone(),
        };
        return preflight_dsh(data, &package);
    };
    // 有独立可执行文件的成员（Codex）：跑那份 exe 报版本。
    let exe = match &target {
        PreflightTarget::Slot(slot) => slot.join(relative),
        // 外部安装两种布局都要认：本地 prefix 是平的，`npm i -g` 把平台包嵌在主包里面。
        PreflightTarget::External(directory) => local_or_nested_executable(directory, layout)
            .ok_or_else(|| "外部安装路径不合法".to_string())?,
    };
    if !exe.is_file() {
        return Err(format!("候选缺少可执行文件 {}", exe.display()));
    }
    let text = run_capture(&exe, &["--version"], PREFLIGHT_TIMEOUT)?;
    if !text.contains(version) {
        return Err(format!(
            "候选 {} 版本不符（期望 {version}）",
            layout.package
        ));
    }
    Ok(())
}

/// 准备一次成员更新：下载并安装到全新 slot，做该成员的预检，再重复封印比对。
///
/// 只有以下全部成立才构造并独占落盘 UpdatePlan：
/// helper `prepare` 真实完成且 `slot_id`/`version`/`tree_sha256` 与本地期望完全一致；
/// 预检通过；helper `verify` 复算指纹与 prepare 完全一致；
/// 期间 InstallState 三元组与当前安装清单指纹都未漂移（否则 stale 拒绝）。
/// 本函数不写 state、不切换 client、不修改当前安装；失败时保留候选目录。
pub(crate) fn prepare_member(
    data: &Path,
    layout: &service_install::Layout,
) -> Result<service_plans::UpdatePlan, String> {
    // 基线：先固定状态与当前清单指纹，之后任何漂移都判 stale。
    let baseline = service_install::read_state_for(data, layout)?;
    let base_manifest_sha256 = current_manifest_hash_for(data, layout)?;
    let installed_version = read_manifest_for(data, layout)?.0;

    let (metadata, latest_is_newer) =
        check_release(data, layout.package, Some(&installed_version))?;
    // 远程比较必须明确为「有更新」才允许创建候选；未知比较同样拒绝，绝不隐式降级。
    match latest_is_newer {
        Some(true) => {}
        Some(false) => return Err("当前安装已是最新版本".to_string()),
        None => return Err("无法确认是否存在更新版本".to_string()),
    }

    let id = uuid::Uuid::new_v4().to_string();
    let root = service_install::root_for(data, layout)?; // root_for 已确认无 reparse 点
    std::fs::create_dir_all(&root).map_err(|_| "无法创建更新根目录".to_string())?;

    let prepare_args = vec![
        root.to_string_lossy().into_owned(),
        id.clone(),
        serde_json::to_string(&metadata).map_err(|_| "更新元数据无法编码".to_string())?,
    ];
    let prepared = run_helper(data, "prepare", &prepare_args, PREPARE_TIMEOUT)?;

    if prepared.get("slot_id").and_then(serde_json::Value::as_str) != Some(id.as_str()) {
        return Err("更新槽位标识与预期不符".to_string());
    }
    if prepared.get("version").and_then(serde_json::Value::as_str)
        != Some(metadata.version.as_str())
    {
        return Err("更新槽位版本与发布元数据不符".to_string());
    }
    let tree_sha256 = prepared
        .get("tree_sha256")
        .and_then(serde_json::Value::as_str)
        .filter(|hash| is_tree_hash(hash))
        .map(str::to_string)
        .ok_or_else(|| "更新槽位指纹无效".to_string())?;

    preflight_target(
        data,
        layout,
        PreflightTarget::Slot(service_install::slot_root_for(data, layout, &id)?),
        &metadata.version,
    )?;

    let verify_args = vec![
        root.to_string_lossy().into_owned(),
        id.clone(),
        tree_sha256.clone(),
    ];
    let verified = run_helper(data, "verify", &verify_args, VERIFY_TIMEOUT)?;
    if verified
        .get("tree_sha256")
        .and_then(serde_json::Value::as_str)
        != Some(tree_sha256.as_str())
    {
        return Err("候选内容在预演后发生变化".to_string());
    }

    // stale 检查：状态三元组与当前安装清单指纹都必须与基线一致。
    let current = service_install::read_state_for(data, layout)?;
    if current.revision != baseline.revision
        || current.active != baseline.active
        || current.previous != baseline.previous
    {
        return Err("本地安装状态已变化".to_string());
    }
    if current_manifest_hash_for(data, layout)? != base_manifest_sha256 {
        return Err("当前安装内容已变化".to_string());
    }

    let plan = service_plans::UpdatePlan {
        id,
        service_id: layout.slot.to_string(),
        metadata,
        tree_sha256,
        base_state: baseline,
        base_manifest_sha256,
        created_at: service_plans::now()?,
    };
    service_plans::save_plan(data, layout, &plan)?;
    Ok(plan)
}
