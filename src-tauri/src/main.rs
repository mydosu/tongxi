#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod quota;
mod project_inquiries;
mod project_brain;
mod prompts;
mod quota_reset;
mod cli_open;
mod cli_transcript;
mod acp_transport;
mod agent_setup;
mod albion;
mod albion_openai;
#[cfg(test)]
mod attachment_tests;
mod attachments;
mod avatars;
mod backgrounds;
#[cfg(test)]
mod chat_question_tests;
mod chat_questions;
mod chat_text;
mod claude;
mod clients;
mod companion;
mod codex;
mod conversation_cleanup;
mod goat;
mod discussion;
mod dsh;
mod group_store;
mod hermes;
mod models;
mod process_scope;
mod project_memory;
mod project_sessions;
mod project_store;
mod project_tools;
mod project_worktree;
mod projects;
mod rpc;
mod service_apply;
mod service_install;
mod service_plans;
mod service_updates;
mod services;
mod store;

use serde::Serialize;
use std::sync::{Arc, Mutex};
use store::{Agent, Conversation, ConversationDetail, Message, Store};
use tauri::{Manager, State};

pub(crate) struct AppState {
    pub(crate) store: Arc<Mutex<Store>>,
    directory: std::path::PathBuf,
}

/// HUD 的全局快捷键：Ctrl+Shift+H（与 Hermes 的 HUD 快捷键一致）。
fn hud_shortcut() -> tauri_plugin_global_shortcut::Shortcut {
    tauri_plugin_global_shortcut::Shortcut::new(
        Some(
            tauri_plugin_global_shortcut::Modifiers::CONTROL
                | tauri_plugin_global_shortcut::Modifiers::SHIFT,
        ),
        tauri_plugin_global_shortcut::Code::KeyH,
    )
}

// ── HUD ─────────────────────────────────────────────────────────────────────
//
// 参考 Hermes：`electron/main.ts::spawnHudWindow` / `openHudWindow` / `closeHudWindow`
// 与 `hud-geometry.ts`。要点：
//  · 620×320、最小 380×160、无边框、透明、置顶、跳过任务栏、无阴影、**不可缩放**
//    （透明无边框窗口在 Windows 上留着系统边缘热区，拖动会被误判成缩放，窗口会长个几像素）
//  · 创建时先不显示，前端套好皮肤后自己 show()，避免白闪
//  · 打开时收起主窗口、关闭时把主窗口还给用户；开/关都向所有窗口广播状态，
//    免得主窗口的切换按钮说谎（Hermes 的 broadcastHudState）
//  · 位置和尺寸记忆下来，下次开在同一处；记忆坐标要跟当前显示器校验，
//    否则外接屏拔掉后 HUD 会停在看不见的地方
//  · 建窗口必须在事件循环线程上做：同步命令里直接 `build()` 会自锁（调用永不返回），
//    所以这里一律走 async，托盘/快捷键从别的线程 spawn 进来

const HUD_STATE_FILE: &str = "hud-window.json";
const HUD_WIDTH: f64 = 620.0;
const HUD_HEIGHT: f64 = 320.0;
/// HUD 最小宽高：与建窗时的 `min_inner_size` 一致（也是 Hermes 的 HUD_MIN_WIDTH/HEIGHT）。
const HUD_MIN_WIDTH: f64 = 380.0;
const HUD_MIN_HEIGHT: f64 = 160.0;
const HUD_BOTTOM_MARGIN: f64 = 72.0;

