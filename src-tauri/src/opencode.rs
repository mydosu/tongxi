//! OpenCode 成员：它自带标准 ACP（`opencode acp`），直接用通用的 ACP 运行时（hermes.rs），不另写桥。
use crate::codex::RuntimeSnapshot;
use crate::store::{RunRecord, Store};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tauri::State;

pub const AGENT_ID: &str = "opencode-win";

/// 状态身份保持 `.0`：和 Hermes 共用同一个运行时类型，按这层包装区分。
pub struct Runtime(pub Arc<crate::hermes::Runtime>);
impl Runtime {
    pub fn new(app: tauri::AppHandle, store: Arc<Mutex<Store>>, directory: PathBuf) -> Self {
        Self(crate::hermes::Runtime::with_target(app, store, directory, crate::acp_transport::Target::OpenCode))
    }
}

#[tauri::command]
pub fn opencode_status(runtime: State<'_, Runtime>) -> RuntimeSnapshot {
    runtime.0.snapshot()
}
#[tauri::command]
pub async fn connect_opencode(
    app: tauri::AppHandle,
    runtime: State<'_, Runtime>,
    lock: State<'_, crate::services::MaintenanceLock>,
) -> Result<RuntimeSnapshot, String> {
    let _guard = crate::services::acquire(&app, lock.inner(), AGENT_ID, "connect")?;
    let runtime = runtime.0.clone();
    tauri::async_runtime::spawn_blocking(move || runtime.connect())
        .await
        .map_err(|_| "OpenCode 连接任务异常".to_string())?
}
#[tauri::command]
pub async fn disconnect_opencode(
    app: tauri::AppHandle,
    runtime: State<'_, Runtime>,
    lock: State<'_, crate::services::MaintenanceLock>,
) -> Result<RuntimeSnapshot, String> {
    let _guard = crate::services::acquire_shutdown(&app, lock.inner(), AGENT_ID)?;
    let runtime = runtime.0.clone();
    tauri::async_runtime::spawn_blocking(move || {
        runtime.stop();
        runtime.snapshot()
    })
    .await
    .map_err(|_| "OpenCode 停止任务异常".into())
}
#[tauri::command]
pub fn send_opencode_message(
    runtime: State<'_, Runtime>,
    conversation_id: String,
    message_id: String,
    content: String,
) -> Result<RunRecord, String> {
    runtime.0.send(&conversation_id, &message_id, &content)
}
#[tauri::command]
pub fn cancel_opencode_run(runtime: State<'_, Runtime>, conversation_id: String) -> Result<RuntimeSnapshot, String> {
    runtime.0.cancel(&conversation_id)
}
