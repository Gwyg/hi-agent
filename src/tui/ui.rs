use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Paragraph},
};

use super::{App, Cursor, Mode, MsgStatus, Role, ToolCallInfo, ToolStatus};
use unicode_width::UnicodeWidthChar;

/// spinner 6 帧旋转(参考 Codex 风格)
const SPINNER: [&str; 6] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴"];

/// 空会话 Logo:方块字母 "hi"
const LOGO: &[&str] = &["█  █   █", "█  █   █", "████   █", "█  █   █", "█  █   █"];

/// 背景:纯黑
const BG: Color = Color::Rgb(0x0a, 0x0a, 0x0a);
/// 输入盒底色
const INPUT_BG: Color = Color::Rgb(0x1a, 0x1a, 0x1a);
/// 主文字(输入内容)
const PRIMARY: Color = Color::Rgb(0xe5, 0xe7, 0xeb);
/// 强调青(Header / 角色标签 / 光标)
const ACCENT: Color = Color::Rgb(0x22, 0xd3, 0xee);
/// 深青(左竖线 / Logo 底部 / Build 标签)
const ACCENT_DARK: Color = Color::Rgb(0x06, 0xb6, 0xd4);
/// 辅助文字(placeholder / 快捷键 / 状态栏)
const HINT_GRAY: Color = Color::Rgb(0x6b, 0x72, 0x80);
/// 琥珀(工具 Running)
const AMBER: Color = Color::Rgb(0xf5, 0x9e, 0x0b);
/// 红(错误 / 工具 Error)
const RED: Color = Color::Rgb(0xef, 0x44, 0x44);

pub fn draw(f: &mut Frame, app: &mut App) {
    if app.messages.is_empty() {
        draw_welcome(f, app);
    } else {
        draw_chat(f, app);
    }
}

/// 空会话:居中视觉组(Logo + 提示 + 快捷键 + 输入框 + Tip),状态栏在最底部
///
/// 上部从 Logo 往下排,下部输入框/Tip/状态栏从底部往上排,中间留白。
fn draw_welcome(f: &mut Frame, app: &mut App) {
    let area = f.area();
    f.render_widget(Block::default().style(Style::default().bg(BG)), area);

    let h = area.height;
    // Logo:垂直中心偏上
    let logo_lines = scale_logo(LOGO, 2, 1.5);
    let logo_h = logo_lines.len() as u16;
    let logo_w = logo_lines[0].chars().count() as u16;
    let logo_x = area.x + area.width.saturating_sub(logo_w) / 2;
    let logo_y = area.y + (h / 2).saturating_sub(8);
    {
        let buf = f.buffer_mut();
        let dark_start = logo_lines.len() * 3 / 4;
        for (i, line) in logo_lines.iter().enumerate() {
            let color = if i >= dark_start { ACCENT_DARK } else { ACCENT };
            let _ = buf.set_string(logo_x, logo_y + i as u16, line, Style::default().fg(color));
        }
    }
    // 提示文字(Logo 下 2 行)
    let hint_y = logo_y + logo_h + 2;
    f.render_widget(
        Paragraph::new("还没有消息，开始对话吧")
            .style(
                Style::default()
                    .fg(HINT_GRAY)
                    .add_modifier(Modifier::ITALIC),
            )
            .alignment(Alignment::Center),
        Rect {
            x: area.x,
            y: hint_y,
            width: area.width,
            height: 1,
        },
    );
    // 快捷键(提示下 1 行)
    let key_y = hint_y + 1;
    f.render_widget(
        Paragraph::new("Enter 提交 · Shift+Enter 换行 · Ctrl+C 退出")
            .style(Style::default().fg(HINT_GRAY))
            .alignment(Alignment::Center),
        Rect {
            x: area.x,
            y: key_y,
            width: area.width,
            height: 1,
        },
    );

    // 输入框/Tip/状态栏从底部往上排
    let status_y = area.y + h.saturating_sub(1);
    let tip_y = status_y.saturating_sub(1);
    let input_h = 6u16;
    let input_y = tip_y.saturating_sub(input_h);
    let input_w = (area.width as u32 * 70 / 100) as u16;
    let input_x = area.x + area.width.saturating_sub(input_w) / 2;
    render_input(
        f,
        app,
        Rect {
            x: input_x,
            y: input_y,
            width: input_w,
            height: input_h,
        },
    );

    // Tip
    f.render_widget(
        Paragraph::new(" Tip 用自然语言描述你的需求，我会帮你完成。")
            .style(Style::default().fg(HINT_GRAY).bg(BG)),
        Rect {
            x: area.x,
            y: tip_y,
            width: area.width,
            height: 1,
        },
    );
    // 状态栏
    render_status_bar(
        f,
        app,
        Rect {
            x: area.x,
            y: status_y,
            width: area.width,
            height: 1,
        },
    );
}