#[derive(Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
struct HudBounds {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

fn hud_bounds_path(data: &std::path::Path) -> std::path::PathBuf {
    data.join(HUD_STATE_FILE)
}

/// 读记忆坐标；文件缺失或损坏一律当作没有（不报错、不阻断开窗）。
fn read_hud_bounds(data: &std::path::Path) -> Option<HudBounds> {
    serde_json::from_str(&std::fs::read_to_string(hud_bounds_path(data)).ok()?).ok()
}

/// 记忆坐标至少要和某块显示器重叠 40px，否则视为「停在看不见的地方」。
fn hud_bounds_on_screen(app: &tauri::AppHandle, bounds: &HudBounds) -> bool {
    let (x, y) = (bounds.x, bounds.y);
    let (w, h) = (bounds.width as i32, bounds.height as i32);
    app.available_monitors()
        .unwrap_or_default()
        .iter()
        .any(|monitor| {
            let origin = monitor.position();
            let size = monitor.size();
            let (mx, my) = (origin.x, origin.y);
            let (mw, mh) = (size.width as i32, size.height as i32);
            x < mx + mw - 40 && x + w > mx + 40 && y < my + mh - 40 && y + h > my + 40
        })
}

/// 默认位置：鼠标所在显示器的下方居中，留 72px 底边距（Hermes 同款）。
fn default_hud_bounds(app: &tauri::AppHandle) -> HudBounds {
    let cursor = app.cursor_position().ok();
    let monitor = cursor
        .and_then(|point| {
            app.available_monitors().ok()?.into_iter().find(|monitor| {
                let origin = monitor.position();
                let size = monitor.size();
                let (px, py) = (point.x as i32, point.y as i32);
                px >= origin.x
                    && px < origin.x + size.width as i32
                    && py >= origin.y
                    && py < origin.y + size.height as i32
            })
        })
        .or_else(|| app.primary_monitor().ok().flatten());
    let Some(monitor) = monitor else {
        return HudBounds {
            x: 0,
            y: 0,
            width: HUD_WIDTH as u32,
            height: HUD_HEIGHT as u32,
        };
    };
    let scale = monitor.scale_factor();
    let origin = monitor.position();
    let size = monitor.size();
    let width = ((HUD_WIDTH * scale) as u32).min(size.width);
    let height = ((HUD_HEIGHT * scale) as u32).min(size.height);
    HudBounds {
        x: origin.x + (size.width as i32 - width as i32) / 2,
        y: origin.y + size.height as i32 - height as i32 - (HUD_BOTTOM_MARGIN * scale) as i32,
        width,
        height,
    }
}

/// 开 HUD：已存在就前置；否则按记忆/默认坐标开，并收起主窗口。
async fn open_hud(app: &tauri::AppHandle, directory: &std::path::Path) -> Result<bool, String> {
    use tauri::Emitter;
    if let Some(hud) = app.get_webview_window("hud") {
        let _ = hud.set_focus();
        return Ok(true);
    }
    let bounds = match read_hud_bounds(directory) {
        Some(saved) if hud_bounds_on_screen(app, &saved) => saved,
        _ => default_hud_bounds(app),
    };
    let hud =
        tauri::WebviewWindowBuilder::new(app, "hud", tauri::WebviewUrl::App("index.html".into()))
            .title("同席 HUD")
            // 交给网页处理文件拖放（输入框接收资源管理器拖入的文件）。
            .disable_drag_drop_handler()
            .inner_size(HUD_WIDTH, HUD_HEIGHT)
            .min_inner_size(380.0, 160.0)
            .decorations(false)
            .transparent(true)
            .resizable(false)
            .minimizable(false)
            .maximizable(false)
            .skip_taskbar(true)
            .shadow(false)
            .always_on_top(true)
            .visible(false)
            // 与主窗口同一个 WebView2 环境（同一数据目录）：共用 localStorage，调试端口也能看到它。
            .data_directory(directory.join("webview"))
            .build()
            .map_err(|_| "无法创建 HUD 窗口".to_string())?;
    let _ = hud.set_size(tauri::PhysicalSize::new(bounds.width, bounds.height));
    let _ = hud.set_position(tauri::PhysicalPosition::new(bounds.x, bounds.y));

    // 位置/尺寸变化就记下来（同一像素重复事件直接跳过）。事件里只用 payload，
    // 不再回调窗口方法 —— 那是在主线程事件循环里再往主线程派发，会自锁。
    let remembered = std::sync::Arc::new(std::sync::Mutex::new(bounds));
    let path = hud_bounds_path(directory);
    let state = remembered.clone();
    hud.on_window_event(move |event| {
        let mut next = *state.lock().unwrap_or_else(|error| error.into_inner());
        match event {
            tauri::WindowEvent::Moved(position) => {
                next.x = position.x;
                next.y = position.y;
            }
            tauri::WindowEvent::Resized(size) => {
                next.width = size.width;
                next.height = size.height;
            }
            _ => return,
        }
        let mut guard = state.lock().unwrap_or_else(|error| error.into_inner());
        if *guard == next {
            return;
        }
        *guard = next;
        if let Ok(text) = serde_json::to_string(&next) {
            let _ = std::fs::write(&path, text);
        }
    });

    if let Some(main) = app.get_webview_window("main") {
        let _ = main.hide();
    }
    let _ = app.emit("hud:active", true);
    Ok(true)
}

/// 关 HUD：唯一的收尾路径（托盘、快捷键、HUD 自己的退出按钮都走这里）。
async fn close_hud(app: &tauri::AppHandle) -> Result<bool, String> {
    use tauri::Emitter;
    if let Some(hud) = app.get_webview_window("hud") {
        hud.destroy().map_err(|_| "无法关闭 HUD 窗口".to_string())?;
    }
    focus_main_window(app);
    let _ = app.emit("hud:active", false);
    Ok(false)
}

/// 按逻辑像素设置 HUD 窗口的位置与尺寸（前端算好绝对 bounds 交过来）。
///
/// 尺寸确实变了才临时打开 `resizable`：透明无边框窗口不能长期留着系统边缘热区，
/// 所以设完立刻关回去（Hermes 的 `hermes:hud:set-bounds` 同款做法）；尺寸没变就只挪位置。
#[tauri::command]
async fn set_hud_bounds(
    app: tauri::AppHandle,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) -> Result<bool, String> {
    let Some(hud) = app.get_webview_window("hud") else {
        return Ok(false);
    };
    let width = width.max(HUD_MIN_WIDTH);
    let height = height.max(HUD_MIN_HEIGHT);
    // 当前尺寸是物理像素：把目标逻辑尺寸换算成物理再比，别拿逻辑和物理直接比。
    let scale = hud
        .scale_factor()
        .map_err(|_| "无法读取 HUD 缩放比例".to_string())?;
    let resizing = match hud.inner_size() {
        Ok(current) => {
            current.width != (width * scale).round() as u32
                || current.height != (height * scale).round() as u32
        }
        Err(_) => true,
    };
    if resizing {
        hud.set_resizable(true)
            .map_err(|_| "无法切换 HUD 缩放状态".to_string())?;
    }
    let applied = hud
        .set_position(tauri::LogicalPosition::new(x, y))
        .and_then(|()| {
            if resizing {
                hud.set_size(tauri::LogicalSize::new(width, height))
            } else {
                Ok(())
            }
        });
    if resizing {
        // 收尾必须关回去：失败也要关，别把热区留给用户。
        let _ = hud.set_resizable(false);
    }
    applied.map_err(|_| "无法设置 HUD 窗口位置与尺寸".to_string())?;
    Ok(true)
}

/// HUD 开关。前端按 `enabled` 传目标状态，返回实际状态。
#[tauri::command]
async fn set_hud_enabled(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    enabled: bool,
) -> Result<bool, String> {
    if enabled {
        open_hud(&app, &state.directory).await
    } else {
        close_hud(&app).await
    }
}

#[tauri::command]
fn dsh_status(runtime: State<'_, Arc<dsh::Runtime>>) -> codex::RuntimeSnapshot {
    runtime.snapshot()
}
#[tauri::command]
async fn connect_dsh(
    app: tauri::AppHandle,
    runtime: State<'_, Arc<dsh::Runtime>>,
    lock: State<'_, services::MaintenanceLock>,
) -> Result<codex::RuntimeSnapshot, String> {
    // 连接维护互斥：guard 覆盖整个 spawn_blocking 连接过程，与维护/重启串行。
    let _guard = services::acquire(&app, lock.inner(), "dsh-win", "connect")?;
    let runtime = runtime.inner().clone();
    tauri::async_runtime::spawn_blocking(move || runtime.connect())
        .await
        .map_err(|_| "DSH 连接任务异常".to_string())?
}
#[tauri::command]
async fn disconnect_dsh(
    app: tauri::AppHandle,
    runtime: State<'_, Arc<dsh::Runtime>>,
    lock: State<'_, services::MaintenanceLock>,
) -> Result<codex::RuntimeSnapshot, String> {
    // store 原子 shutdown 标记挡群聊/项目、放行 privateRun；LockGuard 到命令结束才清。
    let _guard = services::acquire_shutdown(&app, lock.inner(), "dsh-win")?;
    let runtime = runtime.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        runtime.stop();
        runtime.snapshot()
    })
    .await
    .map_err(|_| "DSH 停止任务异常".into())
}
#[tauri::command]
fn send_dsh_message(
    runtime: State<'_, Arc<dsh::Runtime>>,
    conversation_id: String,
    message_id: String,
    content: String,
) -> Result<store::RunRecord, String> {
    runtime.send(&conversation_id, &message_id, &content)
}
#[tauri::command]
fn cancel_dsh_run(
    runtime: State<'_, Arc<dsh::Runtime>>,
    conversation_id: String,
) -> Result<codex::RuntimeSnapshot, String> {
    runtime.cancel(&conversation_id)
}

