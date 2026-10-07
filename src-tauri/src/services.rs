//! Read-only service inventory for the four runtimes, maintenance lock,
//! per-member update checks, and DSH / Codex managed update transactions.

use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tauri::Manager;

/// Managed services, in the order the inventory reports them.
const MANAGED: [(&str, &str); 4] = [
    ("codex-win", "Codex"),
    ("hermes-win", "Hermes"),
    ("dsh-win", "DSH"),
    ("albion-wsl", "Albion"),
];

#[derive(Serialize)]
pub struct ServiceInfo {
    pub id: String,
    pub name: String,
    pub connection: String,
    pub executable: Option<String>,
    pub runtime_version: Option<String>,
    pub installed_version: Option<String>,
    /// `handshake` / `unknown` for the running version, or the install metadata
    /// origin (`executable` / `manifest`) when `installed_version` is filled in.
    pub version_source: String,
    pub busy: bool,
    pub update_state: String,
}

/// Result of an update check. The installed side is always read locally; DSH and
/// Codex additionally ask the npm registry, and `check_source`/`latest_version`
/// say so explicitly instead of dressing a local version up as "latest".
#[derive(Serialize)]
pub struct CheckResult {
    pub service_id: String,
    pub checked_at: String,
    pub runtime_version: Option<String>,
    pub installed_version: Option<String>,
    pub version_source: String,
    /// Remote release version, only filled in when a real remote query ran.
    pub latest_version: Option<String>,
    /// `Some` only when the remote release was actually compared with the
    /// installed version; `None` means "not checked remotely", never "no update".
    pub update_available: Option<bool>,
    /// `npm-registry`, `hermes-update`, or `local`, according to the actual source.
    pub check_source: String,
    pub note: String,
}

/// Update-plan evaluation. `supported: false` means "no executable plan", and the
/// two plan fields stay `None` — never a placeholder id or a guessed version.
///
/// DSH 与 Codex 经专用隔离安装槽、完整指纹、原生预检和持久计划准备候选；
/// 另外两个服务只回报各自更新通道的限制，不假装存在应用内候选。
#[derive(Serialize)]
pub struct PlanResult {
    pub service_id: String,
    pub supported: bool,
    pub reason: String,
    /// 持久计划的 id；只有真的写出了计划才会有值。
    pub plan_id: Option<String>,
    /// 预演选中的候选版本；没有候选一律 `None`，绝不拿本地版本充数。
    pub candidate_version: Option<String>,
    pub created_at: String,
}

/// 受管更新的只读状态快照。`managed: false` 表示这个服务没有受管安装槽，此时其余
/// 字段一律 `None` / `false`，绝不把它伪装成"也有受管更新"。
#[derive(Serialize)]
pub struct UpdateStatus {
    pub service_id: String,
    /// 只有确实存在受管槽（`state.active`）才为 `true`。
    pub managed: bool,
    /// 当前生效槽 manifest 里的版本；路径或 JSON 坏掉时保持 `None`。
    pub active_version: Option<String>,
    /// 回滚会退回的版本：有基线槽就读它的 manifest，否则读外部安装的 manifest。
    pub previous_version: Option<String>,
    /// 与当前 state 完全对齐的最近一个持久计划的 id；没有就是 `None`。
    pub ready_plan_id: Option<String>,
    /// 同一个计划的候选版本；绝不拿本地版本充数。
    pub candidate_version: Option<String>,
    /// 只有 active 槽存在、receipt 可读，且回滚目标 manifest 确实读得出来（外部安装
    /// 还要求它的 manifest 指纹与 receipt 记录一致）时才为 `true`。
    pub rollback_available: bool,
}

/// One held maintenance lock.
#[derive(Debug)]
pub struct LockState {
    pub operation: String,
    #[allow(dead_code)] // Recorded for the next batch's maintenance diagnostics.
    pub acquired_at: u64,
}

/// Serializes maintenance per service. Reused by the next batch, which will
/// restart self-built child processes while holding the same lock.
#[derive(Default, Debug)]
pub struct MaintenanceLock {
    inner: Mutex<HashMap<String, LockState>>,
}

/// RAII handle: dropping it always releases the service lock, so every early
/// `?` return (and any panic the framework catches) still frees the lock.
///
/// 除了本进程的维护 map，它还持有 store 里的原子维护标记。`Drop` 先在一个很短的
/// StoreMutex 作用域内清掉自己的标记，然后才释放维护 map：两把锁不会同时持有。
pub struct LockGuard<'a> {
    lock: &'a MaintenanceLock,
    service_id: String,
    /// 只有 `acquire` 成功写下 store 标记后才是 `Some`；裸 `register`（测试，以及
    /// 下一批的重启路径在接 store 之前）保持 `None`，`Drop` 据此决定是否需要清理标记。
    store: Option<Arc<Mutex<crate::store::Store>>>,
}

/// `crate::store::Store` 没有实现 `Debug`，而测试需要对 `register` 用
/// `unwrap_err`（要求 Ok 侧实现 `Debug`）。这里手写 `Debug`，只打印 service_id，
/// 不打印 store 内容，也不暴露任何凭据。
impl std::fmt::Debug for LockGuard<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LockGuard")
            .field("service_id", &self.service_id)
            .finish()
    }
}