/// 有消息时:Header(1) → 消息列表(Min) → 输入框(6) → Tip(1) → 状态栏(1)
fn draw_chat(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Header
            Constraint::Min(1),    // 消息列表
            Constraint::Length(6), // 输入框
            Constraint::Length(1), // Tip
            Constraint::Length(1), // 状态栏
        ])
        .split(f.area());

    // Header:hi · model
    f.render_widget(
        Paragraph::new(format!(" hi · {}", display_model_name(&app.model_name)))
            .style(Style::default().fg(ACCENT).bg(BG)),
        chunks[0],
    );

    // 消息列表:角色标签 + 内容(保留换行/代码块) + 工具卡片 + 空行间距
    let msg_area = chunks[1];
    let wrap_width = msg_area.width.saturating_sub(2) as usize;
    let mut all_lines: Vec<Line> = Vec::new();
    for msg in &app.messages {
        all_lines.push(role_label(&msg.role));
        if msg.role == Role::Assistant
            && msg.content.is_empty()
            && msg.status == MsgStatus::Thinking
        {
            let spinner = SPINNER[app.frame as usize];
            let elapsed = app
                .thinking_since
                .map(|t| format!("{:.1}s", t.elapsed().as_secs_f32()))
                .unwrap_or_default();
            all_lines.push(Line::styled(
                format!("  {spinner} Thinking… {elapsed}"),
                Style::default()
                    .fg(HINT_GRAY)
                    .add_modifier(Modifier::ITALIC),
            ));
        } else {
            // 保留换行:split('\n') 每行独立折行;代码块(``` 包裹)加 │ 前缀和边框
            let mut in_code = false;
            for raw in msg.content.split('\n') {
                if raw.trim_start().starts_with("```") {
                    in_code = !in_code;
                    if in_code {
                        all_lines.push(Line::styled(
                            "  ┌─".to_string(),
                            Style::default().fg(HINT_GRAY),
                        ));
                    } else {
                        all_lines.push(Line::styled(
                            "  └────".to_string(),
                            Style::default().fg(HINT_GRAY),
                        ));
                    }
                    continue;
                }
                if in_code {
                    for l in wrap_line(&format!("  │ {raw}"), wrap_width) {
                        all_lines.push(Line::styled(l, Style::default().fg(PRIMARY)));
                    }
                } else {
                    for l in wrap_line(raw, wrap_width) {
                        all_lines
                            .push(Line::styled(format!("  {l}"), Style::default().fg(PRIMARY)));
                    }
                }
            }
        }
        // 工具卡片:内联在消息内容下方
        for tc in &msg.tool_calls {
            all_lines.push(tool_card_line(tc, app.frame));
        }
        all_lines.push(Line::raw("")); // 消息间距
    }

    // 智能滚动:auto_scroll 贴底;手动上翻后接近底部(≤3 行)恢复跟随
    let visible_rows = msg_area.height as usize;
    let total = all_lines.len();
    let max_scroll = total.saturating_sub(visible_rows);
    if app.auto_scroll {
        app.scroll_offset = max_scroll;
    } else if app.scroll_offset >= max_scroll.saturating_sub(3) {
        app.auto_scroll = true;
        app.scroll_offset = max_scroll;
    }
    let scroll = app.scroll_offset.min(max_scroll);
    let visible: Vec<Line> = all_lines
        .into_iter()
        .skip(scroll)
        .take(visible_rows)
        .collect();
    f.render_widget(
        Paragraph::new(Text::from(visible)).style(Style::default().bg(BG)),
        msg_area,
    );

    // 输入框
    render_input(f, app, chunks[2]);
    // Tip
    f.render_widget(
        Paragraph::new(" Tip 用自然语言描述你的需求，我会帮你完成。")
            .style(Style::default().fg(HINT_GRAY).bg(BG)),
        chunks[3],
    );
    // 状态栏
    render_status_bar(f, app, chunks[4]);
}