#[tauri::command]
fn claude_status(runtime: State<'_, Arc<claude::Runtime>>) -> codex::RuntimeSnapshot {
    runtime.snapshot()
}
#[tauri::command]
async fn connect_claude(
    app: tauri::AppHandle,
    runtime: State<'_, Arc<claude::Runtime>>,
    lock: State<'_, services::MaintenanceLock>,
) -> Result<codex::RuntimeSnapshot, String> {
    let _guard = services::acquire(&app, lock.inner(), claude::AGENT_ID, "connect")?;
    let runtime = runtime.inner().clone();
    tauri::async_runtime::spawn_blocking(move || runtime.connect())
        .await
        .map_err(|_| "Claude 连接任务异常".to_string())?
}
#[tauri::command]
async fn disconnect_claude(
    app: tauri::AppHandle,
    runtime: State<'_, Arc<claude::Runtime>>,
    lock: State<'_, services::MaintenanceLock>,
) -> Result<codex::RuntimeSnapshot, String> {
    let _guard = services::acquire_shutdown(&app, lock.inner(), claude::AGENT_ID)?;
    let runtime = runtime.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        runtime.stop();
        runtime.snapshot()
    })
    .await
    .map_err(|_| "Claude 停止任务异常".into())
}
#[tauri::command]
fn send_claude_message(
    runtime: State<'_, Arc<claude::Runtime>>,
    conversation_id: String,
    message_id: String,
    content: String,
) -> Result<store::RunRecord, String> {
    runtime.send(&conversation_id, &message_id, &content)
}
#[tauri::command]
fn cancel_claude_run(
    runtime: State<'_, Arc<claude::Runtime>>,
    conversation_id: String,
) -> Result<codex::RuntimeSnapshot, String> {
    runtime.cancel(&conversation_id)
}

#[tauri::command]
fn codex_status(runtime: State<'_, Arc<codex::Runtime>>) -> codex::RuntimeSnapshot {
    runtime.snapshot()
}

#[tauri::command]
fn hermes_status(runtime: State<'_, Arc<hermes::Runtime>>) -> codex::RuntimeSnapshot {
    runtime.snapshot()
}
#[tauri::command]
async fn connect_hermes(
    app: tauri::AppHandle,
    runtime: State<'_, Arc<hermes::Runtime>>,
    lock: State<'_, services::MaintenanceLock>,
) -> Result<codex::RuntimeSnapshot, String> {
    // 连接维护互斥：guard 覆盖整个 spawn_blocking 连接过程，与维护/重启串行。
    let _guard = services::acquire(&app, lock.inner(), "hermes-win", "connect")?;
    let runtime = runtime.inner().clone();
    tauri::async_runtime::spawn_blocking(move || runtime.connect())
        .await
        .map_err(|_| "Hermes 连接任务异常".to_string())?
}
#[tauri::command]
async fn disconnect_hermes(
    app: tauri::AppHandle,
    runtime: State<'_, Arc<hermes::Runtime>>,
    lock: State<'_, services::MaintenanceLock>,
) -> Result<codex::RuntimeSnapshot, String> {
    // store 原子 shutdown 标记挡群聊/项目、放行 privateRun；LockGuard 到命令结束才清。
    let _guard = services::acquire_shutdown(&app, lock.inner(), "hermes-win")?;
    let runtime = runtime.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        runtime.stop();
        runtime.snapshot()
    })
    .await
    .map_err(|_| "Hermes 停止任务异常".into())
}
#[tauri::command]
fn send_hermes_message(
    runtime: State<'_, Arc<hermes::Runtime>>,
    conversation_id: String,
    message_id: String,
    content: String,
) -> Result<store::RunRecord, String> {
    runtime.send(&conversation_id, &message_id, &content)
}
#[tauri::command]
fn cancel_hermes_run(
    runtime: State<'_, Arc<hermes::Runtime>>,
    conversation_id: String,
) -> Result<codex::RuntimeSnapshot, String> {
    runtime.cancel(&conversation_id)
}
#[tauri::command]
fn set_session_settings(
    state: State<'_, AppState>,
    app: tauri::AppHandle,
    id: String,
    agent_id: String,
    model: Option<String>,
    reasoning_effort: Option<String>,
) -> Result<ConversationDetail, String> {
    let snapshot = match agent_id.as_str() {
        "codex-win" => app.state::<Arc<codex::Runtime>>().snapshot(),
        "dsh-win" => app.state::<Arc<dsh::Runtime>>().snapshot(),
        "claude-win" => app.state::<Arc<claude::Runtime>>().snapshot(),
        "albion-wsl" => app.state::<albion::Runtime>().0.snapshot(),
        _ => app.state::<Arc<hermes::Runtime>>().snapshot(),
    };
    if matches!(
        agent_id.as_str(),
        "codex-win" | "hermes-win" | "dsh-win" | "claude-win" | "albion-wsl"
    ) && snapshot.connection == "connected"
    {
        models::validate_selection(
            &snapshot.models,
            model.as_deref().filter(|value| !value.is_empty()),
            reasoning_effort
                .as_deref()
                .filter(|value| !value.is_empty()),
            snapshot.default_model.as_deref(),
        )?;
    }
    // A running turn owns its admission snapshot; edits target the next turn.
    with_store(&state, |store| {
        store.set_session_settings(&id, &agent_id, model, reasoning_effort)
    })
}

#[tauri::command]
async fn connect_codex(
    app: tauri::AppHandle,
    runtime: State<'_, Arc<codex::Runtime>>,
    lock: State<'_, services::MaintenanceLock>,
) -> Result<codex::RuntimeSnapshot, String> {
    // 连接维护互斥：guard 覆盖整个 spawn_blocking 连接过程，与维护/重启串行。
    let _guard = services::acquire(&app, lock.inner(), "codex-win", "connect")?;
    let runtime = runtime.inner().clone();
    tauri::async_runtime::spawn_blocking(move || runtime.connect())
        .await
        .map_err(|_| "连接任务异常".to_string())?
}

#[tauri::command]
async fn disconnect_codex(
    app: tauri::AppHandle,
    runtime: State<'_, Arc<codex::Runtime>>,
    lock: State<'_, services::MaintenanceLock>,
) -> Result<codex::RuntimeSnapshot, String> {
    // store 原子 shutdown 标记挡群聊/项目、放行 privateRun；LockGuard 到命令结束才清。
    let _guard = services::acquire_shutdown(&app, lock.inner(), "codex-win")?;
    let runtime = runtime.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        runtime.stop();
        runtime.snapshot()
    })
    .await
    .map_err(|_| "停止任务异常".into())
}

