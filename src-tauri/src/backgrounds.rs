//! 自定义主题背景：用户导入自己的图片（界面先压缩成 webp），存在本机库里，只有这台电脑看得到。
//! 内置主题是一套不含任何角色的公用背景；个人图片一律走这里，不打包进程序。
use crate::store::{now, Store};
use rusqlite::{params, Transaction};
use serde::Serialize;

type Result<T> = std::result::Result<T, String>;

pub fn migrate(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS backgrounds (
           id TEXT PRIMARY KEY,name TEXT NOT NULL,mode TEXT NOT NULL CHECK(mode IN ('cover','figure')),
           image TEXT NOT NULL,thumb TEXT NOT NULL,created_at INTEGER NOT NULL);",
    )
    .map_err(|_| "背景表迁移失败".into())
}

#[derive(Serialize)]
pub struct Background {
    pub id: String,
    pub name: String,
    pub mode: String,
    pub thumb: String,
}

fn image(data: &str, limit: usize) -> Result<()> {
    if !data.starts_with("data:image/") || data.len() > limit {
        return Err("图片无效或太大".into());
    }
    Ok(())
}

impl Store {
    pub fn backgrounds(&self) -> Result<Vec<Background>> {
        let mut statement = self
            .connection
            .prepare("SELECT id,name,mode,thumb FROM backgrounds ORDER BY created_at,id")
            .map_err(|_| "背景读取失败")?;
        let rows = statement
            .query_map([], |row| Ok(Background { id: row.get(0)?, name: row.get(1)?, mode: row.get(2)?, thumb: row.get(3)? }))
            .map_err(|_| "背景读取失败")?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(|_| "背景读取失败".into())
    }

    pub fn add_background(&mut self, name: &str, mode: &str, data: &str, thumb: &str) -> Result<String> {
        if !matches!(mode, "cover" | "figure") {
            return Err("显示方式无效".into());
        }
        image(data, 8_000_000)?;
        image(thumb, 300_000)?;
        let count: i64 = self.connection.query_row("SELECT COUNT(*) FROM backgrounds", [], |row| row.get(0)).map_err(|_| "背景读取失败")?;
        if count >= 40 {
            return Err("最多保存 40 张自定义背景，先删掉几张".into());
        }
        let id = uuid::Uuid::new_v4().to_string();
        let name: String = name.trim().chars().filter(|c| !c.is_control()).take(40).collect();
        self.connection
            .execute(
                "INSERT INTO backgrounds(id,name,mode,image,thumb,created_at) VALUES(?1,?2,?3,?4,?5,?6)",
                params![id, if name.is_empty() { "我的图片".into() } else { name }, mode, data, thumb, now()],
            )
            .map_err(|_| "背景保存失败")?;
        Ok(id)
    }
}

#[tauri::command]
pub fn list_backgrounds(state: tauri::State<'_, crate::AppState>) -> Result<Vec<Background>> {
    state.store.lock().map_err(|_| "本地数据暂不可用")?.backgrounds()
}

#[tauri::command]
pub fn background_image(state: tauri::State<'_, crate::AppState>, id: String) -> Result<String> {
    state
        .store
        .lock()
        .map_err(|_| "本地数据暂不可用")?
        .connection
        .query_row("SELECT image FROM backgrounds WHERE id=?1", [id], |row| row.get(0))
        .map_err(|_| "这张背景已被删除".into())
}

#[tauri::command]
pub fn add_background(state: tauri::State<'_, crate::AppState>, name: String, mode: String, image: String, thumb: String) -> Result<String> {
    state.store.lock().map_err(|_| "本地数据暂不可用")?.add_background(&name, &mode, &image, &thumb)
}

#[tauri::command]
pub fn set_background_mode(state: tauri::State<'_, crate::AppState>, id: String, mode: String) -> Result<()> {
    if !matches!(mode.as_str(), "cover" | "figure") {
        return Err("显示方式无效".into());
    }
    state
        .store
        .lock()
        .map_err(|_| "本地数据暂不可用")?
        .connection
        .execute("UPDATE backgrounds SET mode=?1 WHERE id=?2", params![mode, id])
        .map(|_| ())
        .map_err(|_| "背景保存失败".into())
}

#[tauri::command]
pub fn remove_background(state: tauri::State<'_, crate::AppState>, id: String) -> Result<()> {
    state
        .store
        .lock()
        .map_err(|_| "本地数据暂不可用")?
        .connection
        .execute("DELETE FROM backgrounds WHERE id=?1", [id])
        .map(|_| ())
        .map_err(|_| "背景删除失败".into())
}

#[cfg(test)]
mod tests {
    #[test]
    fn imported_backgrounds_are_validated_and_listed_without_full_images() {
        let mut store = crate::store::Store::initialize(rusqlite::Connection::open_in_memory().unwrap(), std::path::PathBuf::from(":memory:")).unwrap();
        assert!(store.add_background("x", "cover", "not-an-image", "data:image/webp;base64,AA").is_err());
        assert!(store.add_background("x", "tiled", "data:image/webp;base64,AA", "data:image/webp;base64,AA").is_err());
        let id = store.add_background("窗边", "figure", "data:image/webp;base64,AAAA", "data:image/webp;base64,AA").unwrap();
        let list = store.backgrounds().unwrap();
        assert_eq!((list.len(), list[0].id.as_str(), list[0].mode.as_str()), (1, id.as_str(), "figure"));
    }
}
