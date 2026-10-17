//! 情感陪伴：每个人可以接入自己的陪伴（成员 id 仍是 `albion-wsl`，名字、头像、接口都由用户配置）。
//!
//! 接入方式只有一种、也最通用：**OpenAI 兼容的对话接口**（`/v1/models` + `/v1/chat/completions` 流式）。
//! 本机自建的陪伴服务（例如阿尔比恩的 Pi 脑 `http://127.0.0.1:8650`）、Ollama、LM Studio，
//! 以及 OpenAI / DeepSeek 这类云端接口都能接。人格、记忆在对方服务里时同席只当客户端；
//! 没有自建服务时可以填「人设」，作为 system 消息发过去。
//!
//! Key 不明文落库：直接填写的 Key 用 Windows DPAPI（只有当前 Windows 用户能解开）加密后存在本机库；
//! 也可以只记一个环境变量名或 `.env` 文件位置，同席每次用时现读。
use crate::store::Store;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

type Result<T> = std::result::Result<T, String>;

const KEY: &str = "companion";
/// 阿尔比恩 Pi 脑的 `.env`（老版本写死的接入方式；检测到就预设成她）。
const ALBION_ENV_FILES: [&str; 2] = [
    r"\\wsl.localhost\Ubuntu\root\albion-pi\agent\.env",
    r"\\wsl.localhost\Ubuntu\root\.hermes\profiles\albion\.env",
];

/// 存在本机库里的完整配置（含加密后的 Key）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CompanionConfig {
    pub configured: bool,
    pub name: String,
    pub subtitle: String,
    /// 接口地址，例如 `http://127.0.0.1:8650`、`http://127.0.0.1:11434/v1`、`https://api.deepseek.com`。
    pub endpoint: String,
    /// `none` | `env` | `file` | `inline`
    pub key_source: String,
    pub key_env: Option<String>,
    pub key_file: Option<String>,
    /// `.env` 文件里的变量名（默认 `API_KEY`）。
    pub key_name: Option<String>,
    /// DPAPI 加密后的 Key（base64）；永远不回传给界面（为空时连字段都不输出）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_secret: Option<String>,
    pub model: Option<String>,
    pub persona: Option<String>,
    /// 头像（data URL，界面已缩到 256px）。
    pub avatar: Option<String>,
}

/// 给界面看的配置：去掉密文，只说有没有。
#[derive(Clone, Debug, Serialize)]
pub struct CompanionView {
    #[serde(flatten)]
    pub config: CompanionConfig,
    pub has_secret: bool,
}

/// 界面提交的配置；`secret` 为新填的 Key（空字符串＝清除，缺省＝保留原来的）。
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompanionInput {
    pub name: String,
    pub subtitle: Option<String>,
    pub endpoint: String,
    pub key_source: String,
    pub key_env: Option<String>,
    pub key_file: Option<String>,
    pub key_name: Option<String>,
    pub secret: Option<String>,
    pub model: Option<String>,
    pub persona: Option<String>,
    pub avatar: Option<String>,
}

/// 一次请求要用到的东西。
#[derive(Clone, Debug)]
pub struct Link {
    pub base: String,
    pub key: Option<String>,
    pub model: Option<String>,
    pub persona: Option<String>,
}

fn clean(value: Option<String>) -> Option<String> {
    value.map(|value| value.trim().to_string()).filter(|value| !value.is_empty())
}

/// 没保存过配置时的初始状态：本机有阿尔比恩的脑就预设成她（老用户升级无感），否则是待配置。
fn preset() -> CompanionConfig {
    if let Some(file) = ALBION_ENV_FILES.iter().find(|path| std::path::Path::new(path).is_file()) {
        return CompanionConfig {
            configured: true,
            name: "阿尔比恩".into(),
            subtitle: "WSL 本机服务".into(),
            endpoint: "http://127.0.0.1:8650".into(),
            key_source: "file".into(),
            key_file: Some((*file).into()),
            key_name: Some("API_SERVER_KEY".into()),
            ..CompanionConfig::default()
        };
    }
    CompanionConfig {
        configured: false,
        name: "陪伴".into(),
        subtitle: "待配置".into(),
        key_source: "none".into(),
        ..CompanionConfig::default()
    }
}

impl Store {
    pub fn companion(&self) -> CompanionConfig {
        self.connection
            .query_row("SELECT value FROM metadata WHERE key=?1", [KEY], |row| row.get::<_, String>(0))
            .optional()
            .ok()
            .flatten()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_else(preset)
    }