#[tauri::command]
fn send_codex_message(
    runtime: State<'_, Arc<codex::Runtime>>,
    conversation_id: String,
    message_id: String,
    content: String,
) -> Result<store::RunRecord, String> {
    runtime
        .inner()
        .send(&conversation_id, &message_id, &content)
}

#[tauri::command]
async fn cancel_codex_run(
    runtime: State<'_, Arc<codex::Runtime>>,
    conversation_id: String,
) -> Result<codex::RuntimeSnapshot, String> {
    let runtime = runtime.inner().clone();
    tauri::async_runtime::spawn_blocking(move || runtime.cancel(&conversation_id))
        .await
        .map_err(|_| "取消任务异常".to_string())?
}

/// 回复进行中插话。Codex（turn/steer）与 Claude（写入运行中会话）原生支持，不打断当前回复；
/// 其他成员返回 NO_NATIVE_STEER，由界面改为“先打断再发送”。插话成功后作为用户消息存入会话。
#[tauri::command]
async fn steer_agent_message(
    state: State<'_, AppState>,
    codex: State<'_, Arc<codex::Runtime>>,
    claude: State<'_, Arc<claude::Runtime>>,
    agent_id: String,
    conversation_id: String,
    message_id: String,
    content: String,
) -> Result<store::Message, String> {
    let (codex, claude) = (codex.inner().clone(), claude.inner().clone());
    let (room, text, steer_id) = (conversation_id.clone(), content.clone(), message_id.clone());
    tauri::async_runtime::spawn_blocking(move || match agent_id.as_str() {
        "codex-win" => codex.steer(&room, &text, &steer_id),
        "claude-win" => claude.steer(&room, &text, &steer_id),
        _ => Err("NO_NATIVE_STEER".into()),
    })
    .await
    .map_err(|_| "插话异常".to_string())??;
    with_store(&state, |store| {
        store.save_delivered_message(&conversation_id, &message_id, &content)
    })
}

fn native_snapshot(app: &tauri::AppHandle, agent: &str) -> Result<codex::RuntimeSnapshot, String> {
    Ok(match agent {
        "codex-win" => app.state::<Arc<codex::Runtime>>().snapshot(),
        "dsh-win" => app.state::<Arc<dsh::Runtime>>().snapshot(),
        "claude-win" => app.state::<Arc<claude::Runtime>>().snapshot(),
        "hermes-win" => app.state::<Arc<hermes::Runtime>>().snapshot(),
        _ => return Err("陪伴通过它自己的服务对话，没有 CLI 可以打开".into()),
    })
}

fn emit_cli_handoffs(app: &tauri::AppHandle) {
    let _ = tauri::Emitter::emit(app, "cli-handoffs", list_cli_handoffs(app));
}

fn list_cli_handoffs(app: &tauri::AppHandle) -> Vec<serde_json::Value> {
    let state = app.state::<AppState>();
    let store = state.store.lock().unwrap();
    store
        .cli_handoffs
        .iter()
        .map(|(conversation, agent)| serde_json::json!({"conversation_id":conversation,"agent_id":agent}))
        .collect()
}

/// 把这条原生会话移交给该成员自己的 CLI：同席暂停使用、卸下已载入的会话，新终端里的 CLI 续上同一条会话。
/// CLI 窗口关闭后自动收回。回复进行中不移交，免得两边同时写同一条会话。
#[tauri::command]
fn open_in_cli(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    conversation_id: String,
    agent_id: String,
) -> Result<(), String> {
    native_snapshot(&app, &agent_id)?;
    // 先登记移交（之后不会再开始新回复），再确认当前没有进行中的回复。
    let target = with_store(&state, |store| {
        cli_open::begin(store, &state.directory, &conversation_id, &agent_id)
    })?;
    let undo = |error: String| {
        if let Ok(mut store) = state.store.lock() {
            let _ = store.clear_cli_handoff(&conversation_id, &agent_id);
        }
        Err(error)
    };
    if native_snapshot(&app, &agent_id)?.runs.iter().any(|run| {
        run.record.conversation_id == conversation_id
            && matches!(run.record.status.as_str(), "starting" | "running" | "cancelling")
    }) {
        return undo("这条会话正在回复，请等回复结束或停止后再在 CLI 中打开".into());
    }
    match agent_id.as_str() {
        "codex-win" => app.state::<Arc<codex::Runtime>>().release_native(&target.native),
        "dsh-win" => app.state::<Arc<dsh::Runtime>>().release_native(&target.native),
        "claude-win" => app.state::<Arc<claude::Runtime>>().release_native(&target.native),
        _ => app.state::<Arc<hermes::Runtime>>().release_native(&target.native),
    }
    let executable = match agent_id.as_str() {
        "codex-win" => codex::discover_executable(),
        "claude-win" => claude::discover_executable(),
        _ => None,
    };
    let (window, pid) =
        match cli_open::launch(&target, &agent_id, executable.as_deref(), &conversation_id) {
            Ok(launched) => launched,
            Err(error) => return undo(error),
        };
    with_store(&state, |store| {
        store.set_cli_handoff_pid(&conversation_id, &agent_id, pid)
    })?;
    emit_cli_handoffs(&app);
    watch_cli(app, window, conversation_id, agent_id);
    Ok(())
}

/// 等 CLI 窗口关闭，然后收回会话并通知界面。
fn watch_cli(app: tauri::AppHandle, window: cli_open::Window, conversation_id: String, agent_id: String) {
    std::thread::spawn(move || {
        window.wait();
        let imported = cli_open::finish(&app.state::<AppState>().store, &conversation_id, &agent_id);
        let _ = tauri::Emitter::emit(
            &app,
            "cli-returned",
            serde_json::json!({"conversation_id":conversation_id,"agent_id":agent_id,"imported":imported}),
        );
        emit_cli_handoffs(&app);
    });
}

/// 同席启动时：还开着的 CLI 窗口继续盯着；已经关掉的立刻收回（补上 CLI 里的对话）。
fn restore_cli_handoffs(app: &tauri::AppHandle) {
    let saved = app
        .state::<AppState>()
        .store
        .lock()
        .ok()
        .and_then(|store| store.saved_cli_handoffs().ok())
        .unwrap_or_default();
    for (conversation_id, agent_id, _, pid, _) in saved {
        match cli_open::Window::attach(pid) {
            Some(window) => watch_cli(app.clone(), window, conversation_id, agent_id),
            None => {
                let app = app.clone();
                std::thread::spawn(move || {
                    cli_open::finish(&app.state::<AppState>().store, &conversation_id, &agent_id);
                    emit_cli_handoffs(&app);
                });
            }
        }
    }
}

