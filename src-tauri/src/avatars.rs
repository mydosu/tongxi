//! 成员头像：用户换过就用用户的；否则用成员的原生图标——Hermes、DSH 从本机安装里读，
//! Claude 用 claude.ai 官方发布的应用图标，Codex 用 LobeHub 图标库（MIT）里的 Codex 应用图标；
//! 都没有就退回字母头像（由界面画）。
//! 伴侣没有原生图标，头像就是它配置里的那张。
use crate::store::Store;
use base64::Engine;
use rusqlite::OptionalExtension;
use std::path::Path;

type Result<T> = std::result::Result<T, String>;

const CODEX: &str = include_str!("native_avatars/codex-color.svg");
const CLAUDE: &[u8] = include_bytes!("native_avatars/claude-app.png");

/// 头像值：图片（data URL，≤300KB）或纯色 `color:#rrggbb`（界面用它画「纯色底 + 名字首字」）。
pub fn valid(avatar: &str) -> bool {
    let solid = avatar
        .strip_prefix("color:#")
        .is_some_and(|hex| hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()));
    solid || (avatar.starts_with("data:image/") && avatar.len() <= 400_000)
}

fn svg_url(svg: &str) -> String {
    format!("data:image/svg+xml;base64,{}", base64::engine::general_purpose::STANDARD.encode(svg))
}

/// 去掉 SVG 里随系统深色模式变色的样式（深色下会变成白色，放在浅色头像底上就看不见了）。
fn without_style(svg: &str) -> String {
    match (svg.find("<style"), svg.find("</style>")) {
        (Some(start), Some(end)) if end > start => format!("{}{}", &svg[..start], &svg[end + "</style>".len()..]),
        _ => svg.to_owned(),
    }
}

fn read_svg(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    (text.len() < 200_000 && text.contains("<svg")).then(|| svg_url(&without_style(&text)))
}

/// 没有底色的标志（DSH 的鲸鱼）垫一块白色圆角底，和其他应用图标一样是方形头像。
fn on_tile(inner: &str) -> String {
    svg_url(&format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect width="64" height="64" rx="14" fill="#ffffff"/><image href="{inner}" x="12" y="12" width="40" height="40"/></svg>"##
    ))
}

/// 成员的原生图标；本机找不到就是 None。
pub fn native(agent: &str, data: Option<&Path>) -> Option<String> {
    match agent {
        "codex-win" => Some(svg_url(CODEX)),
        "claude-win" => Some(format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(CLAUDE))),
        "hermes-win" => {
            let python = crate::acp_transport::Target::windows_python()?;
            let repo = python.parent()?.parent()?.parent()?;
            read_svg(&repo.join("assets").join("icon-master.svg"))
        }
        "dsh-win" => {
            let package = crate::service_install::dsh_path(data?).ok()?;
            let candidates = [
                package.join("node_modules/@deepseek-ai/dsh-web-frontend/dist/favicon.svg"),
                package.parent()?.join("dsh-web-frontend/dist/favicon.svg"),
            ];
            candidates.iter().find_map(|path| read_svg(path)).map(|logo| on_tile(&logo))
        }
        _ => None,
    }
}

impl Store {
    /// 用户给成员换的头像（伴侣的在它自己的配置里）。
    pub fn custom_avatar(&self, agent: &str) -> Option<String> {
        if agent == "albion-wsl" {
            return self.companion().avatar;
        }
        self.connection
            .query_row("SELECT value FROM metadata WHERE key=?1", [format!("agent_avatar:{agent}")], |row| row.get(0))
            .optional()
            .ok()
            .flatten()
    }

    /// 换头像；None＝恢复原生头像。
    pub fn set_custom_avatar(&mut self, agent: &str, avatar: Option<&str>) -> Result<()> {
        if !crate::store::agents().iter().any(|item| item.id == agent) {
            return Err("成员不存在".into());
        }
        if let Some(avatar) = avatar {
            if !valid(avatar) {
                return Err("头像需要是图片（不超过 300KB）或纯色".into());
            }
        }
        if agent == "albion-wsl" {
            let mut config = self.companion();
            config.avatar = avatar.map(String::from);
            let text = serde_json::to_string(&config).map_err(|_| "伴侣配置保存失败")?;
            self.connection
                .execute("INSERT INTO metadata(key,value) VALUES('companion',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [text])
                .map_err(|_| "伴侣配置保存失败")?;
            return Ok(());
        }
        let key = format!("agent_avatar:{agent}");
        match avatar {
            Some(avatar) => self.connection.execute(
                "INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                rusqlite::params![key, avatar],
            ),
            None => self.connection.execute("DELETE FROM metadata WHERE key=?1", [key]),
        }
        .map(|_| ())
        .map_err(|_| "头像保存失败".into())
    }
}

#[tauri::command]
pub fn set_agent_avatar(state: tauri::State<'_, crate::AppState>, agent_id: String, avatar: Option<String>) -> Result<()> {
    state.store.lock().map_err(|_| "本地数据暂不可用")?.set_custom_avatar(&agent_id, avatar.as_deref())
}

#[cfg(test)]
mod tests {
    #[test]
    fn bundled_icons_are_svg_and_custom_avatars_override_them() {
        assert!(super::native("codex-win", None).unwrap().starts_with("data:image/svg+xml;base64,"));
        assert!(super::native("claude-win", None).is_some());
        assert!(super::native("albion-wsl", None).is_none(), "伴侣没有原生图标");
        assert_eq!(super::without_style("<svg><style>p{}</style><path/></svg>"), "<svg><path/></svg>");
        let mut store = crate::store::Store::initialize(rusqlite::Connection::open_in_memory().unwrap(), std::path::PathBuf::from(":memory:")).unwrap();
        assert!(store.set_custom_avatar("codex-win", Some("not-an-image")).is_err());
        assert!(store.set_custom_avatar("codex-win", Some("color:#8b80c4")).is_ok(), "纯色头像");
        assert!(store.set_custom_avatar("codex-win", Some("color:red")).is_err());
        store.set_custom_avatar("codex-win", Some("data:image/webp;base64,AAAA")).unwrap();
        assert_eq!(store.custom_avatar("codex-win").as_deref(), Some("data:image/webp;base64,AAAA"));
        store.set_custom_avatar("codex-win", None).unwrap();
        assert!(store.custom_avatar("codex-win").is_none(), "恢复原生头像");
        store.set_custom_avatar("albion-wsl", Some("data:image/webp;base64,BB")).unwrap();
        assert_eq!(store.companion().avatar.as_deref(), Some("data:image/webp;base64,BB"));
    }
}