    pub fn set_companion(&mut self, input: CompanionInput) -> Result<CompanionConfig> {
        let mut config = self.companion();
        let name: String = input.name.trim().chars().filter(|c| !c.is_control()).take(20).collect();
        if name.is_empty() {
            return Err("给陪伴起个名字".into());
        }
        let endpoint = input.endpoint.trim().to_string();
        normalize(&endpoint)?;
        if !matches!(input.key_source.as_str(), "none" | "env" | "file" | "inline") {
            return Err("Key 来源无效".into());
        }
        if let Some(avatar) = &input.avatar {
            if !avatar.is_empty() && !crate::avatars::valid(avatar) {
                return Err("头像需要是图片，且不超过 300KB".into());
            }
        }
        config.configured = true;
        config.name = name;
        config.subtitle = clean(input.subtitle).unwrap_or_else(|| "情感陪伴".into());
        config.endpoint = endpoint;
        config.key_source = input.key_source;
        config.key_env = clean(input.key_env);
        config.key_file = clean(input.key_file);
        config.key_name = clean(input.key_name);
        config.model = clean(input.model);
        config.persona = input.persona.map(|text| text.trim().chars().take(8000).collect::<String>()).filter(|text| !text.is_empty());
        config.avatar = input.avatar.filter(|avatar| !avatar.is_empty());
        match input.secret.as_deref().map(str::trim) {
            Some("") => config.key_secret = None,
            Some(secret) => config.key_secret = Some(protect(secret)?),
            None => {}
        }
        if config.key_source != "inline" {
            config.key_secret = None;
        }
        let text = serde_json::to_string(&config).map_err(|_| "陪伴配置保存失败")?;
        self.connection
            .execute(
                "INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![KEY, text],
            )
            .map_err(|_| "陪伴配置保存失败")?;
        Ok(config)
    }
}

/// 用户填的地址 → 请求根（以 `/v1` 这类版本段结尾）。只有主机没有路径时补 `/v1`。
pub fn normalize(endpoint: &str) -> Result<String> {
    let endpoint = endpoint.trim().trim_end_matches('/');
    if endpoint.is_empty() {
        return Err("填写陪伴的接口地址".into());
    }
    let full = if endpoint.contains("://") { endpoint.to_string() } else { format!("http://{endpoint}") };
    let (scheme, rest) = full.split_once("://").ok_or("接口地址无效")?;
    if !matches!(scheme, "http" | "https") || rest.is_empty() || rest.contains(char::is_whitespace) {
        return Err("接口地址需以 http:// 或 https:// 开头".into());
    }
    Ok(if rest.contains('/') { full } else { format!("{full}/v1") })
}

fn key_from_file(path: &str, name: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()?.lines().find_map(|line| {
        let line = line.trim().strip_prefix("export ").unwrap_or(line.trim());
        let (key, value) = line.split_once('=')?;
        (key.trim() == name).then(|| value.trim().trim_matches('"').trim_matches('\'').to_string())
    }).filter(|value| !value.is_empty())
}

impl CompanionConfig {
    /// 解析出这次请求用的地址与 Key。`AGENT_HUB_ALBION_ENDPOINT` / `_TOKEN` 仍可覆盖（验收脚本用）。
    pub fn link(&self) -> Result<Link> {
        if !self.configured {
            return Err("还没有配置陪伴：到「成员与服务」里点陪伴卡片上的「配置陪伴」".into());
        }
        let endpoint = std::env::var("AGENT_HUB_ALBION_ENDPOINT").unwrap_or_else(|_| self.endpoint.clone());
        let base = normalize(&endpoint)?;
        let key = match std::env::var("AGENT_HUB_ALBION_TOKEN").ok().filter(|value| !value.trim().is_empty()) {
            Some(token) => Some(token.trim().to_string()),
            None => match self.key_source.as_str() {
                "env" => {
                    let name = self.key_env.as_deref().ok_or("填写存放 Key 的环境变量名")?;
                    Some(std::env::var(name).map_err(|_| format!("环境变量 {name} 不存在"))?)
                }
                "file" => {
                    let path = self.key_file.as_deref().ok_or("填写 .env 文件位置")?;
                    let name = self.key_name.as_deref().unwrap_or("API_KEY");
                    Some(key_from_file(path, name).ok_or(format!("在 {path} 里没找到 {name}"))?)
                }
                "inline" => Some(unprotect(self.key_secret.as_deref().ok_or("还没有填写 Key")?)?),
                _ => None,
            },
        };
        Ok(Link { base, key, model: self.model.clone(), persona: self.persona.clone() })
    }
}

#[cfg(windows)]
fn dpapi(input: &[u8], encrypt: bool) -> Result<Vec<u8>> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    let data = CRYPT_INTEGER_BLOB { cbData: input.len() as u32, pbData: input.as_ptr() as *mut u8 };
    let mut output = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
    let ok = unsafe {
        if encrypt {
            CryptProtectData(&data, std::ptr::null(), std::ptr::null(), std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut output)
        } else {
            CryptUnprotectData(&data, std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut output)
        }
    };
    if ok == 0 || output.pbData.is_null() {
        return Err(if encrypt { "Key 加密失败" } else { "Key 解密失败（换了 Windows 用户？请重新填写）" }.into());
    }
    let bytes = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
    unsafe { LocalFree(output.pbData as _) };
    Ok(bytes)
}