/// 同席给成员定的默认模型：会话没单独选模型时使用（目前用于 Hermes）。传 null 清除，回到 harness 默认。
#[tauri::command]
fn set_agent_default_model(
    hermes: State<'_, Arc<hermes::Runtime>>,
    agent_id: String,
    model: Option<String>,
) -> Result<(), String> {
    match agent_id.as_str() {
        "hermes-win" => hermes.set_default_model(model.as_deref()),
        _ => Err("暂只支持为 Hermes 设置同席默认模型".into()),
    }
}

/// 主动收回：结束 CLI 窗口，会话回到同席。
#[tauri::command]
fn reclaim_cli_session(
    state: State<'_, AppState>,
    conversation_id: String,
    agent_id: String,
) -> Result<(), String> {
    with_store(&state, |store| cli_open::reclaim(store, &conversation_id, &agent_id))
}

#[tauri::command]
fn cli_handoffs(app: tauri::AppHandle) -> Vec<serde_json::Value> {
    list_cli_handoffs(&app)
}

fn with_store<T>(
    state: &State<'_, AppState>,
    action: impl FnOnce(&mut Store) -> Result<T, String>,
) -> Result<T, String> {
    let mut store = state
        .store
        .lock()
        .map_err(|_| "本地数据暂不可用，请重启软件".to_string())?;
    action(&mut store)
}

/// 成员名单；陪伴的名字、介绍和头像按用户自己的配置来。
#[tauri::command]
fn list_agents(state: State<'_, AppState>) -> Vec<Agent> {
    let (companion, names, faces) = match state.store.lock() {
        Ok(store) => (
            Some(store.companion()),
            store::agents().into_iter().map(|agent| (agent.id.clone(), store.member_name(&agent.id))).collect::<Vec<_>>(),
            store::agents().into_iter().map(|agent| (agent.id.clone(), store.custom_avatar(&agent.id))).collect::<Vec<_>>(),
        ),
        Err(_) => (None, Vec::new(), Vec::new()),
    };
    store::agents()
        .into_iter()
        .map(|mut agent| {
            if let Some(name) = names.iter().find(|(id, _)| *id == agent.id).map(|(_, name)| name.clone()) {
                agent.name = name;
            }
            // 头像：用户换过的 > 成员的原生图标 > 无（界面画字母头像）。
            agent.avatar = faces
                .iter()
                .find(|(id, _)| *id == agent.id)
                .and_then(|(_, face)| face.clone())
                .or_else(|| avatars::native(&agent.id, Some(&state.directory)));
            if let (true, Some(config)) = (agent.id == "albion-wsl", &companion) {
                agent.name = config.name.clone();
                agent.subtitle = config.subtitle.clone();
                agent.avatar = config.avatar.clone();
                agent.location = "陪伴".into();
                agent.role = if config.configured {
                    "通过它自己的服务对话，人格、记忆与工具都在那边".into()
                } else {
                    "还没有接入：在「成员与服务」里配置你的陪伴".into()
                };
            }
            agent
        })
        .collect()
}

#[tauri::command]
fn list_conversations(
    state: State<'_, AppState>,
    search: String,
    archived: bool,
) -> Result<Vec<Conversation>, String> {
    with_store(&state, |store| store.list(&search, archived))
}
#[tauri::command]
fn set_conversation_order(state: State<'_, AppState>, ids: Vec<String>) -> Result<(), String> {
    with_store(&state, |store| store.set_conversation_order(&ids))
}
#[tauri::command]
fn get_conversation(state: State<'_, AppState>, id: String) -> Result<ConversationDetail, String> {
    with_store(&state, |store| {
        store.ensure_project_agent_chats(&id)?;
        store.detail(&id)
    })
}

#[tauri::command]
fn share_project_agent_message(
    state: State<'_, AppState>,
    group_id: String,
    agent_chat_id: String,
    message_id: String,
) -> Result<project_store::TeamMessage, String> {
    with_store(&state, |store| {
        store.share_project_agent_message(&group_id, &agent_chat_id, &message_id)
    })
}
#[tauri::command]
fn remove_conversation_item(
    state: State<'_, AppState>,
    conversation_id: String,
    kind: String,
    id: String,
) -> Result<conversation_cleanup::RemovedItem, String> {
    with_store(&state, |store| {
        store.remove_conversation_item(&conversation_id, &kind, &id)
    })
}

#[tauri::command]
fn pending_chat_questions(
    state: State<'_, AppState>,
) -> Result<Vec<chat_questions::Pending>, String> {
    with_store(&state, |store| store.pending_chat_questions())
}

#[tauri::command]
fn cancel_chat_question(
    state: State<'_, AppState>,
    conversation_id: String,
    question_id: String,
) -> Result<(), String> {
    with_store(&state, |store| {
        store.cancel_chat_question(&conversation_id, &question_id)
    })
}

#[tauri::command]
fn stage_attachment(
    state: State<'_, AppState>,
    conversation_id: String,
    name: String,
    mime: String,
    data: String,
) -> Result<attachments::Attachment, String> {
    with_store(&state, |store| {
        store.stage_attachment(&conversation_id, &name, &mime, &data)
    })
}
#[tauri::command]
fn attachment_data(
    state: State<'_, AppState>,
    conversation_id: String,
    attachment_id: String,
) -> Result<String, String> {
    with_store(&state, |store| {
        store.attachment_data(&conversation_id, &attachment_id)
    })
}
#[tauri::command]
fn paste_clipboard_files(
    state: State<'_, AppState>,
    conversation_id: String,
) -> Result<Vec<attachments::Attachment>, String> {
    with_store(&state, |store| {
        store.paste_clipboard_files(&conversation_id)
    })
}

