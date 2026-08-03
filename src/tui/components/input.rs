//! 输入框组件:左竖线 + 内容区(留白/文本/Build/半行) + 块状光标 + placeholder

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Paragraph},
};

use crate::tui::text::{compute_scroll, cursor_visual_pos, display_model_name, visual_lines};
use crate::tui::theme::{ACCENT, ACCENT_DARK, BG, HINT_GRAY, INPUT_BG, PRIMARY};
use crate::tui::App;

/// 输入框:左竖线 + 内容区(留白/文本/Build/半行) + 块状光标 + placeholder
pub(crate) fn render_input(f: &mut Frame, app: &mut App, input_area: Rect) {
    let input_w = input_area.width;
    // 左侧青色竖线:满高行用半宽块 ▌,末行(半行)用左上象限 ▘ 与内容半块下沿对齐
    {
        let buf = f.buffer_mut();
        let last = input_area.height.saturating_sub(1);
        for r in 0..input_area.height {
            let (glyph, style) = if r == last {
                ("▘", Style::default().fg(ACCENT_DARK).bg(BG))
            } else {
                ("▌", Style::default().fg(ACCENT_DARK))
            };
            let _ = buf.set_string(input_area.x, input_area.y + r, glyph, style);
        }
    }
    let content_area = Rect {
        x: input_area.x + 1,
        y: input_area.y,
        width: input_w.saturating_sub(1),
        height: input_area.height,
    };
    f.render_widget(
        Block::default().style(Style::default().bg(INPUT_BG)),
        content_area,
    );
    // 内容区行数 = 输入框高度 - Build(1) - 半行(1),由调用方按文本动态给定
    let content_rows_u16 = content_area.height.saturating_sub(2);
    let inner = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(content_rows_u16), // 输入内容
            Constraint::Length(1),                // Build
            Constraint::Length(1),                // 半行(▀ 上半=框色 下半=背景)
        ])
        .split(content_area);

    let wrap_width = inner[0].width as usize;
    app.input_width = wrap_width;
    let visual = visual_lines(&app.buffer, wrap_width);
    let (cur_vi, cur_vcol) = cursor_visual_pos(&app.buffer, app.cursor, wrap_width);
    let content_rows = inner[0].height as usize;
    // 首行(row 0)、末行永远留白;文本放在中间 text_rows 行,超出则文本区内部滚动
    let text_rows = content_rows.saturating_sub(2);
    let scroll = compute_scroll(cur_vi, text_rows, visual.len());

    let show_placeholder = app.is_empty() && app.messages.is_empty();
    let placeholder_row = 1; // 文本首行(首行留白之后)
    let placeholder = "说点什么... \"这个项目用了什么技术栈？\"";
    let mut lines: Vec<Line> = Vec::new();
    for row in 0..content_rows {
        // row 0 与末行永远留白;文本行下标从 row 1 起
        let is_pad = row == 0 || row + 1 == content_rows;
        let vi = scroll + row.saturating_sub(1);
        let in_text = !is_pad && vi < visual.len();
        if show_placeholder && row == placeholder_row {
            // 块状光标:高亮首格(不新增列),灭时为普通空格
            let cursor_style = if app.cursor_visible {
                Style::default().fg(INPUT_BG).bg(ACCENT)
            } else {
                Style::default().bg(INPUT_BG)
            };
            lines.push(Line::from(vec![
                Span::styled(" ", cursor_style),
                Span::styled(
                    placeholder,
                    Style::default()
                        .fg(HINT_GRAY)
                        .bg(INPUT_BG)
                        .add_modifier(Modifier::ITALIC),
                ),
            ]));
        } else if show_placeholder {
            lines.push(Line::from(""));
        } else if in_text {
            let seg = &visual[vi];
            let cursor_at = if vi == cur_vi { Some(cur_vcol) } else { None };
            lines.push(render_seg_line(seg, cursor_at, app.cursor_visible));
        } else if !is_pad && vi == cur_vi {
            // 光标停在文本末尾之外的空视觉行(如尾部换行):高亮一个空格
            lines.push(render_seg_line("", Some(0), app.cursor_visible));
        } else {
            lines.push(Line::from(""));
        }
    }
    f.render_widget(Paragraph::new(Text::from(lines)), inner[0]);

    // Build 行
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Build", Style::default().fg(ACCENT_DARK).bg(INPUT_BG)),
            Span::styled(
                format!(" · {}", display_model_name(&app.model_name)),
                Style::default()
                    .fg(PRIMARY)
                    .bg(INPUT_BG)
                    .add_modifier(Modifier::BOLD),
            ),
        ])),
        inner[1],
    );

    // 半行:▀ 上半=输入框底色,下半=页面背景 → 视觉上像半行留白
    let half = "▀".repeat(inner[2].width as usize);
    f.render_widget(
        Paragraph::new(Line::styled(half, Style::default().fg(INPUT_BG).bg(BG))),
        inner[2],
    );
}

/// 把视觉行渲染为逐字符 Span:cursor_at = 该行内光标字符序号(块状反色高亮),
/// 越过行尾则在末尾补高亮空格。
fn render_seg_line(seg: &str, cursor_at: Option<usize>, cursor_visible: bool) -> Line<'static> {
    let normal = Style::default().fg(PRIMARY).bg(INPUT_BG);
    let cursor_style = if cursor_visible {
        Style::default().fg(INPUT_BG).bg(ACCENT)
    } else {
        normal
    };
    let mut spans: Vec<Span> = Vec::new();
    let mut n = 0usize;
    for (i, ch) in seg.chars().enumerate() {
        n = i + 1;
        let style = if cursor_at == Some(i) { cursor_style } else { normal };
        spans.push(Span::styled(ch.to_string(), style));
    }
    if let Some(idx) = cursor_at {
        if idx >= n {
            spans.push(Span::styled(" ".to_string(), cursor_style));
        }
    }
    Line::from(spans)
}