/// 输入框:左竖线 + 内容区(padding/内容/空/Build) + ▎ 闪烁光标 + placeholder
fn render_input(f: &mut Frame, app: &mut App, input_area: Rect) {
    let input_w = input_area.width;
    // 左侧青色竖线
    {
        let buf = f.buffer_mut();
        let bar = Style::default().fg(ACCENT_DARK);
        for r in 0..input_area.height {
            let _ = buf.set_string(input_area.x, input_area.y + r, "▎", bar);
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
    let inner = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // padding
            Constraint::Length(3), // 输入内容
            Constraint::Length(1), // 空行撑开
            Constraint::Length(1), // Build
        ])
        .split(content_area);

    let wrap_width = inner[1].width as usize;
    app.input_width = wrap_width;
    let visual = visual_lines(&app.buffer, wrap_width);
    let (cur_vi, cur_vcol) = cursor_visual_pos(&app.buffer, app.cursor, wrap_width);
    let content_rows = inner[1].height as usize;
    let scroll = compute_scroll(cur_vi, content_rows, visual.len());

    let show_placeholder = app.is_empty() && app.messages.is_empty();
    let placeholder = "说点什么... \"这个项目用了什么技术栈？\"";
    let mut lines: Vec<Line> = Vec::new();
    for row in 0..content_rows {
        let vi = scroll + row;
        if show_placeholder && row == 0 {
            if app.cursor_visible {
                lines.push(Line::from(vec![
                    Span::styled("▎", Style::default().fg(ACCENT).bg(INPUT_BG)),
                    Span::styled(
                        placeholder,
                        Style::default()
                            .fg(HINT_GRAY)
                            .bg(INPUT_BG)
                            .add_modifier(Modifier::ITALIC),
                    ),
                ]));
            } else {
                lines.push(Line::styled(
                    placeholder,
                    Style::default()
                        .fg(HINT_GRAY)
                        .bg(INPUT_BG)
                        .add_modifier(Modifier::ITALIC),
                ));
            }
        } else if vi < visual.len() {
            let seg = &visual[vi];
            if app.cursor_visible && vi == cur_vi {
                let before: String = seg.chars().take(cur_vcol).collect();
                let after: String = seg.chars().skip(cur_vcol).collect();
                lines.push(Line::from(vec![
                    Span::styled(before, Style::default().fg(PRIMARY).bg(INPUT_BG)),
                    Span::styled("▎", Style::default().fg(ACCENT).bg(INPUT_BG)),
                    Span::styled(after, Style::default().fg(PRIMARY).bg(INPUT_BG)),
                ]));
            } else {
                lines.push(Line::styled(
                    seg.clone(),
                    Style::default().fg(PRIMARY).bg(INPUT_BG),
                ));
            }
        } else if app.cursor_visible && vi == cur_vi {
            lines.push(Line::styled(
                "▎".to_string(),
                Style::default().fg(ACCENT).bg(INPUT_BG),
            ));
        } else {
            lines.push(Line::from(""));
        }
    }
    f.render_widget(Paragraph::new(Text::from(lines)), inner[1]);

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
        inner[3],
    );
}

/// 状态栏:左 cwd · mode,右 版本号
fn render_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let half = area.width / 2;
    f.render_widget(
        Paragraph::new(format!(" {} · {}", display_cwd(&app.cwd), mode_str(app)))
            .style(Style::default().fg(HINT_GRAY).bg(BG)),
        Rect {
            x: area.x,
            y: area.y,
            width: half,
            height: 1,
        },
    );
    f.render_widget(
        Paragraph::new(format!(" {}", env!("CARGO_PKG_VERSION")))
            .style(Style::default().fg(HINT_GRAY).bg(BG))
            .alignment(Alignment::Right),
        Rect {
            x: area.x + half,
            y: area.y,
            width: area.width - half,
            height: 1,
        },
    );
}

