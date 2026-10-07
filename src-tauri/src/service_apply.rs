//! Managed apply/rollback for this app's own member installs (`service_apply`).
//!
//! Scope is deliberately narrow: the managed slot from `service_install::slot_root_for`,
//! the install-state pointer written by `service_install::write_state_for` and the member's
//! own runtime inside this app (dsh / codex). The external install is never overwritten,
//! SQLite / nativehistory are never modified and no process is killed by name.
//!
//! DSH 与 Codex 的切换事务完全一样，只有「布局」和「运行时句柄」不同，所以事务只写一份，
//! 运行时用 `MemberRuntime` 适配：两套安全逻辑不会各自漂移。

use std::path::Path;
use std::sync::Arc;

use tauri::Manager;

use crate::codex::RuntimeSnapshot;
use crate::service_install::Layout;
use crate::{codex, dsh, service_install, service_plans, service_updates, services};

/// 成员在自己进程里的运行时句柄：切换槽位前停掉它、切完再连上并按版本核对。
///
/// 两个运行时的快照是同一个 `RuntimeSnapshot` 类型，所以事务本身不为成员分叉。
enum MemberRuntime {
    Dsh(Arc<dsh::Runtime>),
    Codex(Arc<codex::Runtime>),
}

impl MemberRuntime {
    fn handle(app: &tauri::AppHandle, layout: &Layout) -> Result<Self, String> {
        match layout.slot {
            "dsh-win" => Ok(Self::Dsh(app.state::<Arc<dsh::Runtime>>().inner().clone())),
            "codex-win" => Ok(Self::Codex(
                app.state::<Arc<codex::Runtime>>().inner().clone(),
            )),
            _ => Err("该服务暂不支持受管更新".to_string()),
        }
    }

    fn snapshot(&self) -> RuntimeSnapshot {
        match self {
            Self::Dsh(runtime) => runtime.snapshot(),
            Self::Codex(runtime) => runtime.snapshot(),
        }
    }

    fn version(&self) -> Option<String> {
        self.snapshot().version
    }

    fn is_connected(&self) -> bool {
        self.snapshot().connection == "connected"
    }

    fn stop(&self) {
        match self {
            Self::Dsh(runtime) => runtime.stop(),
            Self::Codex(runtime) => runtime.stop(),
        }
    }

    /// 连上当前成员。Codex 连之前先让发现逻辑重新解析受管槽位——刚切过槽，重连就该用新槽里那份。
    fn connect(&self, data: &Path) -> Result<RuntimeSnapshot, String> {
        match self {
            Self::Dsh(runtime) => runtime.connect(),
            Self::Codex(runtime) => {
                codex::adopt_managed_executable(data)?;
                runtime.connect()
            }
        }
    }
}

/// Install-state write was refused; the pointer still points at the previous revision.
const ERR_STATE_WRITE: &str = "安装状态写入被拒绝，指针未变更";
/// Install-state write was refused and the previous connection could not be revived.
const ERR_STATE_WRITE_RECONNECT: &str = "安装状态写入被拒绝，指针未变更，且原连接未能恢复";
/// No revision slot is left for the apply revision plus its rollback revision.
const ERR_REVISION_SPACE: &str = "安装版本号已无递增空间，指针未变更";
/// Candidate boot/version check failed and the previous revision was put back.
const ERR_ROLLED_BACK: &str = "候选校验失败已恢复";
/// Restoration after a failed apply did not complete; manual inspection required.
const ERR_ROLLBACK_FAILED: &str = "恢复失败请检查";
/// No active revision is recorded, so there is no base state to roll back to.
const ERR_ROLLBACK_NO_BASE: &str = "当前无可回退的安装版本，指针未变更";
/// The recorded base state and the installed slot disagree; refuse instead of guessing.
const ERR_ROLLBACK_TARGET: &str = "回退目标与记录不一致，拒绝回退";
/// Rollback failed, the current selection was put back and is connected again.
const ERR_ROLLBACK_RESTORED: &str = "回退失败已恢复当前版本";
/// Rollback failed and the current selection could not be put back.
const ERR_ROLLBACK_RESTORED_FAILED: &str = "回退及恢复失败";