impl Drop for LockGuard<'_> {
    fn drop(&mut self) {
        if let Some(store) = self.store.take() {
            // 短作用域：拿到 StoreMutex 只为清掉本次自己的标记，随即释放。
            let mut store = match store.lock() {
                Ok(store) => store,
                // poison 恢复：标记只是普通数据，清理它是安全的。
                Err(poisoned) => poisoned.into_inner(),
            };
            // 清理属于尽力而为：Drop 不 panic，也不把错误抛给调用方。
            let _ = store.end_service_maintenance(&self.service_id);
        }
        // StoreMutex 已释放，此时才去释放维护 map，不会同时持两把锁。
        self.lock.release(&self.service_id);
    }
}

impl MaintenanceLock {
    /// Registers a lock unless one exists or the runtime is busy.
    /// `busy` is passed in so the state machine stays testable without an app.
    pub fn register<'a>(
        &'a self,
        service_id: &str,
        operation: &str,
        busy: bool,
    ) -> Result<LockGuard<'a>, String> {
        let mut locks = self
            .inner
            .lock()
            .map_err(|_| "维护锁暂不可用，请重启软件".to_string())?;
        if let Some(existing) = locks.get(service_id) {
            return Err(format!("该服务正在维护：{}", existing.operation));
        }
        if busy {
            return Err("运行中，先等待或停止".to_string());
        }
        locks.insert(
            service_id.to_string(),
            LockState {
                operation: operation.to_string(),
                acquired_at: now_millis(),
            },
        );
        Ok(LockGuard {
            lock: self,
            service_id: service_id.to_string(),
            store: None,
        })
    }

    /// Releases one service. A future batch that restarts child processes must
    /// refresh that runtime's snapshot/state here before returning.
    pub fn release(&self, service_id: &str) {
        if let Ok(mut locks) = self.inner.lock() {
            locks.remove(service_id);
        }
    }

    /// 该服务此刻是否被本进程的维护租约占住。map 中毒时保守返回 `true`：宁可当作
    /// 维护中，也不谎报空闲。只读判断，不占锁、不改状态。
    pub fn held(&self, service_id: &str) -> bool {
        match self.inner.lock() {
            Ok(locks) => locks.contains_key(service_id),
            Err(_) => true,
        }
    }
}

fn now_millis() -> u64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(elapsed) => elapsed.as_millis() as u64,
        Err(_) => 0,
    }
}

fn now_ms() -> String {
    now_millis().to_string()
}

fn ensure_managed(service_id: &str) -> Result<&'static str, String> {
    MANAGED
        .iter()
        .find(|(id, _)| *id == service_id)
        .map(|(_, name)| *name)
        .ok_or_else(|| format!("未知服务：{service_id}"))
}

/// Codex exposes its installed version only through a fast `--version` probe.
fn codex_installed() -> (Option<String>, &'static str) {
    let Some(executable) = crate::codex::discover_executable() else {
        return (None, "unknown");
    };
    match crate::codex::native_version(&executable) {
        Some(version) if !version.trim().is_empty() => (Some(version), "executable"),
        _ => (None, "unknown"),
    }
}

/// DSH installs ship a package.json inside this app's own install slot; read its
/// version without connecting. A missing/broken manifest stays `unknown` instead
/// of falling back to some external installation path.
fn dsh_installed(data: &Path) -> (Option<String>, &'static str) {
    let Ok(root) = crate::service_install::dsh_path(data) else {
        return (None, "invalid-managed-state");
    };
    let Ok(text) = std::fs::read_to_string(root.join("package.json")) else {
        return (None, "unknown");
    };
    let version = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|value| value["version"].as_str().map(str::to_owned))
        .filter(|version| !version.trim().is_empty());
    match version {
        Some(version) => (Some(version), "manifest"),
        None => (None, "unknown"),
    }
}

/// A runtime is busy while connecting or while a run is starting/running/cancelling.
fn snapshot_busy(snapshot: &crate::codex::RuntimeSnapshot) -> bool {
    snapshot.connection == "connecting"
        || snapshot.active.as_ref().is_some_and(|active| {
            matches!(
                active.record.status.as_str(),
                "starting" | "running" | "cancelling"
            )
        })
}

