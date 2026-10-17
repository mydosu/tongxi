//! 成员头像：用户换过就用用户的；否则用成员的原生图标——Hermes、DSH 从本机安装里读，
//! Claude 用 Claude Code 的像素小螃蟹 Clawd，Codex 用 Codex 应用图标（都来自 LobeHub 图标库，MIT）；
//! 都没有就退回字母头像（由界面画）。
//! 陪伴没有原生图标，头像就是它配置里的那张。
use crate::store::Store;
use base64::Engine;
use rusqlite::OptionalExtension;
use std::path::Path;

type Result<T> = std::result::Result<T, String>;

const CODEX: &str = include_str!("native_avatars/codex-color.svg");
const CLAUDE: &str = include_str!("native_avatars/claudecode-color.svg");

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

/// 没有底色的标志（DSH 的鲸鱼、Claude Code 的小螃蟹）垫一块白色圆角底，和其他应用图标一样是方形头像。
fn on_tile(inner: &str) -> String {
    svg_url(&format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect width="64" height="64" rx="14" fill="#ffffff"/><image href="{inner}" x="12" y="12" width="40" height="40"/></svg>"##
    ))
}

/// 成员的原生图标；本机找不到就是 None。
pub fn native(agent: &str, data: Option<&Path>) -> Option<String> {
    match agent {
        "codex-win" => Some(svg_url(CODEX)),
        "claude-win" => Some(on_tile(&svg_url(CLAUDE))),
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
    /// 用户给成员换的头像（陪伴的在它自己的配置里）。
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
            let text = serde_json::to_string(&config).map_err(|_| "陪伴配置保存失败")?;
            self.connection
                .execute("INSERT INTO metadata(key,value) VALUES('companion',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [text])
                .map_err(|_| "陪伴配置保存失败")?;
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

/// 你和群聊（含项目）的自定义头像；没换过就由界面画默认的「你」和群聊图标。
#[derive(serde::Serialize, Debug, Default, PartialEq)]
pub struct OwnAvatars {
    pub user: Option<String>,
    pub conversations: std::collections::BTreeMap<String, String>,
}

impl Store {
    pub fn own_avatars(&self) -> Result<OwnAvatars> {
        let mut statement = self
            .connection
            .prepare("SELECT key,value FROM metadata WHERE key='user_avatar' OR key LIKE 'conversation_avatar:%'")
            .map_err(|_| "头像读取失败")?;
        let rows = statement
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
            .map_err(|_| "头像读取失败")?;
        let mut avatars = OwnAvatars::default();
        for (key, value) in rows.flatten() {
            match key.strip_prefix("conversation_avatar:") {
                Some(id) => {
                    avatars.conversations.insert(id.to_owned(), value);
                }
                None => avatars.user = Some(value),
            }
        }
        Ok(avatars)
    }

    /// 换你的头像（conversation=None）或某个群聊的头像；avatar=None 恢复默认。
    pub fn set_own_avatar(&mut self, conversation: Option<&str>, avatar: Option<&str>) -> Result<()> {
        if avatar.is_some_and(|avatar| !valid(avatar)) {
            return Err("头像需要是图片（不超过 300KB）或纯色".into());
        }
        let key = match conversation {
            Some(id) => {
                let group: Option<String> = self
                    .connection
                    .query_row("SELECT kind FROM conversations WHERE id=?1", [id], |row| row.get(0))
                    .optional()
                    .map_err(|_| "会话读取失败")?;
                if group.as_deref() != Some("group") {
                    return Err("只能给群聊或项目换头像".into());
                }
                format!("conversation_avatar:{id}")
            }
            None => "user_avatar".to_owned(),
        };
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
pub fn own_avatars(state: tauri::State<'_, crate::AppState>) -> Result<OwnAvatars> {
    state.store.lock().map_err(|_| "本地数据暂不可用")?.own_avatars()
}

#[tauri::command]
pub fn set_own_avatar(state: tauri::State<'_, crate::AppState>, conversation_id: Option<String>, avatar: Option<String>) -> Result<()> {
    state.store.lock().map_err(|_| "本地数据暂不可用")?.set_own_avatar(conversation_id.as_deref(), avatar.as_deref())
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
        assert!(super::native("albion-wsl", None).is_none(), "陪伴没有原生图标");
        assert_eq!(super::without_style("<svg><style>p{}</style><path/></svg>"), "<svg><path/></svg>");
        let mut store = crate::store::Store::initialize(rusqlite::Connection::open_in_memory().unwrap(), std::path::PathBuf::from(":memory:")).unwrap();
        assert!(store.set_custom_avatar("codex-win", Some("not-an-image")).is_err());
        assert!(store.set_custom_avatar("codex-win", Some("color:#8b80c4")).is_ok(), "纯色头像");
        assert!(store.set_custom_avatar("codex-win", Some("color:red")).is_err());
        let group = store.create("项目群", "group", &["codex-win".into()]).unwrap();
        let direct = store.create("私聊", "direct", &["codex-win".into()]).unwrap();
        store.set_own_avatar(None, Some("color:#5fae86")).unwrap();
        store.set_own_avatar(Some(&group.id), Some("color:#d8a04f")).unwrap();
        assert!(store.set_own_avatar(Some(&direct.id), Some("color:#d8a04f")).is_err(), "私聊用成员头像");
        assert!(store.set_own_avatar(None, Some("javascript:x")).is_err());
        let own = store.own_avatars().unwrap();
        assert_eq!(own.user.as_deref(), Some("color:#5fae86"));
        assert_eq!(own.conversations.get(&group.id).map(String::as_str), Some("color:#d8a04f"));
        store.set_own_avatar(None, None).unwrap();
        assert_eq!(store.own_avatars().unwrap().user, None, "恢复默认");
        store.set_custom_avatar("codex-win", Some("data:image/webp;base64,AAAA")).unwrap();
        assert_eq!(store.custom_avatar("codex-win").as_deref(), Some("data:image/webp;base64,AAAA"));
        store.set_custom_avatar("codex-win", None).unwrap();
        assert!(store.custom_avatar("codex-win").is_none(), "恢复原生头像");
        store.set_custom_avatar("albion-wsl", Some("data:image/webp;base64,BB")).unwrap();
        assert_eq!(store.companion().avatar.as_deref(), Some("data:image/webp;base64,BB"));
    }
}
