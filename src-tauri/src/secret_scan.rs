//! 自动提交前的敏感内容检查：只看这次新增的内容（已跟踪文件看 diff 里加的行，未跟踪文件看全文）。
//! 高置信度（私钥、各家访问令牌的固定格式）拦下不提交；低置信度（像是写死的密码、密钥赋值）只提醒。
//! 用户确认安全的文件写进 `.git/info/tongxi-secrets-allow`（每行一个路径，目录以 / 结尾，# 开头为注释），
//! 这个文件只在本机仓库里，不会被提交。
use std::path::Path;

#[derive(Clone, Debug, PartialEq)]
pub struct Finding {
    pub file: String,
    pub line: usize,
    pub kind: &'static str,
    pub high: bool,
}

pub const ALLOW_FILE: &str = "tongxi-secrets-allow";

/// 固定前缀 + 最短长度的令牌格式。
const TOKENS: [(&str, usize, &str); 12] = [
    ("AKIA", 16, "AWS 访问密钥"),
    ("ghp_", 36, "GitHub 令牌"),
    ("gho_", 36, "GitHub 令牌"),
    ("ghs_", 36, "GitHub 令牌"),
    ("github_pat_", 40, "GitHub 令牌"),
    ("glpat-", 20, "GitLab 令牌"),
    ("sk-ant-", 32, "Anthropic API Key"),
    ("sk-proj-", 32, "OpenAI API Key"),
    ("sk-", 32, "API Key"),
    ("xoxb-", 20, "Slack 令牌"),
    ("xoxp-", 20, "Slack 令牌"),
    ("AIza", 35, "Google API Key"),
];

fn token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// 占位符（xxxx、全同一字符、只有字母或只有数字）不算真令牌。
fn looks_real(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    let placeholder = ["xxxx", "your", "example", "placeholder", "dummy", "test", "fake", "0000", "1234"]
        .iter()
        .any(|word| lower.contains(word));
    let digits = body.chars().any(|c| c.is_ascii_digit());
    let letters = body.chars().any(|c| c.is_ascii_alphabetic());
    let distinct = {
        let mut chars: Vec<char> = body.chars().collect();
        chars.sort_unstable();
        chars.dedup();
        chars.len()
    };
    !placeholder && digits && letters && distinct >= 10
}

/// 检查一行，返回 (类型, 是否高置信度)。
pub fn scan_line(line: &str) -> Option<(&'static str, bool)> {
    if line.contains("-----BEGIN") && line.contains("PRIVATE KEY-----") {
        return Some(("私钥", true));
    }
    for (prefix, min, kind) in TOKENS {
        let mut rest = line;
        while let Some(at) = rest.find(prefix) {
            let before = rest[..at].chars().next_back();
            let body: String = rest[at + prefix.len()..].chars().take_while(|c| token_char(*c)).collect();
            if !before.is_some_and(token_char) && body.len() >= min && looks_real(&body) {
                return Some((kind, true));
            }
            rest = &rest[at + prefix.len()..];
        }
    }
    let lower = line.to_ascii_lowercase();
    let named = ["password", "passwd", "secret", "api_key", "apikey", "access_key", "private_key", "auth_token"]
        .iter()
        .any(|word| lower.contains(word));
    if named {
        let quoted = line
            .split(['"', '\''])
            .enumerate()
            .filter(|(index, _)| index % 2 == 1)
            .any(|(_, value)| {
                let value_lower = value.to_ascii_lowercase();
                value.len() >= 12
                    && !value.contains(' ')
                    && !["${", "{{", "<", "env", "your", "example", "xxx", "changeme", "placeholder"]
                        .iter()
                        .any(|word| value_lower.contains(word))
            });
        let assigned = lower.contains('=') || lower.contains(':');
        if quoted && assigned {
            return Some(("疑似写死的密码或密钥", false));
        }
    }
    None
}

fn allowed(root: &Path, file: &str) -> bool {
    let Ok(git_dir) = crate::project_worktree::git_dir(root) else {
        return false;
    };
    let Ok(text) = std::fs::read_to_string(git_dir.join("info").join(ALLOW_FILE)) else {
        return false;
    };
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .any(|entry| {
            let entry = entry.trim_start_matches('/');
            file == entry || (entry.ends_with('/') && file.starts_with(entry))
        })
}

/// 扫描 `files` 里这次新增的内容。仓库没有 HEAD（还没提交过）时按全文扫。
pub fn scan(root: &Path, files: &[String]) -> Vec<Finding> {
    let mut findings = Vec::new();
    let files: Vec<&String> = files.iter().filter(|file| !allowed(root, file)).collect();
    if files.is_empty() {
        return findings;
    }
    let tracked = crate::project_worktree::added_lines(root, &files).unwrap_or_default();
    for file in files {
        let lines: Vec<(usize, String)> = match tracked.get(file.as_str()) {
            Some(lines) => lines.clone(),
            None => {
                let Ok(bytes) = std::fs::read(root.join(file)) else { continue };
                // 二进制和大文件不扫。
                if bytes.len() > 1_000_000 || bytes.contains(&0) {
                    continue;
                }
                String::from_utf8_lossy(&bytes)
                    .lines()
                    .enumerate()
                    .map(|(index, line)| (index + 1, line.to_owned()))
                    .collect()
            }
        };
        for (line, text) in lines {
            if let Some((kind, high)) = scan_line(&text) {
                findings.push(Finding { file: file.clone(), line, kind, high });
            }
        }
    }
    findings
}

/// 给团队看的一句说明。
pub fn describe(findings: &[Finding]) -> String {
    let mut parts: Vec<String> = findings
        .iter()
        .take(6)
        .map(|finding| format!("{}:{}（{}）", finding.file, finding.line, finding.kind))
        .collect();
    if findings.len() > 6 {
        parts.push(format!("等 {} 处", findings.len()));
    }
    parts.join("、")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_tokens_are_high_and_placeholders_are_ignored() {
        assert_eq!(scan_line("-----BEGIN OPENSSH PRIVATE KEY-----"), Some(("私钥", true)));
        // 假令牌拼出来，源码里不留完整字面量（免得被同席自己或 GitHub 的扫描当成真的）。
        let fake = |prefix: &str, body: &str| format!("{prefix}{body}");
        assert_eq!(scan_line(&format!("key = \"{}\"", fake("ghp_", "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8"))), Some(("GitHub 令牌", true)));
        assert_eq!(scan_line(&format!("ANTHROPIC_API_KEY={}", fake("sk-ant-", "api03-Zx8Qw2Lk9Pm4Rt7Yb1Nc5Vd3Hg6Jf0Ae"))), Some(("Anthropic API Key", true)));
        assert_eq!(scan_line(&format!("AWS={}", fake("AKIA", "Z3X9Q2L7M4N8P1R6"))), Some(("AWS 访问密钥", true)));
        assert_eq!(scan_line("OPENAI_API_KEY=sk-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"), None);
        assert_eq!(scan_line("const key = process.env.OPENAI_API_KEY;"), None);
        assert_eq!(scan_line("task-sk-1234 is a ticket"), None);
        assert_eq!(scan_line("-----BEGIN PUBLIC KEY-----"), None);
    }

    #[test]
    fn hardcoded_passwords_only_warn() {
        assert_eq!(scan_line("password = \"Tr0ub4dor&3horse\""), Some(("疑似写死的密码或密钥", false)));
        assert_eq!(scan_line("password = \"${DB_PASSWORD}\""), None);
        assert_eq!(scan_line("api_key: \"your-api-key-here\""), None);
        assert_eq!(scan_line("let password = read_password();"), None);
    }
}
