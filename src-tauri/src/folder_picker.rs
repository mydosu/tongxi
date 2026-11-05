//! 目录输入框旁的「选择…」：弹 Windows 原生的选文件夹对话框（和资源管理器同一套），不用手打路径。

/// 用户取消返回 None。`start` 是对话框一打开时所在的文件夹（输入框里已有的路径），不存在就忽略。
#[tauri::command]
pub async fn pick_folder(window: tauri::WebviewWindow, title: String, start: Option<String>) -> Result<Option<String>, String> {
    #[cfg(windows)]
    {
        let owner = window.hwnd().map(|hwnd| hwnd.0 as isize).unwrap_or(0);
        // 对话框要在单线程套间（STA）里跑，别占用 Tauri 的异步线程。
        tauri::async_runtime::spawn_blocking(move || show(owner, &title, start.as_deref()))
            .await
            .map_err(|error| error.to_string())?
    }
    #[cfg(not(windows))]
    {
        let _ = (window, title, start);
        Err("只有 Windows 版支持选择文件夹".into())
    }
}

#[cfg(windows)]
fn show(owner: isize, title: &str, start: Option<&str>) -> Result<Option<String>, String> {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{ERROR_CANCELLED, HWND};
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
    use windows::Win32::UI::Shell::{FileOpenDialog, IFileOpenDialog, IShellItem, SHCreateItemFromParsingName, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, SIGDN_FILESYSPATH};
    let text = |error: windows::core::Error| format!("打开选择文件夹窗口失败：{}", error.message());
    unsafe {
        let init = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        let result = (|| {
            let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).map_err(text)?;
            dialog.SetOptions(dialog.GetOptions().map_err(text)? | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).map_err(text)?;
            dialog.SetTitle(&HSTRING::from(title)).map_err(text)?;
            if let Some(start) = start.map(str::trim).filter(|path| !path.is_empty()) {
                if let Ok(folder) = SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(start.trim_start_matches(r"\\?\")), None) {
                    let _ = dialog.SetFolder(&folder);
                }
            }
            if let Err(error) = dialog.Show(Some(HWND(owner as *mut _))) {
                return if error.code() == ERROR_CANCELLED.to_hresult() { Ok(None) } else { Err(text(error)) };
            }
            let raw = dialog.GetResult().map_err(text)?.GetDisplayName(SIGDN_FILESYSPATH).map_err(text)?;
            let path = raw.to_string().map_err(|error| error.to_string());
            CoTaskMemFree(Some(raw.0 as *const _));
            path.map(Some)
        })();
        if init.is_ok() {
            CoUninitialize();
        }
        result
    }
}
