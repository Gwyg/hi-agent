//! 联网工具共用的文本裁剪：按字节预算切到完整的 UTF-8 字符边界。
//!
//! 工具结果会原样进历史、每轮重发，所以预算按字节算（决定体积与 token），
//! 但中文一个字符 3 字节，直接 `&s[..max]` 会在多字节字符中间 panic。

/// 返回 `(保留部分, 是否发生截断)`。`max` 是字节数，不是字符数。
pub(crate) fn truncate_utf8(s: &str, max: usize) -> (&str, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (&s[..end], true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_whole_string_when_within_budget() {
        assert_eq!(truncate_utf8("abc", 3), ("abc", false));
        assert_eq!(truncate_utf8("abc", 10), ("abc", false));
    }

    /// 中文 3 字节/字:预算 10 落在第 4 个字中间,必须退到 9(整 3 个字)
    #[test]
    fn cuts_back_to_char_boundary() {
        let s = "中".repeat(10);
        let (kept, cut) = truncate_utf8(&s, 10);
        assert!(cut);
        assert_eq!(kept.len(), 9);
        assert_eq!(kept, "中".repeat(3));
    }

    #[test]
    fn exact_boundary_is_not_marked_as_cut() {
        let s = "中".repeat(3);
        let (kept, cut) = truncate_utf8(&s, 9);
        assert!(!cut);
        assert_eq!(kept, "中".repeat(3));
    }
}
