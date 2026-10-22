//! Local attachment staging. No upload service, and no file bytes in conversation JSON.
use crate::store::Store;
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const MAX_BYTES: usize = 32 * 1024 * 1024;
pub const OPEN: &str = "```tongxi-attachments\n";
type Result<T> = std::result::Result<T, String>;
#[derive(Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub bytes: usize,
    pub path: String,
}
fn directory(db: &Path, room: &str) -> Result<PathBuf> {
    Uuid::parse_str(room).map_err(|_| "附件会话标识无效")?;
    Ok(db
        .parent()
        .ok_or("附件目录不可用")?
        .join("attachments")
        .join(room))
}
fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else {
        None
    }
}
impl Store {
    pub fn stage_attachment(
        &self,
        room: &str,
        name: &str,
        mime: &str,
        encoded: &str,
    ) -> Result<Attachment> {
        if self.conversation(room)?.archived {
            return Err("请先恢复归档会话".into());
        }
        if encoded.len() > MAX_BYTES.div_ceil(3) * 4 {
            return Err("单个附件最大32 MiB".into());
        }
        let bytes = STANDARD.decode(encoded).map_err(|_| "附件内容无效")?;
        self.stage_attachment_bytes(room, name, mime, &bytes)
    }
    fn stage_attachment_bytes(
        &self,
        room: &str,
        name: &str,
        mime: &str,
        bytes: &[u8],
    ) -> Result<Attachment> {
        if bytes.len() > MAX_BYTES {
            return Err("单个附件最大32 MiB".into());
        }
        let name: String = name
            .chars()
            .take(160)
            .map(|c| {
                if c.is_control() || "<>:\"/\\|?*".contains(c) {
                    '_'
                } else {
                    c
                }
            })
            .collect();
        let name = name.trim().trim_end_matches(['.', ' ']);
        if name.is_empty() {
            return Err("附件名称为空".into());
        }
        let root = directory(&self.path, room)?;
        let id = Uuid::new_v4().to_string();
        let folder = root.join(&id);
        std::fs::create_dir_all(&folder).map_err(|_| "无法创建附件目录")?;
        let payload = folder.join("data");
        std::fs::create_dir_all(&payload).map_err(|_| "无法创建附件目录")?;
        let path = payload.join(name);
        std::fs::write(&path, bytes).map_err(|_| "无法保存附件")?;
        let attachment = Attachment {
            id: id.clone(),
            name: name.into(),
            mime: image_mime(bytes).map(str::to_owned).unwrap_or_else(|| {
                if mime.starts_with("image/") || mime.is_empty() {
                    "application/octet-stream".into()
                } else {
                    mime.into()
                }
            }),
            bytes: bytes.len(),
            path: path.to_string_lossy().into(),
        };
        let metadata = serde_json::to_vec(&attachment).map_err(|_| "附件信息无效")?;
        std::fs::write(folder.join("attachment.json"), metadata).map_err(|_| "无法保存附件信息")?;
        Ok(attachment)
    }
    pub fn attachment_data(&self, room: &str, id: &str) -> Result<String> {
        self.conversation(room)?;
        let item = load(&self.path, room, id)?;
        let bytes = std::fs::read(&item.path).map_err(|_| "附件已不存在")?;
        if bytes.len() > MAX_BYTES {
            return Err("附件过大".into());
        }
        Ok(format!(
            "data:{};base64,{}",
            item.mime,
            STANDARD.encode(bytes)
        ))
    }
    pub fn paste_clipboard_files(&self, room: &str) -> Result<Vec<Attachment>> {
        if self.conversation(room)?.archived {
            return Err("请先恢复归档会话".into());
        }
        clipboard_files()?
            .into_iter()
            .map(|path| {
                let metadata = std::fs::metadata(&path).map_err(|_| "无法读取剪贴板附件")?;
                if !metadata.is_file() || metadata.len() > MAX_BYTES as u64 {
                    return Err("只能粘贴不超过32 MiB的文件".into());
                }
                let bytes = std::fs::read(&path).map_err(|_| "无法读取剪贴板附件")?;
                self.stage_attachment_bytes(
                    room,
                    path.file_name()
                        .and_then(|s| s.to_str())
                        .ok_or("附件名称无效")?,
                    "application/octet-stream",
                    &bytes,
                )
            })
            .collect()
    }
}
fn load(db: &Path, room: &str, id: &str) -> Result<Attachment> {
    Uuid::parse_str(id).map_err(|_| "附件标识无效")?;
    let folder = directory(db, room)?.join(id);
    let item: Attachment = serde_json::from_slice(
        &std::fs::read(folder.join("attachment.json")).map_err(|_| "附件不存在或不属于本会话")?,
    )
    .map_err(|_| "附件信息无效")?;
    let path = std::fs::canonicalize(&item.path).map_err(|_| "附件已不存在")?;
    let root = std::fs::canonicalize(folder).map_err(|_| "附件目录不可用")?;
    if !path.starts_with(root) {
        return Err("附件路径无效".into());
    }
    Ok(item)
}
pub fn items(content: &str) -> Vec<Attachment> {
    content
        .rsplit_once(OPEN)
        .and_then(|(_, rest)| rest.strip_suffix("```"))
        .and_then(|json| serde_json::from_str(json.trim()).ok())
        .unwrap_or_default()
}
pub fn text(content: &str) -> &str {
    content
        .rsplit_once(OPEN)
        .filter(|_| !items(content).is_empty())
        .map(|(text, _)| text.trim_end())
        .unwrap_or(content)
}
pub fn prompt(db: &Path, room: &str, content: &str, images: bool) -> Result<Vec<Value>> {
    let items = items(content)
        .into_iter()
        .take(20)
        .map(|item| load(db, room, &item.id))
        .collect::<Result<Vec<_>>>()?;
    // 主脑会话：新原生会话的第一条带上主脑说明；它问用户走问答面板，不用私聊提问的说明。
    let brain = crate::project_brain::preamble(db, room);
    let mut body = match &brain {
        Some(Some(preamble)) => format!("{preamble}\n\n{}", text(content)),
        _ => text(content).to_owned(),
    };
    if !items.is_empty() {
        body.push_str("\n\n用户附件已保存到本机，按需用原生工具读取。图片可用图片或视觉工具查看；若当前模型和工具无法查看，请如实说明，不猜测图像内容。\n");
        for item in &items {
            body.push_str(&format!("{}：{}\n", item.name, item.path));
        }
    }
    if brain.is_none() {
        body.push_str("\n\n");
        body.push_str(crate::chat_questions::GUIDANCE);
    }
    let mut input = vec![json!({"type":"text","text":body})];
    if images {
        for item in items.iter().filter(|item| item.mime.starts_with("image/")) {
            input.push(json!({"type":"localImage","path":item.path}));
        }
    }
    Ok(input)
}
#[cfg(windows)]
fn clipboard_files() -> Result<Vec<PathBuf>> {
    use windows_sys::Win32::{
        System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard},
        UI::Shell::DragQueryFileW,
    };
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return Ok(Vec::new());
        }
        struct Close;
        impl Drop for Close {
            fn drop(&mut self) {
                unsafe {
                    CloseClipboard();
                }
            }
        }
        let _close = Close;
        let handle = GetClipboardData(15);
        if handle.is_null() {
            return Ok(Vec::new());
        }
        let count = DragQueryFileW(handle, u32::MAX, std::ptr::null_mut(), 0);
        if count > 20 {
            return Err("一次最多粘贴20个附件".into());
        }
        let mut paths = Vec::new();
        for i in 0..count {
            let len = DragQueryFileW(handle, i, std::ptr::null_mut(), 0);
            let mut buffer = vec![0u16; len as usize + 1];
            DragQueryFileW(handle, i, buffer.as_mut_ptr(), len + 1);
            paths.push(PathBuf::from(String::from_utf16_lossy(
                &buffer[..len as usize],
            )));
        }
        Ok(paths)
    }
}
#[cfg(not(windows))]
fn clipboard_files() -> Result<Vec<PathBuf>> {
    Ok(Vec::new())
}