pub(crate) fn apply_member(
    app: &tauri::AppHandle,
    layout: &Layout,
    plan_id: &str,
) -> Result<RuntimeSnapshot, String> {
    // Whole-transaction maintenance lease: held until this function returns.
    let lock = app.state::<services::MaintenanceLock>();
    let _lease = services::acquire(app, lock.inner(), layout.slot, "apply")?;

    let data = service_updates::data_directory(app)?;
    let plan = service_plans::load_plan(&data, layout, plan_id)?;
    service_updates::verify_plan_for(&data, layout, &plan)?;

    // Re-preflight exactly the staged candidate this plan points at.
    service_updates::preflight_target(
        &data,
        layout,
        service_updates::PreflightTarget::Slot(service_install::slot_root_for(
            &data, layout, plan_id,
        )?),
        &plan.metadata.version,
    )?;
    service_updates::verify_plan_for(&data, layout, &plan)?;

    let runtime = MemberRuntime::handle(app, layout)?;
    let was_connected = runtime.is_connected();

    let base = service_install::read_state_for(&data, layout)?;
    // Version the live runtime is on right now, recorded before anything moves:
    // a restore only counts as restored if the runtime comes back on this version.
    let base_version = service_updates::installed_version_for(&data, layout)?;
    // The rollback path needs a free revision slot of its own, so require room for
    // the apply revision plus the restore revision. If it is missing, refuse before
    // the runtime is stopped and before the pointer is touched at all.
    let next_revision = match base.revision.checked_add(2) {
        Some(_) => base.revision + 1,
        None => return Err(ERR_REVISION_SPACE.to_string()),
    };
    let next = service_install::InstallState {
        revision: next_revision,
        active: Some(plan.id.clone()),
        previous: base.active.clone(),
    };

    // Roll back the pointer, then report one of two fixed, non-echoing errors.
    let restore = || -> String {
        let rollback_revision = match next.revision.checked_add(1) {
            Some(revision) => revision,
            None => return ERR_ROLLBACK_FAILED.to_string(),
        };
        runtime.stop();
        let rollback = service_install::InstallState {
            revision: rollback_revision,
            active: base.active.clone(),
            previous: base.previous.clone(),
        };
        let written =
            service_install::write_state_for(&data, layout, next.revision, &rollback).is_ok();
        // The original runtime counts as restored only when it is connected on the
        // version this transaction started from, not merely when connect() is Ok.
        let revived =
            runtime.connect(&data).is_ok() && runtime.version() == Some(base_version.clone());
        if !was_connected {
            runtime.stop();
        }
        if written && revived {
            ERR_ROLLED_BACK.to_string()
        } else {
            ERR_ROLLBACK_FAILED.to_string()
        }
    };

    // Flip the pointer only while the runtime is down.
    runtime.stop();
    if service_install::write_state_for(&data, layout, base.revision, &next).is_err() {
        // Pointer unchanged: put the connection back the way it was found, and report
        // which of the two outcomes actually happened instead of assuming success.
        let mut revived = true;
        if was_connected {
            revived =
                runtime.connect(&data).is_ok() && runtime.version() == Some(base_version.clone());
        }
        return Err(if revived {
            ERR_STATE_WRITE.to_string()
        } else {
            ERR_STATE_WRITE_RECONNECT.to_string()
        });
    }

    let started = match runtime.connect(&data) {
        Ok(snapshot) => snapshot,
        Err(_) => return Err(restore()),
    };
    if started.version != Some(plan.metadata.version.clone()) {
        return Err(restore());
    }

    // Succeeded: leave the runtime disconnected if the caller had it down, then report
    // its real final state instead of the boot-time snapshot taken before the stop.
    if !was_connected {
        runtime.stop();
    }
    Ok(runtime.snapshot())
}

