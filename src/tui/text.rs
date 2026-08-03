//! 文本/光标算法:软换行、视觉行、光标坐标映射、滚动;以及展示用小工具

use super::Cursor;
use unicode_width::UnicodeWidthChar;

/// 字符在输入框中的显示宽度(宽字符计 2)
fn char_cells(ch: char) -> usize {
    ch.width().unwrap_or(0)
}

/// 软换行:把 Buffer 的每行按显示宽度折成视觉行(Buffer 本身不拆,只拆视觉行)
pub(crate) fn visual_lines(buffer: &[String], width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for line in buffer {
        out.extend(wrap_line(line, width));
    }
    out
}

/// 输入内容区行数:首末各留 1 行空白,中间放文本;文本每多一行框长高一行,封顶 8 行(超出文本区内部滚动)
pub(crate) fn input_content_rows(buffer: &[String], wrap_width: usize) -> usize {
    (visual_lines(buffer, wrap_width).len() + 2).clamp(3, 8)
}

/// 按显示宽度折行(宽字符计 2,不切割字符)
pub(crate) fn wrap_line(line: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for ch in line.chars() {
        let w = char_cells(ch);
        if cur_w + w > width && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        cur.push(ch);
        cur_w += w;
    }
    out.push(cur);
    out
}

/// 光标在视觉行中的位置:返回 (视觉行下标, 该行内的字符序号)
///
/// 折行边界(column 恰在段末且非行尾)归到下一视觉行行首,保证 ←→ 单步跨行平滑。
pub(crate) fn cursor_visual_pos(buffer: &[String], cursor: Cursor, width: usize) -> (usize, usize) {
    let mut vi = 0usize;
    for (li, line) in buffer.iter().enumerate() {
        let segs = wrap_line(line, width);
        if li == cursor.line {
            let mut before = 0usize;
            for (si, seg) in segs.iter().enumerate() {
                let seg_chars = seg.chars().count();
                if cursor.column < before + seg_chars {
                    return (vi + si, cursor.column.saturating_sub(before));
                }
                before += seg_chars;
            }
            // 行尾:归到最后一个视觉行末尾
            let tail = segs.last().map_or(0, |s| s.chars().count());
            return (vi + segs.len().saturating_sub(1), tail);
        }
        vi += segs.len();
    }
    (vi, 0)
}

/// 视觉行内,光标前字符的显示宽度(用于水平定位光标)
pub(crate) fn visual_x_of(seg: &str, vcol: usize) -> usize {
    seg.chars().take(vcol).map(char_cells).sum()
}

/// 视觉行下标 + 显示宽度列 → buffer 位置 (line, column)
///
/// 供 ↑↓ 跨视觉行移动:在目标视觉行内按显示宽度 x 反查字符列,
/// 目标行更短时落在行尾。
pub(crate) fn visual_to_buffer(buffer: &[String], width: usize, vi: usize, x: usize) -> (usize, usize) {
    let mut acc = 0usize;
    for (li, line) in buffer.iter().enumerate() {
        let segs = wrap_line(line, width);
        if vi < acc + segs.len() {
            let seg_idx = vi - acc;
            let chars_before: usize = segs[..seg_idx].iter().map(|s| s.chars().count()).sum();
            let seg = &segs[seg_idx];
            let mut col_in_seg = 0usize;
            let mut w = 0usize;
            for ch in seg.chars() {
                let cw = char_cells(ch);
                if w + cw > x {
                    break;
                }
                w += cw;
                col_in_seg += 1;
            }
            return (li, chars_before + col_in_seg);
        }
        acc += segs.len();
    }
    let last = buffer.len().saturating_sub(1);
    (last, buffer[last].chars().count())
}

/// 根据光标所在视觉行计算滚动偏移,保证光标在可视区域内
pub(crate) fn compute_scroll(cursor_vi: usize, visible_rows: usize, total: usize) -> usize {
    let max_scroll = total.saturating_sub(visible_rows);
    let mut scroll = 0;
    if cursor_vi >= scroll + visible_rows {
        scroll = cursor_vi + 1 - visible_rows;
    }
    if cursor_vi < scroll {
        scroll = cursor_vi;
    }
    scroll.min(max_scroll)
}

