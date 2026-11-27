//! 运行状态事件只当「有变化」的通知，不带快照；前端收到后自己调 `<key>_status` 取最新快照。
//! 不把快照塞进事件：Tauri 的事件靠 WebView2 的 ExecuteScript 送进页面，每执行一次都漏一点内存、载荷越大漏得越多
//! （tauri#12724、wry#1489）。流式回复时一秒几十个、每个上 MB 的快照把 WebView 撑到过十几 GB；取快照走 invoke（fetch），不经过它。
//! 同一事件 100ms 内只发一次：通知不带数据、前端取到的总是最新的，合并掉不丢变化。
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tauri::{AppHandle, Emitter};

const EVERY: Duration = Duration::from_millis(100);
static PENDING: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn pending() -> std::sync::MutexGuard<'static, HashSet<String>> {
    PENDING.get_or_init(Default::default).lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn notify(app: &AppHandle, event: &str) {
    if !pending().insert(event.to_string()) {
        return;
    }
    let (app, event) = (app.clone(), event.to_string());
    std::thread::spawn(move || {
        std::thread::sleep(EVERY);
        // 先摘掉再发：发之后的变化会另起一次通知，前端不会停在旧快照上。
        pending().remove(&event);
        let _ = app.emit(&event, ());
    });
}
