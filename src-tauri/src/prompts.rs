//! 主脑和成员的说明放在外置文件里，改了下一回合就生效，不用重新打包重启：
//! 先找数据目录下的 prompts/（用户自己改的版本），调试版再找源码里的 prompts/，都没有就用打包进来的那份。
const BRAIN_DEFAULT: &str = include_str!("../prompts/brain.md");
const MODELS_DEFAULT: &str = include_str!("../prompts/models.md");
const MEMBER_DEFAULT: &str = include_str!("../prompts/member.md");
static PROMPT_DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

pub fn set_prompt_dir(directory: std::path::PathBuf) {
    let _ = PROMPT_DIR.set(directory);
}

/// 依次可能存放某份说明的外置文件，排在前面的优先。
pub fn prompt_files(name: &str) -> Vec<std::path::PathBuf> {
    let mut files: Vec<std::path::PathBuf> = PROMPT_DIR.get().map(|dir| dir.join(name)).into_iter().collect();
    if cfg!(debug_assertions) {
        files.push(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("prompts").join(name));
    }
    files
}

pub fn load(name: &str, fallback: &str) -> String {
    prompt_files(name)
        .iter()
        .find_map(|file| std::fs::read_to_string(file).ok())
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| fallback.trim().to_owned())
}

pub fn brain() -> String {
    load("brain.md", BRAIN_DEFAULT)
}

pub fn models() -> String {
    load("models.md", MODELS_DEFAULT)
}

pub fn member() -> String {
    load("member.md", MEMBER_DEFAULT)
}