/// 显示用 cwd:去掉 Windows verbatim 路径前缀 \\?\,保留盘符开头
pub(crate) fn display_cwd(cwd: &str) -> String {
    cwd.strip_prefix(r"\\?\").unwrap_or(cwd).to_string()
}

/// 模型名映射为展示用大小写(不直接显示 .env 原样)
pub(crate) fn display_model_name(raw: &str) -> String {
    match raw.to_lowercase().as_str() {
        "deepseek-v4-pro" | "deepseek-v4" => "DeepSeek V4 Pro".to_string(),
        "deepseek-v4-flash" => "DeepSeek V4 Flash".to_string(),
        "gpt-4" => "GPT-4".to_string(),
        _ => raw.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::App;

    fn cur(line: usize, column: usize) -> Cursor {
        Cursor { line, column }
    }

    #[test]
    fn wrap_line_basic() {
        assert_eq!(wrap_line("abcdef", 3), vec!["abc", "def"]);
        assert_eq!(wrap_line("abc", 3), vec!["abc"]);
        assert_eq!(wrap_line("", 3), vec![""]);
        assert_eq!(wrap_line("a中b", 3), vec!["a中", "b"]);
    }

    #[test]
    fn visual_pos_and_boundary() {
        let buffer = vec![
            "aaaa".to_string(),
            "bbbbbbbbb".to_string(), // 宽5 → ["bbbbb","bbbb"]
            "cc".to_string(),
        ];
        // visual = ["aaaa","bbbbb","bbbb","cc"]
        assert_eq!(cursor_visual_pos(&buffer, cur(1, 0), 5), (1, 0));
        // 折行边界归到下一视觉行行首
        assert_eq!(cursor_visual_pos(&buffer, cur(1, 5), 5), (2, 0));
        // 行尾归到最后一个视觉行末尾
        assert_eq!(cursor_visual_pos(&buffer, cur(1, 9), 5), (2, 4));
        assert_eq!(cursor_visual_pos(&buffer, cur(2, 1), 5), (3, 1));
    }

    #[test]
    fn visual_to_buffer_roundtrip() {
        let buffer = vec![
            "aaaa".to_string(),
            "bbbbbbbbb".to_string(),
            "cc".to_string(),
        ];
        // visual = ["aaaa","bbbbb","bbbb","cc"]
        assert_eq!(visual_to_buffer(&buffer, 5, 1, 0), (1, 0));
        assert_eq!(visual_to_buffer(&buffer, 5, 1, 5), (1, 5));
        assert_eq!(visual_to_buffer(&buffer, 5, 2, 4), (1, 9));
        assert_eq!(visual_to_buffer(&buffer, 5, 3, 1), (2, 1));
        // x 超过行宽 → 落在行尾
        assert_eq!(visual_to_buffer(&buffer, 5, 1, 99), (1, 5));
    }

    #[test]
    fn scroll_keeps_cursor_visible() {
        for total in 0..10usize {
            for visible in 1..6usize {
                for cursor_vi in 0..total.max(1) {
                    if cursor_vi >= total && total > 0 {
                        continue;
                    }
                    let scroll = compute_scroll(cursor_vi, visible, total);
                    assert!(scroll <= total.saturating_sub(visible));
                    assert!(cursor_vi >= scroll, "cur={cursor_vi} scroll={scroll}");
                    if total > 0 {
                        assert!(cursor_vi - scroll < visible);
                    }
                }
            }
        }
    }

    #[test]
    fn up_down_keeps_visual_column() {
        // 长行(2 视觉行)上移到短行:列按显示宽度收敛到短行行尾
        let mut app = App::new("m".into(), "cwd".into());
        app.input_width = 5;
        app.buffer = vec!["ab".to_string(), "bbbbbbbbb".to_string()];
        app.cursor = cur(1, 9); // 长行行尾 → 视觉行 (2,4)
        app.cursor_up();
        // 上一视觉行(1)在长行内,列保持 x=4
        assert_eq!((app.cursor.line, app.cursor.column), (1, 4));
        app.cursor_up();
        // 再上一视觉行是短行 "ab"(视觉行0),x=4 超宽 → 行尾 col2
        assert_eq!((app.cursor.line, app.cursor.column), (0, 2));
    }
}