/// Normalizes a runtime snapshot; `version` is only the handshake version.
/// `installed` carries the installation version and where it came from, so a
/// missing detection stays honestly `None` / `unknown`.
pub fn from_snapshot(
    id: &str,
    name: &str,
    snapshot: &crate::codex::RuntimeSnapshot,
    installed: (Option<String>, &'static str),
) -> ServiceInfo {
    let busy = snapshot_busy(snapshot);

    let (installed_version, installed_source) = installed;
    let version_source = if installed_version.is_some() {
        installed_source.to_string()
    } else if snapshot.version.is_some() {
        "handshake".to_string()
    } else {
        "unknown".to_string()
    };

    ServiceInfo {
        id: id.to_string(),
        name: name.to_string(),
        connection: snapshot.connection.clone(),
        executable: snapshot.executable.clone(),
        runtime_version: snapshot.version.clone(),
        installed_version,
        version_source,
        busy,
        // Update checking is not implemented yet, so never claim a fake "latest".
        update_state: "unchecked".to_string(),
    }
}

/// Re-runs the existing detection for one service: no download, no install
/// change, only a fresh snapshot plus the installed-version probe.
///
/// 追加两层真实状态：本进程维护 map 里的租约（连接 / 检查 / 准备 / 切换 / 重启 /
/// 回滚）一律算忙并报 `maintenance`；DSH 非维护时按受管 state 报 `managed` 或
/// `external`，state 读不出来就 Err，绝不把未知状态标成 managed 成功。
pub fn detect(app: &tauri::AppHandle, service_id: &str) -> Result<ServiceInfo, String> {
    let name = ensure_managed(service_id)?;
    let (snapshot, installed) = match service_id {
        "codex-win" => (
            app.state::<Arc<crate::codex::Runtime>>().snapshot(),
            codex_installed(),
        ),
        "hermes-win" => (
            app.state::<Arc<crate::hermes::Runtime>>().snapshot(),
            (None, "unknown"),
        ),
        "dsh-win" => {
            // 已激活槽也走这里：安装版本从本 app 的安装槽 manifest 读，读不到就受控地
            // 报 unknown，绝不回退到外部安装目录去猜。
            let installed = match crate::service_updates::data_directory(app) {
                Ok(data) => dsh_installed(&data),
                Err(_) => (None, "unknown"),
            };
            (
                app.state::<Arc<crate::dsh::Runtime>>().snapshot(),
                installed,
            )
        }
        _ => (
            app.state::<crate::albion::Runtime>().0.snapshot(),
            (None, "unknown"),
        ),
    };
    let mut info = from_snapshot(service_id, name, &snapshot, installed);
    // 本进程维护 map 里的租约同样算忙：维护期间如实报 `maintenance`，不伪装成普通运行中。
    let maintenance = app.state::<MaintenanceLock>().held(service_id);
    info.busy = info.busy || maintenance;
    if maintenance {
        info.update_state = "maintenance".to_string();
    } else if service_id == "dsh-win" {
        // 受管 state 未知 / 损坏时原样 Err：绝不把它标成受管安装的成功状态。
        let data = crate::service_updates::data_directory(app)?;
        info.update_state = match crate::service_install::read_state(&data)?.active {
            Some(_) => "managed".to_string(),
            None => "external".to_string(),
        };
    }
    Ok(info)
}

fn runtime_busy(app: &tauri::AppHandle, service_id: &str) -> bool {
    match service_id {
        "codex-win" => snapshot_busy(&app.state::<Arc<crate::codex::Runtime>>().snapshot()),
        "hermes-win" => snapshot_busy(&app.state::<Arc<crate::hermes::Runtime>>().snapshot()),
        "dsh-win" => snapshot_busy(&app.state::<Arc<crate::dsh::Runtime>>().snapshot()),
        "albion-wsl" => snapshot_busy(&app.state::<crate::albion::Runtime>().0.snapshot()),
        _ => false,
    }
}

/// Takes the maintenance lock for one service.
///
/// 顺序固定：(1) `ensure_managed`；(2) `register` 占住本进程的维护 map；
/// (3) 在短 StoreMutex 作用域内 `begin_service_maintenance`，由 store 原子标记该
/// 成员已被维护占用 —— 这一个原子标记同时覆盖运行、项目任务与群讨论发言三类入口；
/// (4) StoreMutex 释放之后，再查 runtime 快照的 connecting / 活动 run，以及
/// `projects::Runtime::agent_project_busy`，挡住还在运行的项目 attempt。
///
/// 第 (3) 步失败（StoreMutex 中毒同样算失败）就直接返回 Err：此时 `guard` 里还没有
/// store，`Drop` 不会去清理标记，因此不可能误清别人写下的标记，只释放维护 map。
/// 第 (4) 步命中 busy 也返回 Err，`Drop` 会先清掉本次自己的标记再释放维护 map。
///
/// ponytail: 运行 / 项目 / 群讨论的受理入口已接齐 `acquire`，并与 store 的原子标记、
/// 这里的 `agent_project_busy` 一起挡住并发；主动断开走 `acquire_shutdown`（放行
/// privateRun，好让本 app 停掉自己的私聊会话），整服务重启走 `acquire(..., "restart")`。
/// 命令层的 handle / UI 接线留到下一批。
pub fn acquire<'a>(
    app: &tauri::AppHandle,
    lock: &'a MaintenanceLock,
    service_id: &str,
    operation: &str,
) -> Result<LockGuard<'a>, String> {
    ensure_managed(service_id)?;
    let mut guard = lock.register(service_id, operation, false)?;
    let store = app.state::<crate::AppState>().store.clone();
    {
        // store 中毒说明维护标记可能只写了一半：宁可拒绝进入维护，也不继续用它。
        let mut store_guard = store
            .lock()
            .map_err(|_| "项目状态暂不可用，请重启软件".to_string())?;
        store_guard.begin_service_maintenance(service_id)?;
    }
    // 标记成功写入后 guard 才接管 store，`Drop` 才会负责清理它。
    guard.store = Some(store);
    if runtime_busy(app, service_id) {
        return Err("运行中，先等待或停止".to_string());
    }
    // 原子标记落定后再查项目运行时：命中就返回 Err，`Drop` 会清掉本次标记，
    // 不会因为一次维护请求而留下悬挂的项目 attempt。
    if app
        .state::<Arc<crate::projects::Runtime>>()
        .agent_project_busy(service_id)
    {
        return Err("该项目成员正在执行项目任务，请等待完成或先停止".to_string());
    }
    Ok(guard)
}