/// 工具调用卡片:  │ {图标} {工具名} {目标} [{状态}]
fn tool_card_line(tc: &ToolCallInfo, frame: u8) -> Line<'static> {
    let icon = match tc.name.as_str() {
        "edit" | "write" => "✏️",
        "read" => "👁",
        "bash" => "$",
        _ => "●",
    };
    let target = extract_target(&tc.args);
    let (status_text, status_color) = match tc.status {
        ToolStatus::Running => {
            let spinner = SPINNER[frame as usize];
            (format!("{spinner} Running"), AMBER)
        }
        ToolStatus::Done => ("Done".to_string(), HINT_GRAY),
        ToolStatus::Error => ("Error".to_string(), RED),
    };
    Line::from(vec![
        Span::styled(
            format!("  │ {icon} {} ", tc.name),
            Style::default().fg(PRIMARY),
        ),
        Span::styled(target, Style::default().fg(HINT_GRAY)),
        Span::styled(
            format!(" [{status_text}]"),
            Style::default().fg(status_color),
        ),
    ])
}

/// 从 tool args(JSON)提取目标:path / file_path / command,失败返回原样
fn extract_target(args: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(args) {
        for key in ["path", "file_path", "command"] {
            if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
                return s.to_string();
            }
        }
    }
    args.to_string()
}

/// 状态栏模式:Thinking 且有 tool_calls 时显示"工具调用中"
fn mode_str(app: &App) -> String {
    match app.mode {
        Mode::Input => "Input".to_string(),
        Mode::Thinking => {
            if let Some(last) = app.messages.last() {
                if last.role == Role::Assistant && !last.tool_calls.is_empty() {
                    return "工具调用中".to_string();
                }
            }
            "Thinking".to_string()
        }
        Mode::Quit => "Quit".to_string(),
    }
}

/// 显示用 cwd:去掉 Windows verbatim 路径前缀 \\?\,保留盘符开头
fn display_cwd(cwd: &str) -> String {
    cwd.strip_prefix(r"\\?\").unwrap_or(cwd).to_string()
}

/// 模型名映射为展示用大小写(不直接显示 .env 原样)
fn display_model_name(raw: &str) -> String {
    match raw.to_lowercase().as_str() {
        "deepseek-v4-pro" | "deepseek-v4" => "DeepSeek V4 Pro".to_string(),
        "deepseek-v4-flash" => "DeepSeek V4 Flash".to_string(),
        "gpt-4" => "GPT-4".to_string(),
        _ => raw.to_string(),
    }
}

/// 将 logo 横向 ×fx、纵向 ×fy(fy 可为小数,每行重复次数累进取整)放大
fn scale_logo(base: &[&str], fx: u16, fy: f32) -> Vec<String> {
    let mut out = Vec::new();
    for (i, line) in base.iter().enumerate() {
        let mut wide = String::with_capacity(line.chars().count() * fx as usize);
        for c in line.chars() {
            for _ in 0..fx {
                wide.push(c);
            }
        }
        let prev = (i as f32 * fy).round() as usize;
        let next = ((i + 1) as f32 * fy).round() as usize;
        for _ in prev..next {
            out.push(wide.clone());
        }
    }
    out
}

fn role_label(role: &Role) -> Line<'static> {
    match role {
        Role::User => Line::styled(
            "▸ 你".to_string(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Role::Assistant => Line::styled(
            "◇ hi".to_string(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Role::System => Line::styled("● system".to_string(), Style::default().fg(HINT_GRAY)),
    }
}

/// 软换行:把 Buffer 的每行按显示宽度折成视觉行(Buffer 本身不拆,只拆视觉行)
pub(super) fn visual_lines(buffer: &[String], width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for line in buffer {
        out.extend(wrap_line(line, width));
    }
    out
}

/// 按显示宽度折行(宽字符计 2,不切割字符)
pub(super) fn wrap_line(line: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for ch in line.chars() {
        let w = ch.width().unwrap_or(0);
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
pub(super) fn cursor_visual_pos(buffer: &[String], cursor: Cursor, width: usize) -> (usize, usize) {
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
pub(super) fn visual_x_of(seg: &str, vcol: usize) -> usize {
    seg.chars().take(vcol).map(|c| c.width().unwrap_or(0)).sum()
}

/// 视觉行下标 + 显示宽度列 → buffer 位置 (line, column)
///
/// 供 ↑↓ 跨视觉行移动:在目标视觉行内按显示宽度 x 反查字符列,
/// 目标行更短时落在行尾。
pub(super) fn visual_to_buffer(
    buffer: &[String],
    width: usize,
    vi: usize,
    x: usize,
) -> (usize, usize) {
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
                let cw = ch.width().unwrap_or(0);
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
fn compute_scroll(cursor_vi: usize, visible_rows: usize, total: usize) -> usize {
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

#[cfg(test)]
mod tests {
    use super::*;

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