#[tauri::command]
fn answer_chat_question(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    conversation_id: String,
    question_id: String,
    content: String,
) -> Result<chat_questions::Question, String> {
    let question = with_store(&state, |store| {
        store.answer_chat_question(&conversation_id, &question_id, &content)
    })?;
    if question.mode == "deferred" {
        let store = state.store.clone();
        let codex = app.state::<Arc<codex::Runtime>>().inner().clone();
        let hermes = app.state::<Arc<hermes::Runtime>>().inner().clone();
        let dsh = app.state::<Arc<dsh::Runtime>>().inner().clone();
        let claude = app.state::<Arc<claude::Runtime>>().inner().clone();
        let queued = question.clone();
        std::thread::spawn(move || {
            loop {
                let ready = {
                    let store = store.lock().unwrap();
                    let Ok(rows) = store.chat_questions(&queued.conversation_id) else {
                        return;
                    };
                    if !rows.iter().any(|row| {
                        row.id == queued.id && row.status == "answered" && row.delivery == "queued"
                    }) {
                        return;
                    }
                    store.connection.query_row("SELECT NOT EXISTS(SELECT 1 FROM runs WHERE agent_id=?1 AND status IN ('starting','running','cancelling')) AND NOT EXISTS(SELECT 1 FROM project_attempts WHERE agent_id=?1 AND status IN ('starting','running','cancelling'))",[&queued.agent_id],|row|row.get::<_,bool>(0)).unwrap_or(false)
                };
                if ready {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            let message = queued.answer_message_id.as_deref().unwrap_or_default();
            let reply = format!(
                "对你刚才的问题“{}”，我的回答是：{}。请按这个实际回答继续，保留已完成工作。",
                queued.prompt,
                queued.answer.as_deref().unwrap_or_default()
            );
            let result = match queued.agent_id.as_str() {
                "codex-win" => codex
                    .send(&queued.conversation_id, message, &reply)
                    .map(|_| ()),
                "hermes-win" => hermes
                    .send(&queued.conversation_id, message, &reply)
                    .map(|_| ()),
                "dsh-win" => dsh
                    .send(&queued.conversation_id, message, &reply)
                    .map(|_| ()),
                "claude-win" => claude
                    .send(&queued.conversation_id, message, &reply)
                    .map(|_| ()),
                _ => Err("提问成员不可用".into()),
            };
            let store = store.lock().unwrap();
            match result {
                Ok(()) => {
                    let _ = store.connection.execute(
                        "UPDATE chat_questions SET delivery='sent' WHERE id=?1",
                        [&queued.id],
                    );
                }
                Err(_) => {
                    let _=store.connection.execute("UPDATE chat_questions SET status='pending',delivery='awaiting',error='回答未能送达，请连接成员或等待当前任务完成后重试' WHERE id=?1 AND delivery='queued'",[&queued.id]);
                }
            }
        });
    }
    Ok(question)
}
/// 问答面板：项目群里主脑挂的问题（没答的全部，加最近答过的）。
#[tauri::command]
fn project_inquiries(state: State<'_, AppState>, conversation_id: String) -> Result<Vec<project_inquiries::Inquiry>, String> {
    with_store(&state, |store| store.project_inquiries(&conversation_id))
}
#[tauri::command]
fn answer_project_inquiry(
    state: State<'_, AppState>,
    conversation_id: String,
    inquiry_id: String,
    answer: String,
) -> Result<project_inquiries::Inquiry, String> {
    with_store(&state, |store| store.answer_inquiry(&conversation_id, &inquiry_id, &answer))
}
#[tauri::command]
fn brain_overview(state: State<'_, AppState>, conversation_id: String) -> Result<project_brain::BrainOverview, String> {
    with_store(&state, |store| store.brain_overview(&conversation_id))
}
#[tauri::command]
fn stop_brain_job(app: tauri::AppHandle, state: State<'_, AppState>, conversation_id: String, job_id: String) -> Result<(), String> {
    with_store(&state, |store| store.stop_brain_job(&conversation_id, &job_id))?;
    let _ = tauri::Emitter::emit(&app, "brain-state", conversation_id);
    Ok(())
}
#[tauri::command]
fn create_conversation(
    state: State<'_, AppState>,
    title: String,
    kind: String,
    members: Vec<String>,
) -> Result<Conversation, String> {
    with_store(&state, |store| store.create(&title, &kind, &members))
}
#[tauri::command]
fn rename_conversation(
    state: State<'_, AppState>,
    id: String,
    title: String,
) -> Result<Conversation, String> {
    with_store(&state, |store| store.rename(&id, &title))
}
#[tauri::command]
fn archive_conversation(
    albion: State<'_, albion::Runtime>,
    dsh: State<'_, Arc<dsh::Runtime>>,
    claude: State<'_, Arc<claude::Runtime>>,
    runtime: State<'_, Arc<codex::Runtime>>,
    hermes: State<'_, Arc<hermes::Runtime>>,
    id: String,
    archived: bool,
) -> Result<Conversation, String> {
    albion.0.guard(&id, || {
        dsh.guard(&id, || {
            claude.guard(&id, || {
                hermes.guard(&id, || {
                    runtime.mutate(&id, |store| store.archive(&id, archived))
                })
            })
        })
    })
}
#[tauri::command]
fn delete_conversation(
    albion: State<'_, albion::Runtime>,
    dsh: State<'_, Arc<dsh::Runtime>>,
    claude: State<'_, Arc<claude::Runtime>>,
    runtime: State<'_, Arc<codex::Runtime>>,
    hermes: State<'_, Arc<hermes::Runtime>>,
    id: String,
) -> Result<(), String> {
    albion.0.guard(&id, || {
        dsh.guard(&id, || {
            claude.guard(&id, || {
                hermes.guard(&id, || runtime.mutate(&id, |store| store.delete(&id)))
            })
        })
    })
}
#[tauri::command]
fn update_group_members(
    albion: State<'_, albion::Runtime>,
    dsh: State<'_, Arc<dsh::Runtime>>,
    claude: State<'_, Arc<claude::Runtime>>,
    runtime: State<'_, Arc<codex::Runtime>>,
    hermes: State<'_, Arc<hermes::Runtime>>,
    id: String,
    members: Vec<String>,
) -> Result<Conversation, String> {
    albion.0.guard(&id, || {
        dsh.guard(&id, || {
            claude.guard(&id, || {
                hermes.guard(&id, || {
                    runtime.mutate(&id, |store| store.replace_members(&id, &members))
                })
            })
        })
    })
}
#[tauri::command]
fn save_local_message(
    state: State<'_, AppState>,
    conversation_id: String,
    message_id: String,
    content: String,
) -> Result<Message, String> {
    with_store(&state, |store| {
        store.save_message(&conversation_id, &message_id, &content)
    })
}

#[derive(Serialize)]
struct AppInfo {
    version: String,
    milestone: u32,
    database_path: String,
    /// Ctrl+Shift+H 是否真的在系统里注册成功（问插件，不问自己）。
    hud_shortcut: bool,
    /// 启动时是否自动扫描并接入本机成员；`AGENT_HUB_AUTOCONNECT=0` 关掉（自动化测试用）。
    autoconnect: bool,
}

#[tauri::command]
fn app_info(app: tauri::AppHandle, state: State<'_, AppState>) -> Result<AppInfo, String> {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;
    with_store(&state, |store| {
        Ok(AppInfo {
            version: env!("CARGO_PKG_VERSION").into(),
            milestone: 7,
            database_path: store.path.to_string_lossy().into(),
            hud_shortcut: app.global_shortcut().is_registered(hud_shortcut()),
            autoconnect: std::env::var("AGENT_HUB_AUTOCONNECT").map_or(true, |value| value != "0"),
        })
    })
}

/// 只读列出某个成员自己的历史会话（各家原生实现）。
///
/// async：底层是阻塞式 RPC（最长 20s），放在 async 命令里跑在工作线程上，
/// 不会卡住界面；一次点击换一个工作线程，代价可接受。
#[tauri::command]
async fn open_agent_client(agent_id: String) -> Result<String, String> {
    // 起客户端可能要等（DSH 网页端要等服务打印地址），放阻塞线程里别卡住界面。
    tauri::async_runtime::spawn_blocking(move || clients::open(&agent_id))
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn native_sessions(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    agent_id: String,
    conversation_id: Option<String>,
) -> Result<Vec<codex::NativeSession>, String> {
    let mut sessions = match agent_id.as_str() {
        "codex-win" => app.state::<Arc<codex::Runtime>>().list_sessions()?,
        "dsh-win" => app.state::<Arc<dsh::Runtime>>().list_sessions()?,
        "claude-win" => app.state::<Arc<claude::Runtime>>().list_sessions()?,
        "hermes-win" => app.state::<Arc<hermes::Runtime>>().list_sessions()?,
        "albion-wsl" => app.state::<albion::Runtime>().0.list_sessions()?,
        _ => return Err("成员不存在".into()),
    };
    // 标出占用情况与当前绑定，界面据此决定能不能点「接入」。
    with_store(&state, |store| {
        let owners = store.native_session_owners(&agent_id)?;
        let bound = conversation_id
            .as_deref()
            .and_then(|id| store.detail(id).ok())
            .and_then(|detail| {
                detail
                    .sessions
                    .into_iter()
                    .find(|session| session.agent_id == agent_id)
            })
            .and_then(|session| session.native_session_id);
        for session in &mut sessions {
            session.occupied_by = owners.get(&session.id).cloned();
            session.current = bound.as_deref() == Some(session.id.as_str());
        }
        Ok(sessions)
    })
}

/// 把选中的原生会话接进这个同席会话：之后这个成员的消息就续在那个原生会话里。
#[tauri::command]
fn attach_native_session(
    state: State<'_, AppState>,
    conversation_id: String,
    agent_id: String,
    native_session_id: String,
    native_cwd: Option<String>,
    title: Option<String>,
) -> Result<store::ConversationDetail, String> {
    with_store(&state, |store| {
        store.attach_native_session(
            &conversation_id,
            &agent_id,
            &native_session_id,
            native_cwd.as_deref(),
        )?;
        // 接入后沿用原生会话原本的名称（去掉换行、截到 80 字）；名称不可用时保留原名。
        if let Some(title) = title {
            let title: String = title
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .filter(|c| !c.is_control())
                .take(80)
                .collect();
            if !title.is_empty() {
                store.rename(&conversation_id, &title)?;
            }
        }
        store.detail(&conversation_id)
    })
}

/// 把主窗口从托盘/最小化状态唤回前台；托盘左键、托盘菜单与单实例重复启动共用这一条路径。
fn focus_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn main() {
    // 不把启动同席那个环境里的 OpenViking 身份（例如从 Claude Code 里重启同席时继承的
    // OPENVIKING_API_KEY）传给成员：Codex 等插件以环境变量为先，会把记忆写成那个 agent。
    // 各成员只读自己的 ovcli.conf 配置。
    for (name, _) in std::env::vars_os() {
        if name
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("OPENVIKING_")
        {
            std::env::remove_var(name);
        }
    }
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_some_and(|arg| arg == "--project-chat-tools") {
        let result = if args.len() == 4 {
            project_tools::serve_project_chat(
                std::path::Path::new(&args[2]),
                &args[3].to_string_lossy(),
            )
        } else {
            Err("项目 Agent 私聊工具启动参数无效".into())
        };
        if result.is_err() {
            std::process::exit(1);
        }
        return;
    }
    // 全局快捷键 Ctrl+Shift+H：和托盘、HUD 自己的退出按钮共用 open_hud/close_hud 这一对路径。
    let hud_shortcut = hud_shortcut();
    let shortcut_plugin = tauri_plugin_global_shortcut::Builder::new()
        .with_shortcuts([hud_shortcut])
        .expect("注册 HUD 快捷键失败")
        .with_handler(move |app, shortcut, event| {
            if shortcut != &hud_shortcut
                || event.state() != tauri_plugin_global_shortcut::ShortcutState::Pressed
            {
                return;
            }
            let app = app.clone();
            // 处理器跑在事件循环线程上：建/收窗口必须派发到别处做，别在这里同步调用。
            tauri::async_runtime::spawn(async move {
                let Some(state) = app.try_state::<AppState>() else {
                    return;
                };
                let directory = state.directory.clone();
                if app.get_webview_window("hud").is_some() {
                    let _ = close_hud(&app).await;
                } else {
                    let _ = open_hud(&app, &directory).await;
                }
            });
        })
        .build();
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            focus_main_window(app);
        }))
        .plugin(shortcut_plugin)
        .setup(|app| {
            let directory = match std::env::var_os("AGENT_HUB_DATA_DIR") {
                Some(value) => {
                    let path = std::path::PathBuf::from(value);
                    if !path.is_absolute() {
                        return Err("AGENT_HUB_DATA_DIR must be absolute".into());
                    }
                    path
                }
                None => app.path().app_local_data_dir()?,
            };
            prompts::set_prompt_dir(directory.join("prompts"));
            let store = Arc::new(Mutex::new(
                Store::open(&directory.join("hub.db")).map_err(std::io::Error::other)?,
            ));
            let codex_runtime =
                codex::Runtime::new(app.handle().clone(), store.clone(), directory.clone());
            codex_runtime.watch_quota_resume();
            app.manage(codex_runtime);
            app.manage(hermes::Runtime::new(
                app.handle().clone(),
                store.clone(),
                directory.clone(),
            ));
            app.manage(dsh::Runtime::new(
                app.handle().clone(),
                store.clone(),
                directory.clone(),
            ));
            app.manage(claude::Runtime::new(
                app.handle().clone(),
                store.clone(),
                directory.clone(),
            ));
            app.manage(albion::Runtime::new(
                app.handle().clone(),
                store.clone(),
                directory.clone(),
            ));
            app.manage(discussion::Runtime::new(
                app.handle().clone(),
                store.clone(),
                app.state::<Arc<codex::Runtime>>().inner().clone(),
                app.state::<Arc<hermes::Runtime>>().inner().clone(),
                app.state::<Arc<dsh::Runtime>>().inner().clone(),
                app.state::<Arc<claude::Runtime>>().inner().clone(),
                app.state::<albion::Runtime>().0.clone(),
            ));
            project_brain::Courier::start(
                app.handle().clone(),
                store.clone(),
                app.state::<Arc<codex::Runtime>>().inner().clone(),
                app.state::<Arc<hermes::Runtime>>().inner().clone(),
                app.state::<Arc<dsh::Runtime>>().inner().clone(),
                app.state::<Arc<claude::Runtime>>().inner().clone(),
            );
            app.manage(services::MaintenanceLock::default());
            app.manage(AppState {
                store,
                directory: directory.clone(),
            });
            restore_cli_handoffs(app.handle());
            tauri::WebviewWindowBuilder::from_config(app, &app.config().app.windows[0])?
                .data_directory(directory.join("webview"))
                .build()?;
            // 最小化收进托盘；关闭按钮直接退出程序（走 app.exit，RunEvent::Exit 的清理照常执行）。
            // 是否已最小化要问窗口本身；事件回调跑在主线程事件循环里，直接查询会自锁，挪到后台线程去问。
            let window = app
                .get_webview_window("main")
                .ok_or_else(|| "主窗口未创建".to_string())?;
            let hidden = window.clone();
            let handle = app.handle().clone();
            window.on_window_event(move |event| match event {
                tauri::WindowEvent::CloseRequested { api, .. } => {
                    api.prevent_close();
                    handle.exit(0);
                }
                tauri::WindowEvent::Resized(_) => {
                    let window = hidden.clone();
                    std::thread::spawn(move || {
                        if window.is_minimized().unwrap_or(false) {
                            let _ = window.hide();
                        }
                    });
                }
                _ => {}
            });
            let menu = tauri::menu::MenuBuilder::new(app)
                .item(&tauri::menu::MenuItem::with_id(
                    app,
                    "show",
                    "显示主窗口",
                    true,
                    None::<&str>,
                )?)
                .item(&tauri::menu::MenuItem::with_id(
                    app,
                    "quit",
                    "退出",
                    true,
                    None::<&str>,
                )?)
                .build()?;
            let mut tray = tauri::tray::TrayIconBuilder::new()
                .menu(&menu)
                .tooltip("同席 · Agent Hub")
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    // 托盘唤回主窗口时，如果 HUD 还开着就一并收掉（保持「只有一个面」）。
                    "show" => {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let _ = close_hud(&app).await;
                        });
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let tauri::tray::TrayIconEvent::Click {
                        button: tauri::tray::MouseButton::Left,
                        button_state: tauri::tray::MouseButtonState::Up,
                        ..
                    } = event
                    {
                        focus_main_window(tray.app_handle());
                    }
                });
            if let Some(icon) = app.default_window_icon().cloned() {
                tray = tray.icon(icon);
            }
            tray.build(app)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_agents,
            list_conversations,
            set_conversation_order,
            get_conversation,
            create_conversation,
            rename_conversation,
            archive_conversation,
            delete_conversation,
            remove_conversation_item,
            pending_chat_questions,
            answer_chat_question,
            cancel_chat_question,
            stage_attachment,
            attachment_data,
            paste_clipboard_files,
            project_inquiries,
            answer_project_inquiry,
            brain_overview,
            stop_brain_job,
            update_group_members,
            save_local_message,
            app_info,
            codex_status,
            connect_codex,
            disconnect_codex,
            send_codex_message,
            cancel_codex_run,
            hermes_status,
            connect_hermes,
            disconnect_hermes,
            send_hermes_message,
            cancel_hermes_run,
            set_session_settings,
            dsh_status,
            connect_dsh,
            disconnect_dsh,
            send_dsh_message,
            cancel_dsh_run,
            claude_status,
            connect_claude,
            disconnect_claude,
            send_claude_message,
            steer_agent_message,
            open_in_cli,
            set_agent_default_model,
            reclaim_cli_session,
            cli_handoffs,
            cancel_claude_run,
            albion::albion_status,
            albion::connect_albion,
            albion::disconnect_albion,
            albion::send_albion_message,
            albion::cancel_albion_run,
            discussion::discussion_status,
            discussion::start_discussion,
            discussion::cancel_discussion,
            projects::list_projects,
            projects::register_project,
            projects::bind_project,
            projects::set_project_coordinator,
            projects::set_project_grants,
            project_sessions::project_session_bindings,
            project_sessions::bind_project_sessions,
            project_sessions::add_project_seat,
            agent_setup::scan_agents,
            companion::companion_config,
            companion::save_companion,
            companion::test_companion,
            backgrounds::list_backgrounds,
            backgrounds::background_image,
            backgrounds::add_background,
            backgrounds::set_background_mode,
            backgrounds::remove_background,
            agent_setup::install_agent,
            agent_setup::setup_agent,
            project_sessions::remove_project_seat,
            project_sessions::rename_project_seat,
            project_sessions::rename_agent,
            avatars::set_agent_avatar,
            avatars::own_avatars,
            avatars::set_own_avatar,
            projects::set_project_summary,
            projects::release_project,
            services::service_inventory,
            services::check_service_update,
            services::prepare_service_update,
            services::restart_service,
            services::service_update_status,
            services::apply_service_update,
            services::rollback_service_update,
            set_hud_enabled,
            set_hud_bounds,
            native_sessions,
            open_agent_client,
            attach_native_session,
            share_project_agent_message
        ])
        .build(tauri::generate_context!())
        .expect("Agent Hub failed to start")
        .run(|app, event| {
            if matches!(event, tauri::RunEvent::Exit) {
                app.state::<Arc<discussion::Runtime>>().stop();
                app.state::<Arc<codex::Runtime>>().stop();
                app.state::<Arc<hermes::Runtime>>().stop();
                app.state::<Arc<dsh::Runtime>>().stop();
                app.state::<Arc<claude::Runtime>>().stop();
                app.state::<albion::Runtime>().0.stop();
            }
        });
}