/// Takes the maintenance lock for this app's own service shutdown / disconnect.
///
/// 顺序与 `acquire` 一致：(1) `ensure_managed`；(2) `register` 占住本进程维护 map；
/// (3) 在短 StoreMutex 作用域内 `begin_service_shutdown`，由 store 原子标记该成员；
/// (4) StoreMutex 释放后，只兜底查 `projects::Runtime::agent_project_busy`。
///
/// `begin_service_shutdown` 与 maintenance 一样严格挡 project / group 忙态，但**放行**
/// privateRun：主动断开本来就是要停掉本 app 自己的私聊会话，所以这里不再查 runtime
/// 快照的 connecting / 活动 run（那是 `acquire` 的严格门禁），也不用 `runtime_busy`
/// 拒绝私聊忙态。
///
/// 第 (3) 步失败（含 StoreMutex 中毒）直接返回 Err：此时 `guard.store` 仍是 `None`，
/// `Drop` 不会去清任何标记，只释放维护 map，因此不可能误清别人写下的 flag。
/// 第 (4) 步命中 busy 也返回 Err，`Drop` 会先清掉本次自己的标记再释放维护 map。
/// 三个 `disconnect_*` 与 `albion::disconnect_albion` 已接到这里。
pub fn acquire_shutdown<'a>(
    app: &tauri::AppHandle,
    lock: &'a MaintenanceLock,
    service_id: &str,
) -> Result<LockGuard<'a>, String> {
    ensure_managed(service_id)?;
    // 维护 map 已有同 service 的 connect / restart 时，这里就会拒绝。
    let mut guard = lock.register(service_id, "disconnect", false)?;
    let store = app.state::<crate::AppState>().store.clone();
    {
        // store 中毒说明标记可能只写了一半：宁可拒绝断开，也不带着半截状态继续。
        let mut store_guard = store
            .lock()
            .map_err(|_| "项目状态暂不可用，请重启软件".to_string())?;
        store_guard.begin_service_shutdown(service_id)?;
    }
    // 标记成功写入后 guard 才接管 store，`Drop` 才会负责清理它。
    guard.store = Some(store);
    // 断开也不能踩正在跑的项目 attempt；命中就返回 Err，`Drop` 清掉本次标记。
    if app
        .state::<Arc<crate::projects::Runtime>>()
        .agent_project_busy(service_id)
    {
        return Err("该项目成员正在执行项目任务，请等待完成或先停止".to_string());
    }
    Ok(guard)
}

/// Why a member without a managed install layout cannot be updated in place yet. One honest sentence each.
///
/// 有受管布局的成员（`layout_for` 命中）不走这里：它们走 prepare / apply / rollback。
fn update_blocker(service_id: &str) -> Result<&'static str, String> {
    ensure_managed(service_id)?;
    Ok(match service_id {
        "hermes-win" => {
            "Hermes 由它自己的更新器管理：同席只做只读的远程核对，更新交接给它自己的通道。"
        }
        "albion-wsl" => {
            "Albion 位于 WSL 内并带有本地定制改动，直接覆盖会丢失定制，暂不支持自动更新。"
        }
        _ => return Err(format!("该服务暂不支持受管更新：{service_id}")),
    })
}

/// Re-detects one service while holding its maintenance lock, then asks the proper source
/// (npm registry, Hermes updater, or local runtime) for a current update check.
///
/// 严格 `acquire(check)` 租约覆盖 detect 与远程查询全程：`MaintenanceLock` 与整段
/// 工作都在阻塞线程上跑，网络 IO 不会卡住 async 命令，也不在任何 StoreMutex 作用域内
/// （`acquire` 早已释放 store 锁），因此不存在"持 store 锁做网络 IO"。
/// DSH / Codex 有应用内隔离安装槽和回滚；Hermes 使用自己的更新通道，Albion 只核对本地版本。
/// 各自的 `latest_version` / `update_available` 与 `check_source` 只在真实数据可得时填写，
/// 不把本地版本伪装成远程发行版。
#[tauri::command]
pub async fn check_service_update(
    app: tauri::AppHandle,
    service_id: String,
) -> Result<CheckResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let lock = app.state::<MaintenanceLock>();
        let _guard = acquire(&app, lock.inner(), &service_id, "check")?;
        let info = detect(&app, &service_id)?;
        // DSH / Codex 从 npm 安装并使用隔离槽；Hermes 是从源码构建的桌面端，走自己的更新通道。
        // Hermes 的更新按钮交接给它自己的更新通道（见 service_updates::check_hermes 的说明）。
        let (latest_version, update_available, check_source, note, installed_override) =
            match service_id.as_str() {
                "dsh-win" => {
                    let data = crate::service_updates::data_directory(&app)?;
                    let (metadata, latest_is_newer) = crate::service_updates::check_release(
                        &data,
                        crate::service_updates::EXPECTED_PACKAGE,
                        info.installed_version.as_deref(),
                    )?;
                    (
                        Some(metadata.version),
                        latest_is_newer,
                        "npm-registry",
                        "已核对 npm 发行版本".to_string(),
                        None,
                    )
                }
                "codex-win" => {
                    let data = crate::service_updates::data_directory(&app)?;
                    let (metadata, latest_is_newer) = crate::service_updates::check_release(
                        &data,
                        crate::service_updates::CODEX_PACKAGE,
                        info.installed_version.as_deref(),
                    )?;
                    (
                        Some(metadata.version),
                        latest_is_newer,
                        "npm-registry",
                        "已核对 npm 发行版本".to_string(),
                        None,
                    )
                }
                "hermes-win" => match crate::service_updates::hermes_repo() {
                    Some(repo) => {
                        let (installed, available, note) =
                            crate::service_updates::check_hermes(&repo)?;
                        (None, available, "hermes-update", note, installed)
                    }
                    None => (
                        None,
                        None,
                        "local",
                        "未找到 Hermes 的源码检出，仅核对本地版本".to_string(),
                        None,
                    ),
                },
                _ => (
                    None,
                    None,
                    "local",
                    "仅核对本地版本，尚未查询远程发行".to_string(),
                    None,
                ),
            };
        Ok(CheckResult {
            service_id: info.id,
            checked_at: now_ms(),
            runtime_version: info.runtime_version,
            installed_version: installed_override.or(info.installed_version),
            version_source: info.version_source,
            latest_version,
            update_available,
            check_source: check_source.to_string(),
            note,
        })
    })
    .await
    .map_err(|_| "检查任务未正常完成，请重试".to_string())?
}

