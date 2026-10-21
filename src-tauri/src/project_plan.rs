//! 项目的长期计划：放在项目根目录的 `.tongxi/计划.md`，跨协作记住总体目标、方案要点、各板块范围与状态、
//! 关键决定和相关文档的路径。调度每回合在参考资料里看到它（几回合不变，命中提示缓存），有变化时交新版，
//! 同席写回文件；用户也可以直接改，下一回合以文件为准。方案任务的全文另存到 `.tongxi/方案存档/`，计划里按路径引用。
//! `.tongxi/` 记进 `.git/info/exclude`：不进提交、不会被推送。
use std::path::{Path, PathBuf};

type Result<T> = std::result::Result<T, String>;

pub const DIR: &str = ".tongxi";
pub const PLAN: &str = "计划.md";
const PREVIOUS: &str = "计划.上一版.md";
const ARCHIVE: &str = "方案存档";
/// 计划的长度上限（字）：放进调度每回合的参考资料，太长就失去缓存和阅读上的意义。
pub const LIMIT: usize = 12_000;

fn ensure(root: &Path) -> Result<PathBuf> {
    let dir = root.join(DIR);
    std::fs::create_dir_all(&dir).map_err(|_| "计划目录创建失败")?;
    let exclude = root.join(".git").join("info").join("exclude");
    if root.join(".git").is_dir() {
        let text = std::fs::read_to_string(&exclude).unwrap_or_default();
        if !text.lines().any(|line| line.trim() == ".tongxi/") {
            let _ = std::fs::create_dir_all(exclude.parent().unwrap());
            let separator = if text.is_empty() || text.ends_with('\n') { "" } else { "\n" };
            std::fs::write(&exclude, format!("{text}{separator}.tongxi/\n")).map_err(|_| "计划目录排除规则写入失败")?;
        }
    }
    Ok(dir)
}

/// 读计划给调度看；没有就是 None。超长时截断并注明。
pub fn read(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join(DIR).join(PLAN)).ok()?;
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let count = text.chars().count();
    Some(if count > LIMIT {
        format!("{}\n……（计划超过 {LIMIT} 字，后面 {} 字没给出，请压缩）", text.chars().take(LIMIT).collect::<String>(), count - LIMIT)
    } else {
        text.to_owned()
    })
}

/// 写入新版计划，旧版留一份 `计划.上一版.md`。
pub fn write(root: &Path, text: &str) -> Result<()> {
    let dir = ensure(root)?;
    let path = dir.join(PLAN);
    if let Ok(previous) = std::fs::read_to_string(&path) {
        if previous.trim() == text.trim() {
            return Ok(());
        }
        let _ = std::fs::write(dir.join(PREVIOUS), previous);
    }
    std::fs::write(&path, format!("{}\n", text.trim_end())).map_err(|_| "计划文件写入失败".into())
}

/// 方案全文存档，返回相对项目根的路径（给计划和群消息引用）。同名同日的覆盖为最新一版。
pub fn archive(root: &Path, title: &str, output: &str, now_ms: i64) -> Result<String> {
    let dir = ensure(root)?.join(ARCHIVE);
    std::fs::create_dir_all(&dir).map_err(|_| "方案存档目录创建失败")?;
    let (offset, _) = crate::quota_reset::local_offset();
    let date = crate::quota_reset::iso_utc(now_ms / 1000 + offset);
    let name: String = title
        .chars()
        .map(|ch| if ch.is_control() || r#"\/:*?"<>|"#.contains(ch) { '_' } else { ch })
        .take(40)
        .collect();
    let file = format!("{}-{}.md", &date[..10], name.trim());
    std::fs::write(dir.join(&file), format!("# {title}\n\n{}\n", output.trim_end())).map_err(|_| "方案存档写入失败")?;
    Ok(format!("{DIR}/{ARCHIVE}/{file}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_lives_in_an_excluded_folder_and_keeps_the_previous_version() {
        let root = std::env::temp_dir().join(format!("plan-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join(".git").join("info")).unwrap();
        assert_eq!(read(&root), None);
        write(&root, "# 计划\n第一版").unwrap();
        write(&root, "# 计划\n第二版").unwrap();
        assert_eq!(read(&root).as_deref(), Some("# 计划\n第二版"));
        assert!(std::fs::read_to_string(root.join(DIR).join(PREVIOUS)).unwrap().contains("第一版"));
        let exclude = std::fs::read_to_string(root.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.matches(".tongxi/").count(), 1);
        let path = archive(&root, "方案 · 壳/脑: 重构", "全文", 1_792_548_590_594).unwrap();
        assert!(path.starts_with(".tongxi/方案存档/2026-") && path.ends_with("方案 · 壳_脑_ 重构.md"), "{path}");
        assert!(std::fs::read_to_string(root.join(&path)).unwrap().contains("全文"));
        write(&root, &"长".repeat(LIMIT + 10)).unwrap();
        assert!(read(&root).unwrap().contains("请压缩"));
        let _ = std::fs::remove_dir_all(root);
    }
}