/// Roll the managed slot back to the base state recorded in the active plan's receipt.
///
/// Runs under the same maintenance lease as an apply and only flips the install-state
/// pointer while the runtime is down. The receipt -- not the current selection --
/// decides the target, that target is verified again right before anything moves, and
/// no failure surfaces a raw native or config error. SQLite / nativehistory, the SDK
/// source install, the install directory and external processes are never touched.
pub(crate) fn rollback_member(
    app: &tauri::AppHandle,
    layout: &Layout,
) -> Result<RuntimeSnapshot, String> {
    // Whole-transaction maintenance lease: held until this function returns.
    let lock = app.state::<services::MaintenanceLock>();
    let _lease = services::acquire(app, lock.inner(), layout.slot, "rollback")?;

    let data = service_updates::data_directory(app)?;
    let current = service_install::read_state_for(&data, layout)?;
    let active_id = match current.active.clone() {
        Some(active_id) => active_id,
        None => return Err(ERR_ROLLBACK_NO_BASE.to_string()),
    };
    // The receipt of the current selection is the only description of the base state to
    // go back to, and the current selection must still match it: `verify_receipt` rejects
    // a receipt whose plan / fingerprint no longer describes what is installed.
    let receipt = service_plans::load_receipt(&data, layout, &active_id)?;
    service_updates::verify_receipt_for(&data, layout, &receipt)?;

    // Target: a recorded revision when the base state had one, otherwise the untouched
    // external install. Either way the version we must end up on is pinned right here.
    let target_version;
    let target_slot: Option<String>;
    match &receipt.base_state.active {
        Some(old_id) => {
            let old = service_plans::load_receipt(&data, layout, old_id)?;
            service_updates::verify_receipt_for(&data, layout, &old)?;
            target_version = old.metadata.version.clone();
            target_slot = Some(old_id.clone());
        }
        None => {
            // The external install is the anchor: if it changed, rolling back would
            // install something this receipt never described, so refuse instead.
            let (version, hash) =
                service_updates::manifest_at(&service_install::external(layout)?, layout.package)?;
            if hash != receipt.base_manifest_sha256 {
                return Err(ERR_ROLLBACK_TARGET.to_string());
            }
            target_version = version;
            target_slot = None;
        }
    }

    // Preflight the target, then confirm its fingerprint once more; nothing has moved yet.
    let preflight = match &target_slot {
        Some(id) => service_updates::PreflightTarget::Slot(service_install::slot_root_for(
            &data, layout, id,
        )?),
        None => service_updates::PreflightTarget::External(service_install::external(layout)?),
    };
    service_updates::preflight_target(&data, layout, preflight, &target_version)?;
    match &receipt.base_state.active {
        Some(old_id) => {
            let old = service_plans::load_receipt(&data, layout, old_id)?;
            service_updates::verify_receipt_for(&data, layout, &old)?;
        }
        None => {
            let path = service_install::external(layout)?;
            let (_, hash) = service_updates::manifest_at(&path, layout.package)?;
            if hash != receipt.base_manifest_sha256 {
                return Err(ERR_ROLLBACK_TARGET.to_string());
            }
        }
    }

    // The failure path below needs a revision slot of its own, so require room for the
    // rollback revision plus the restore revision before the runtime is even stopped.
    let next_revision = match current.revision.checked_add(2) {
        Some(_) => current.revision + 1,
        None => return Err(ERR_REVISION_SPACE.to_string()),
    };
    // Restore the selection chain the receipt recorded, so the revision being rolled
    // back to is not mistaken for a previous selection of itself.
    let next = service_install::InstallState {
        revision: next_revision,
        active: receipt.base_state.active.clone(),
        previous: receipt.base_state.previous.clone(),
    };

    let runtime = MemberRuntime::handle(app, layout)?;
    let was_connected = runtime.is_connected();

    // Put the current selection back after a failed rollback and report one of two
    // fixed, non-echoing errors: restored only when the pointer write landed *and* the
    // runtime came back connected on the version this transaction started from.
    let rollback_failed = |cause: String| -> String {
        let restore_revision = match next.revision.checked_add(1) {
            Some(revision) => revision,
            None => return format!("{ERR_ROLLBACK_RESTORED_FAILED}（{cause}）"),
        };
        runtime.stop();
        let back = service_install::InstallState {
            revision: restore_revision,
            active: current.active.clone(),
            previous: current.previous.clone(),
        };
        let written = service_install::write_state_for(&data, layout, next.revision, &back).is_ok();
        let revived = runtime.connect(&data).is_ok()
            && runtime.version() == Some(receipt.metadata.version.clone());
        if !was_connected {
            runtime.stop();
        }
        if written && revived {
            format!("{ERR_ROLLBACK_RESTORED}（{cause}）")
        } else {
            format!("{ERR_ROLLBACK_RESTORED_FAILED}（{cause}）")
        }
    };

    // Flip the pointer only while the runtime is down.
    runtime.stop();
    if service_install::write_state_for(&data, layout, current.revision, &next).is_err() {
        // Pointer unchanged: put the connection back the way it was found, and report
        // which of the two outcomes actually happened instead of assuming success.
        let mut revived = true;
        if was_connected {
            revived = runtime.connect(&data).is_ok()
                && runtime.version() == Some(receipt.metadata.version.clone());
        }
        return Err(if revived {
            ERR_STATE_WRITE.to_string()
        } else {
            ERR_STATE_WRITE_RECONNECT.to_string()
        });
    }

    let started = match runtime.connect(&data) {
        Ok(snapshot) => snapshot,
        // 带上真实原因：只报一句「回退失败」等于把现场销毁，查不出为什么。
        Err(error) => return Err(rollback_failed(format!("连接失败：{error}"))),
    };
    if started.version != Some(target_version.clone()) {
        return Err(rollback_failed(format!(
            "版本不一致：期望 {target_version:?}，实际 {:?}",
            started.version
        )));
    }

    // Succeeded: leave the runtime disconnected if the caller had it down, then report
    // its real final state instead of the boot-time snapshot taken before the stop.
    if !was_connected {
        runtime.stop();
    }
    Ok(runtime.snapshot())
}