/// 更新准备：DSH / Codex 走真实受管流程，Hermes / Albion 返回各自的通道限制。
///
/// 整段工作（含候选隔离安装、完整指纹与成员原生预检）都在同一个
/// `acquire(prepare)` 租约内，且整体跑在阻塞线程上：网络 / 磁盘 IO 不会卡住 async
/// 命令，也不在任何 StoreMutex 作用域内（`acquire` 返回前早已释放 store 锁）。
///
/// 候选准备成功才回报 `supported: true` 与持久计划 id / 候选版本；任何失败都保留受控 `Err`，
/// 绝不包装成"可切换"的 ready 计划。Hermes / Albion 保持 `supported: false` 且不下载候选。
#[tauri::command]
pub async fn prepare_service_update(
    app: tauri::AppHandle,
    service_id: String,
) -> Result<PlanResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let lock = app.state::<MaintenanceLock>();
        // 整个准备任务都由这一份 prepare 租约覆盖，成功后到这里才释放。
        let _guard = acquire(&app, lock.inner(), &service_id, "prepare")?;
        if let Some(layout) = crate::service_install::layout_for(&service_id) {
            // 真实准备：隔离安装候选 + 完整指纹 + 该成员的原生预检 + 持久计划。
            let data = crate::service_updates::data_directory(&app)?;
            let plan = crate::service_updates::prepare_member(&data, &layout)?;
            return Ok(PlanResult {
                service_id,
                supported: true,
                reason: "候选安装与原生预检通过，尚未切换".to_string(),
                plan_id: Some(plan.id),
                candidate_version: Some(plan.metadata.version),
                created_at: plan.created_at.to_string(),
            });
        }
        let reason = update_blocker(&service_id)?;
        Ok(PlanResult {
            service_id,
            supported: false,
            reason: reason.to_string(),
            plan_id: None,
            candidate_version: None,
            created_at: now_ms(),
        })
    })
    .await
    .map_err(|_| "准备任务未正常完成，请重试".to_string())?
}

/// 从槽自己的 `package.json` manifest 读版本；路径或 JSON 坏掉一律 `None`：
/// 读不出来就不算版本成功，绝不回退到别处去猜一个版本出来。
fn slot_manifest_version(root: &Path, package: &str) -> Option<String> {
    let version = crate::service_updates::manifest_at(root, package).ok()?.0;
    (!version.trim().is_empty()).then_some(version)
}

/// 受管更新的只读状态：生效版本、可回滚版本，以及与当前 state 对齐的 ready 计划。
///
/// 只读本机安装槽与计划目录：不 fetch 远程版本、不下载、不跑 prepare 的整树 fingerprint
/// 预演（UI 轮询因此不会被卡住），不碰任何私聊内容，只回报版本 / id，不回传计划 JSON 原文。
/// 受管 state 自身损坏（`read_state` Err）与计划目录读取失败都原样返回 Err，不用空状态
/// 掩盖问题；非 DSH（以及没有受管槽的 DSH）如实回报 `managed: false`。
#[tauri::command]
pub fn service_update_status(
    app: tauri::AppHandle,
    service_id: String,
) -> Result<UpdateStatus, String> {
    ensure_managed(&service_id)?;
    // 没有受管安装槽的成员：不替它们编造受管状态。
    let Some(layout) = crate::service_install::layout_for(&service_id) else {
        return Ok(UpdateStatus {
            service_id,
            managed: false,
            active_version: None,
            previous_version: None,
            ready_plan_id: None,
            candidate_version: None,
            rollback_available: false,
        });
    };
    let data = crate::service_updates::data_directory(&app)?;
    managed_update_status(&data, &layout, &service_id)
}

/// 受管成员自己的安装 / 候选 / 回滚状态（按布局，DSH 与 Codex 共用）。
fn managed_update_status(
    data: &Path,
    layout: &crate::service_install::Layout,
    service_id: &str,
) -> Result<UpdateStatus, String> {
    let state = crate::service_install::read_state_for(data, layout)?;
    let managed = state.active.is_some();
    // 生效版本只认当前槽自己的 manifest；没有受管槽就没有版本可报。
    let active_version = if managed {
        let root = crate::service_install::package_path_for(data, layout)?;
        slot_manifest_version(&root, layout.package)
    } else {
        None
    };
    // ready 计划：base_state 的 revision / active / previous 必须与当前 state 全等，
    // 且不能是已经生效的那个 plan；命中多个时取最近创建的一个。
    let candidate = crate::service_plans::list_plans(data, layout)?
        .into_iter()
        .filter(|plan| {
            state.active.as_deref() != Some(plan.id.as_str())
                && plan.base_state.revision == state.revision
                && plan.base_state.active == state.active
                && plan.base_state.previous == state.previous
        })
        .max_by_key(|plan| plan.created_at);
    let (ready_plan_id, candidate_version) = match candidate {
        Some(plan) => (Some(plan.id), Some(plan.metadata.version)),
        None => (None, None),
    };
    // receipt 长期有效：active 槽的 receipt 读得出来、且回滚目标确实读得出来（外部安装
    // 还要 manifest 指纹与 receipt 记录一致），才算能回滚，不做过期判断。
    let mut previous_version = None;
    let mut rollback_available = false;
    if let Some(active_id) = state.active.as_deref() {
        if let Ok(receipt) = crate::service_plans::load_receipt(data, layout, active_id) {
            let previous = match receipt.base_state.active.as_deref() {
                // 槽根下的 node_modules 包目录才是 manifest 目录，槽根不是包。
                Some(old_id) => {
                    let root = crate::service_install::slot_package_for(data, layout, old_id)?;
                    slot_manifest_version(&root, layout.package)
                }
                // 第一次受管激活：被换掉的是外部安装。
                // 只有它的 manifest 指纹仍与 receipt 记录一致，退回的才是原来那一份。
                None => crate::service_install::external(layout)
                    .ok()
                    .and_then(|external| {
                        crate::service_updates::manifest_at(&external, layout.package)
                            .ok()
                            .filter(|(_, sha256)| *sha256 == receipt.base_manifest_sha256)
                            .and_then(|(version, _)| {
                                (!version.trim().is_empty()).then_some(version)
                            })
                    }),
            };
            // 读不出来就只有 None / false：绝不把读失败的槽标成可回滚。
            if let Some(version) = previous {
                previous_version = Some(version);
                rollback_available = true;
            }
        }
    }
    Ok(UpdateStatus {
        service_id: service_id.to_string(),
        managed,
        active_version,
        previous_version,
        ready_plan_id,
        candidate_version,
        rollback_available,
    })
}

