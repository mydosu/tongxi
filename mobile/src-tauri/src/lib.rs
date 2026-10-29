// 同席手机薄壳。这里刻意不做任何事：
// 配对页只是把桌面网关那条带令牌的地址存下来，然后整页跳过去，页面里跑的是桌面那份前端。
// 所以薄壳不编译 src-tauri 的任何命令，只开扫码插件最少的权限。
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_barcode_scanner::init())
        // 配对页会整页跳到 http://100.x:47680（不在 tauri.localhost 上）。这里刻意什么都
        // 不做：wry 在安卓上把 WebViewClient.shouldOverrideUrlLoading 映射成
        // `!(navigation_handler)(url)`，而 Tauri 不给主 webview 装 navigation_handler，
        // 所以 shouldOverrideUrlLoading 返回 false —— WebView 自己在 App 里加载新地址，
        // 不会甩到系统浏览器（也不会经过 opener 插件）。想改成「外部链接交给浏览器」的话，
        // 要显式给 webview 装 on_navigation 并只对白名单返回 true。
        .run(tauri::generate_context!())
        .expect("error while running tongxi mobile shell");
}
