#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod acp_transport;
mod albion;
mod albion_openai;
mod chat_text;
mod clients;
mod codex;
mod discussion;
mod dsh;
mod group_store;
mod hermes;
mod models;
mod process_scope;
mod project_models;
mod project_native;
mod project_store;
#[cfg(test)]
mod project_tests;
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

struct AppState {
    store: Arc<Mutex<Store>>,
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
        "albion-wsl" => app.state::<albion::Runtime>().0.snapshot(),
        _ => app.state::<Arc<hermes::Runtime>>().snapshot(),
    };
    if matches!(
        agent_id.as_str(),
        "codex-win" | "hermes-win" | "dsh-win" | "albion-wsl"
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

#[tauri::command]
fn list_agents() -> Vec<Agent> {
    store::agents()
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
fn get_conversation(state: State<'_, AppState>, id: String) -> Result<ConversationDetail, String> {
    with_store(&state, |store| store.detail(&id))
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
    runtime: State<'_, Arc<codex::Runtime>>,
    hermes: State<'_, Arc<hermes::Runtime>>,
    id: String,
    archived: bool,
) -> Result<Conversation, String> {
    albion.0.guard(&id, || {
        dsh.guard(&id, || {
            hermes.guard(&id, || {
                runtime.mutate(&id, |store| store.archive(&id, archived))
            })
        })
    })
}
#[tauri::command]
fn delete_conversation(
    albion: State<'_, albion::Runtime>,
    dsh: State<'_, Arc<dsh::Runtime>>,
    runtime: State<'_, Arc<codex::Runtime>>,
    hermes: State<'_, Arc<hermes::Runtime>>,
    id: String,
) -> Result<(), String> {
    albion.0.guard(&id, || {
        dsh.guard(&id, || {
            hermes.guard(&id, || runtime.mutate(&id, |store| store.delete(&id)))
        })
    })
}
#[tauri::command]
fn update_group_members(
    albion: State<'_, albion::Runtime>,
    dsh: State<'_, Arc<dsh::Runtime>>,
    runtime: State<'_, Arc<codex::Runtime>>,
    hermes: State<'_, Arc<hermes::Runtime>>,
    id: String,
    members: Vec<String>,
) -> Result<Conversation, String> {
    albion.0.guard(&id, || {
        dsh.guard(&id, || {
            hermes.guard(&id, || {
                runtime.mutate(&id, |store| store.replace_members(&id, &members))
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
) -> Result<store::ConversationDetail, String> {
    with_store(&state, |store| {
        store.attach_native_session(
            &conversation_id,
            &agent_id,
            &native_session_id,
            native_cwd.as_deref(),
        )?;
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
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_some_and(|arg| arg == "--project-tools") {
        let result = if args.len() == 4 {
            project_tools::serve(std::path::Path::new(&args[2]), &args[3].to_string_lossy())
        } else {
            Err("项目工具启动参数无效".into())
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
            let store = Arc::new(Mutex::new(
                Store::open(&directory.join("hub.db")).map_err(std::io::Error::other)?,
            ));
            app.manage(codex::Runtime::new(
                app.handle().clone(),
                store.clone(),
                directory.clone(),
            ));
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
                app.state::<albion::Runtime>().0.clone(),
            ));
            app.manage(projects::Runtime::new(
                app.handle().clone(),
                store.clone(),
                directory.clone(),
                app.state::<Arc<codex::Runtime>>().inner().clone(),
                app.state::<Arc<hermes::Runtime>>().inner().clone(),
                app.state::<Arc<dsh::Runtime>>().inner().clone(),
            ));
            app.manage(services::MaintenanceLock::default());
            app.manage(AppState {
                store,
                directory: directory.clone(),
            });
            tauri::WebviewWindowBuilder::from_config(app, &app.config().app.windows[0])?
                .data_directory(directory.join("webview"))
                .build()?;
            // 关闭按钮只把主窗口收进托盘；真正退出走托盘菜单，保证 RunEvent::Exit 的清理照常执行。
            let window = app
                .get_webview_window("main")
                .ok_or_else(|| "主窗口未创建".to_string())?;
            let hidden = window.clone();
            window.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = hidden.hide();
                }
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
            get_conversation,
            create_conversation,
            rename_conversation,
            archive_conversation,
            delete_conversation,
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
            projects::project_status,
            projects::start_project,
            projects::plan_project,
            projects::confirm_project,
            projects::continue_project,
            projects::update_paused_project_roles,
            projects::cancel_project,
            projects::set_project_summary,
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
            attach_native_session
        ])
        .build(tauri::generate_context!())
        .expect("Agent Hub failed to start")
        .run(|app, event| {
            if matches!(event, tauri::RunEvent::Exit) {
                app.state::<Arc<projects::Runtime>>().stop();
                app.state::<Arc<discussion::Runtime>>().stop();
                app.state::<Arc<codex::Runtime>>().stop();
                app.state::<Arc<hermes::Runtime>>().stop();
                app.state::<Arc<dsh::Runtime>>().stop();
                app.state::<albion::Runtime>().0.stop();
            }
        });
}