/// 阻塞线程里做的整服务重启：停掉本 app 自己持有的运行时，再重新连接。
///
/// 只碰本进程 own 的 runtime（Codex / Hermes / DSH / Albion）不杀外部 gateway，
/// 也不动系统服务。严格租约在 stop + connect 全程持有，`_guard` 到函数结束（快照
/// 已经在手）才释放。连接失败就把运行时给出的真实错误原样返回，不假报成功。
/// 已由已注册的 `restart_service` 命令在阻塞线程上调用。
fn restart_blocking(
    app: tauri::AppHandle,
    service_id: String,
) -> Result<crate::codex::RuntimeSnapshot, String> {
    let lock = app.state::<MaintenanceLock>();
    // `acquire` 会写 store 标记并挡 project / group / private；未知 id 也在这里被拒。
    let _guard = acquire(&app, lock.inner(), &service_id, "restart")?;
    let snapshot = match service_id.as_str() {
        "codex-win" => {
            let runtime = app.state::<Arc<crate::codex::Runtime>>();
            runtime.stop();
            runtime.connect()?;
            runtime.snapshot()
        }
        "hermes-win" => {
            let runtime = app.state::<Arc<crate::hermes::Runtime>>();
            runtime.stop();
            runtime.connect()?;
            runtime.snapshot()
        }
        "dsh-win" => {
            let runtime = app.state::<Arc<crate::dsh::Runtime>>();
            runtime.stop();
            runtime.connect()?;
            runtime.snapshot()
        }
        // 未知 id 已被上面的 `acquire` 拒掉，这里只剩 albion：runtime 放在 `.0`。
        _ => {
            let runtime = app.state::<crate::albion::Runtime>();
            runtime.0.stop();
            runtime.0.connect()?;
            runtime.0.snapshot()
        }
    };
    Ok(snapshot)
}

/// 维护门禁内的整服务重启，工作放在阻塞线程上，避免卡住 async 命令。
/// 失败时返回真实的连接错误；已在 `main.rs` 的 invoke handler 注册，服务页重启按钮调用。
#[tauri::command]
pub async fn restart_service(
    app: tauri::AppHandle,
    service_id: String,
) -> Result<crate::codex::RuntimeSnapshot, String> {
    tauri::async_runtime::spawn_blocking(move || restart_blocking(app, service_id))
        .await
        .map_err(|_| "重启任务未正常完成，请重试".to_string())?
}

/// 受管更新的切换命令：只有拥有受管 Layout 的成员可启用候选。
///
/// 未知成员与无受管 Layout 的成员如实拒绝。DSH / Codex 的切换整体交给
/// `crate::service_apply::apply_member`，它自己持严格租约覆盖全程，所以这里**不**再 `acquire`，
/// 否则同一租约自我冲突会死锁。
/// 切换跑在阻塞线程上，网络 / 磁盘 IO 不卡 async 命令；join 失败只报固定中文，
/// 内层 `Result` 原样返回，运行时的真实错误不被包装掉。
#[tauri::command]
pub async fn apply_service_update(
    app: tauri::AppHandle,
    service_id: String,
    plan_id: String,
) -> Result<crate::codex::RuntimeSnapshot, String> {
    let Some(layout) = crate::service_install::layout_for(&service_id) else {
        return Err(update_blocker(&service_id)?.to_string());
    };
    tauri::async_runtime::spawn_blocking(move || {
        crate::service_apply::apply_member(&app, &layout, &plan_id)
    })
    .await
    .map_err(|_| "更新任务未正常完成，请重试".to_string())?
}

/// 受管更新的回滚命令：只有拥有受管 Layout 的成员可回滚。
///
/// 无受管 Layout 的成员如实拒绝。DSH / Codex 的真实回滚整体交给 `rollback_member`：它自己持严格
/// 租约覆盖全程，所以这里**不**再 `acquire`，否则同一租约自我冲突会死锁。回滚跑在
/// 阻塞线程上，磁盘 IO 不卡 async 命令；join 失败只报固定中文，内层 `Result` 原样
/// 返回，真实错误不被包装掉。
#[tauri::command]
pub async fn rollback_service_update(
    app: tauri::AppHandle,
    service_id: String,
) -> Result<crate::codex::RuntimeSnapshot, String> {
    let Some(layout) = crate::service_install::layout_for(&service_id) else {
        return Err(update_blocker(&service_id)?.to_string());
    };
    tauri::async_runtime::spawn_blocking(move || {
        crate::service_apply::rollback_member(&app, &layout)
    })
    .await
    .map_err(|_| "回滚任务未正常完成，请重试".to_string())?
}

