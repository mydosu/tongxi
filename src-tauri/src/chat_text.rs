/// Hide structured Albion voice-control tags, including partial stream chunks.
/// User messages and other agents' messages remain literal text.
pub fn albion_visible(raw: &str) -> String {
    visible(raw, false)
}

pub fn albion_complete(raw: &str) -> String {
    visible(raw, true)
}

fn visible(raw: &str, complete: bool) -> String {
    const PREFIX: &str = "<|ACT:";
    let mut visible = String::new();
    let mut rest = raw;
    while let Some(start) = rest.find(PREFIX) {
        visible.push_str(&rest[..start]);
        rest = &rest[start..];
        let Some(end) = rest.find("|>") else {
            if rest.len() > 4096 {
                visible.push_str(rest);
            }
            return visible;
        };
        let control = &rest[PREFIX.len()..end];
        let structured = control.len() <= 4096
            && serde_json::from_str::<serde_json::Value>(control)
                .is_ok_and(|value| value.is_object());
        if !structured {
            visible.push_str(&rest[..end + 2]);
        }
        rest = &rest[end + 2..];
    }
    let held = (1..PREFIX.len())
        .rev()
        .find(|count| {
            if complete && *count == 1 {
                return false;
            }
            *count <= rest.len()
                && rest.is_char_boundary(rest.len() - count)
                && PREFIX.starts_with(&rest[rest.len() - count..])
        })
        .unwrap_or(0);
    visible.push_str(&rest[..rest.len() - held]);
    visible
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn streamed_controls_never_appear_between_split_characters() {
        let tag = r#"<|ACT:{"emotion":"gentle"}|>"#;
        let mut raw = "晚安".to_string();
        for character in tag.chars() {
            raw.push(character);
            assert_eq!(albion_visible(&raw), "晚安");
        }
        raw.push_str("，老公。");
        assert_eq!(albion_visible(&raw), "晚安，老公。");
    }
    #[test]
    fn ordinary_markup_and_invalid_control_examples_remain_literal() {
        assert_eq!(
            albion_visible("<b>中文</b> & <|ACT:example|>"),
            "<b>中文</b> & <|ACT:example|>"
        );
        assert_eq!(albion_visible("正文<|AC"), "正文");
        assert_eq!(albion_visible("中文"), "中文");
        assert_eq!(albion_complete("比较符号 <"), "比较符号 <");
        assert_eq!(albion_complete("晚安<|AC"), "晚安");
    }
}