#[cfg(not(windows))]
fn dpapi(input: &[u8], _encrypt: bool) -> Result<Vec<u8>> {
    Ok(input.to_vec())
}

fn protect(secret: &str) -> Result<String> {
    use base64::Engine;
    Ok(base64::engine::general_purpose::STANDARD.encode(dpapi(secret.as_bytes(), true)?))
}

fn unprotect(stored: &str) -> Result<String> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(stored).map_err(|_| "Key 已损坏，请重新填写")?;
    String::from_utf8(dpapi(&bytes, false)?).map_err(|_| "Key 已损坏，请重新填写".into())
}

pub fn view(config: CompanionConfig) -> CompanionView {
    let has_secret = config.key_secret.is_some();
    CompanionView { config: CompanionConfig { key_secret: None, ..config }, has_secret }
}

#[tauri::command]
pub fn companion_config(state: tauri::State<'_, crate::AppState>) -> Result<CompanionView> {
    let store = state.store.lock().map_err(|_| "本地数据暂不可用")?;
    Ok(view(store.companion()))
}

#[tauri::command]
pub fn save_companion(
    state: tauri::State<'_, crate::AppState>,
    runtime: tauri::State<'_, crate::albion::Runtime>,
    config: CompanionInput,
) -> Result<CompanionView> {
    let saved = state.store.lock().map_err(|_| "本地数据暂不可用")?.set_companion(config)?;
    // 换了接口就断开，下次连接按新配置来。
    runtime.0.stop();
    Ok(view(saved))
}

/// 测试连接：按界面上还没保存的配置取一次模型目录。
#[tauri::command]
pub async fn test_companion(state: tauri::State<'_, crate::AppState>, config: CompanionInput) -> Result<Vec<String>> {
    let mut draft = state.store.lock().map_err(|_| "本地数据暂不可用")?.companion();
    draft.configured = true;
    draft.endpoint = config.endpoint;
    draft.key_source = config.key_source;
    draft.key_env = clean(config.key_env);
    draft.key_file = clean(config.key_file);
    draft.key_name = clean(config.key_name);
    if let Some(secret) = config.secret.filter(|secret| !secret.trim().is_empty()) {
        draft.key_secret = Some(protect(secret.trim())?);
    }
    let link = draft.link()?;
    tauri::async_runtime::spawn_blocking(move || crate::albion_openai::models(&link))
        .await
        .map_err(|error| error.to_string())?
}

#[cfg(test)]
mod tests {
    use super::normalize;

    #[test]
    fn endpoints_get_a_version_segment_only_when_they_have_no_path() {
        assert_eq!(normalize("127.0.0.1:8650").unwrap(), "http://127.0.0.1:8650/v1");
        assert_eq!(normalize("http://127.0.0.1:11434/v1/").unwrap(), "http://127.0.0.1:11434/v1");
        assert_eq!(normalize("https://api.deepseek.com").unwrap(), "https://api.deepseek.com/v1");
        assert_eq!(normalize("https://ark.example.com/api/v3").unwrap(), "https://ark.example.com/api/v3");
        assert!(normalize("ftp://x").is_err());
        assert!(normalize("").is_err());
    }

    #[test]
    fn inline_keys_round_trip_through_dpapi_and_never_reach_the_view() {
        let stored = super::protect("sk-test-123").unwrap();
        assert!(!stored.contains("sk-test-123"));
        assert_eq!(super::unprotect(&stored).unwrap(), "sk-test-123");
        let view = super::view(super::CompanionConfig { key_secret: Some(stored), ..Default::default() });
        assert!(view.has_secret && view.config.key_secret.is_none());
    }

    #[test]
    fn saved_config_shapes_the_link() {
        let mut store = crate::store::Store::initialize(rusqlite::Connection::open_in_memory().unwrap(), std::path::PathBuf::from(":memory:")).unwrap();
        let saved = store
            .set_companion(super::CompanionInput {
                name: "小夜".into(),
                subtitle: None,
                endpoint: "http://127.0.0.1:11434".into(),
                key_source: "inline".into(),
                key_env: None,
                key_file: None,
                key_name: None,
                secret: Some("sk-local".into()),
                model: Some("qwen3".into()),
                persona: Some("温柔、话不多".into()),
                avatar: None,
            })
            .unwrap();
        assert_eq!(saved.name, "小夜");
        let link = store.companion().link().unwrap();
        assert_eq!(link.base, "http://127.0.0.1:11434/v1");
        assert_eq!(link.key.as_deref(), Some("sk-local"));
        assert_eq!(link.model.as_deref(), Some("qwen3"));
    }
}