#[tauri::command]
pub fn service_inventory(app: tauri::AppHandle) -> Vec<ServiceInfo> {
    MANAGED
        .iter()
        .filter_map(|(id, _)| detect(&app, id).ok())
        .collect()
}

/// Header of the public development digest.
const DIGEST_HEADER: &str =
    "以下是同席项目的公共开发动态（可能不是最新，仅供了解），不是用户私聊内容。";

/// Builds the public development digest of recent project activity.
///
/// 只读 store 里的共享（跨项目）workflow 状态，绝不读私聊内容。失败关闭：
/// `Ok(空)` 与 `Err` 一律返回空 String，调用方因此不会注入任何内容；只有确实存在
/// 共享 workflow 时才渲染 digest。
pub fn dev_digest(store: &crate::store::Store) -> String {
    match store.shared_workflows() {
        Ok(workflows) if !workflows.is_empty() => digest_from_workflows(&workflows),
        Ok(_) | Err(_) => String::new(),
    }
}

/// Keeps at most `limit` characters, never splitting a UTF-8 code point.
fn snippet(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

fn workflow_status_zh(status: &str) -> &'static str {
    match status {
        "queued" => "排队中",
        "planning" => "制定方案中",
        "running" => "执行中",
        "verifying" => "检查中",
        "reviewing" => "验收中",
        "cancelling" => "停止中",
        "completed" => "已完成",
        "failed" => "已失败",
        "interrupted" => "已中断",
        _ => "未知",
    }
}

fn stage_zh(stage: &str) -> &'static str {
    match stage {
        "plan" => "方案",
        "implement" => "实现",
        "verify" => "检查",
        "review" => "验收",
        "repair" => "修复",
        _ => "其他",
    }
}

fn attempt_status_zh(status: &str) -> &'static str {
    match status {
        "starting" | "running" | "cancelling" => "进行中",
        "completed" => "完成",
        "failed" => "失败",
        "interrupted" => "中断",
        _ => "未知",
    }
}

/// Renders the digest: newest two live workflows plus the newest three finished
/// ones, one line each. An empty list says so without the header.
fn digest_from_workflows(workflows: &[crate::project_store::Workflow]) -> String {
    const LIVE: [&str; 6] = [
        "queued",
        "planning",
        "running",
        "verifying",
        "reviewing",
        "cancelling",
    ];
    let is_live = |status: &str| LIVE.contains(&status);
    let mut live: Vec<&crate::project_store::Workflow> = workflows
        .iter()
        .filter(|workflow| is_live(workflow.status.as_str()))
        .collect();
    let mut finished: Vec<&crate::project_store::Workflow> = workflows
        .iter()
        .filter(|workflow| !is_live(workflow.status.as_str()))
        .collect();
    live.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| b.id.cmp(&a.id))
    });
    finished.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| b.id.cmp(&a.id))
    });
    live.truncate(2);
    finished.truncate(3);

    let mut lines = vec![DIGEST_HEADER.to_string()];
    for workflow in live.into_iter().chain(finished) {
        let mut line = format!(
            "项目：{}｜状态：{}",
            snippet(workflow.request.trim(), 80),
            workflow_status_zh(&workflow.status)
        );
        let summary = workflow.summary.trim();
        if !summary.is_empty() {
            line.push_str(&format!("｜成果：{}", snippet(summary, 100)));
        }
        if let Some(error) = workflow
            .error
            .as_deref()
            .map(str::trim)
            .filter(|error| !error.is_empty())
        {
            line.push_str(&format!("｜问题：{}", snippet(error, 100)));
        }
        if let Some(attempt) = workflow.attempts.last() {
            line.push_str(&format!(
                "（{} {} {}）",
                attempt.agent_id,
                stage_zh(&attempt.stage),
                attempt_status_zh(&attempt.status)
            ));
        }
        lines.push(line);
    }
    if lines.len() == 1 {
        return "当前没有进行中的项目协作。".to_string();
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(connection: &str, version: Option<&str>) -> crate::codex::RuntimeSnapshot {
        crate::codex::RuntimeSnapshot {
            revision: 0,
            connection: connection.into(),
            executable: None,
            version: version.map(str::to_owned),
            error: None,
            active: None,
            models: Vec::new(),
            default_model: None,
            default_effort: None,
        }
    }

    #[test]
    fn hermes_keeps_installed_version_unknown() {
        let info = from_snapshot(
            "hermes-win",
            "Hermes",
            &snapshot("disconnected", None),
            (None, "unknown"),
        );
        assert_eq!(info.installed_version, None);
        assert_eq!(info.version_source, "unknown");
        assert_eq!(info.runtime_version, None);
        assert_eq!(info.update_state, "unchecked");
        assert!(!info.busy);
    }

    #[test]
    fn installed_metadata_wins_over_handshake_source() {
        let info = from_snapshot(
            "dsh-win",
            "DSH",
            &snapshot("disconnected", Some("9.9.9")),
            (Some("1.2.3".to_string()), "manifest"),
        );
        assert_eq!(info.installed_version.as_deref(), Some("1.2.3"));
        assert_eq!(info.version_source, "manifest");
        assert_eq!(info.runtime_version.as_deref(), Some("9.9.9"));

        let handshake = from_snapshot(
            "hermes-win",
            "Hermes",
            &snapshot("connecting", Some("0.1.0")),
            (None, "unknown"),
        );
        assert_eq!(handshake.installed_version, None);
        assert_eq!(handshake.version_source, "handshake");
        assert!(handshake.busy);
    }

    #[test]
    fn second_acquire_for_the_same_service_is_rejected() {
        let lock = MaintenanceLock::default();
        let _first = lock.register("codex-win", "check", false).unwrap();
        let error = lock.register("codex-win", "prepare", false).unwrap_err();
        assert!(error.contains("该服务正在维护"));
        assert!(error.contains("check"));
    }

    #[test]
    fn locks_on_different_services_do_not_conflict() {
        let lock = MaintenanceLock::default();
        let _codex = lock.register("codex-win", "check", false).unwrap();
        let _hermes = lock.register("hermes-win", "check", false).unwrap();
        assert_eq!(lock.inner.lock().unwrap().len(), 2);
    }

    #[test]
    fn busy_runtime_is_rejected_without_leaving_a_lock_behind() {
        let lock = MaintenanceLock::default();
        let error = lock.register("dsh-win", "check", true).unwrap_err();
        assert_eq!(error, "运行中，先等待或停止");
        assert!(lock.register("dsh-win", "check", false).is_ok());
    }

    #[test]
    fn dropping_the_guard_releases_the_lock() {
        let lock = MaintenanceLock::default();
        {
            let _guard = lock.register("albion-wsl", "prepare", false).unwrap();
            assert!(lock.register("albion-wsl", "check", false).is_err());
        }
        assert!(lock.register("albion-wsl", "check", false).is_ok());
    }

    #[test]
    fn unknown_service_id_is_rejected() {
        let error = ensure_managed("nope").unwrap_err();
        assert!(error.contains("未知服务"));
        assert!(update_blocker("nope").is_err());
        assert_eq!(ensure_managed("hermes-win").unwrap(), "Hermes");
    }

    #[test]
    fn every_managed_service_is_either_updatable_or_has_a_blocker_reason() {
        for (id, _) in MANAGED {
            let managed = crate::service_install::layout_for(id).is_some();
            match update_blocker(id) {
                Ok(reason) => assert!(!reason.trim().is_empty()),
                Err(_) => assert!(managed, "{id} 既没有受管布局，也没有一句说明"),
            }
        }
    }

    fn workflow(
        id: &str,
        status: &str,
        updated_at: i64,
        request: &str,
        summary: &str,
        error: Option<&str>,
        attempts: Vec<crate::project_store::Attempt>,
    ) -> crate::project_store::Workflow {
        crate::project_store::Workflow {
            id: id.to_string(),
            project_id: "project".to_string(),
            conversation_id: "room".to_string(),
            user_message_id: format!("message-{id}"),
            request: request.to_string(),
            status: status.to_string(),
            plan: None,
            roles: None,
            summary: summary.to_string(),
            error: error.map(str::to_owned),
            created_at: 0,
            updated_at,
            tasks: Vec::new(),
            attempts,
            changes: Vec::new(),
        }
    }

    fn attempt(agent: &str, stage: &str, status: &str) -> crate::project_store::Attempt {
        crate::project_store::Attempt {
            id: format!("attempt-{agent}-{stage}"),
            workflow_id: "workflow".to_string(),
            task_id: None,
            agent_id: agent.to_string(),
            stage: stage.to_string(),
            status: status.to_string(),
            native_thread_id: None,
            native_turn_id: None,
            model: None,
            reasoning_effort: None,
            output: String::new(),
            checks: Vec::new(),
            error: None,
        }
    }

    #[test]
    fn digest_empty_workflows_are_declared() {
        let digest = digest_from_workflows(&[]);
        assert_eq!(digest, "当前没有进行中的项目协作。");
    }

    #[test]
    fn digest_caps_live_and_finished_workflows() {
        let workflows = vec![
            workflow("live-10", "queued", 10, "需求 10", "", None, Vec::new()),
            workflow("live-20", "planning", 20, "需求 20", "", None, Vec::new()),
            workflow(
                "live-30",
                "running",
                30,
                "需求 30",
                "已完成一部分",
                None,
                Vec::new(),
            ),
            workflow(
                "live-40",
                "verifying",
                40,
                "需求 40",
                "",
                None,
                vec![attempt("codex-win", "verify", "running")],
            ),
            workflow(
                "done-5",
                "completed",
                5,
                "需求 5",
                "全部通过",
                None,
                Vec::new(),
            ),
            workflow(
                "done-6",
                "failed",
                6,
                "需求 6",
                "",
                Some("构建失败"),
                Vec::new(),
            ),
            workflow("done-7", "interrupted", 7, "需求 7", "", None, Vec::new()),
        ];
        let digest = digest_from_workflows(&workflows);
        assert!(digest.starts_with("以下是同席项目的公共开发动态"));
        assert!(digest.contains("需求 40"));
        assert!(digest.contains("需求 30"));
        assert!(!digest.contains("需求 20"));
        assert!(!digest.contains("需求 10"));
        assert!(digest.contains("需求 7"));
        assert!(digest.contains("需求 6"));
        assert!(digest.contains("需求 5"));
        assert!(digest.contains("状态：执行中"));
        assert!(digest.contains("成果：全部通过"));
        assert!(digest.contains("问题：构建失败"));
        assert!(digest.contains("（codex-win 检查 进行中）"));
        assert_eq!(digest.lines().count(), 6);
    }
}
