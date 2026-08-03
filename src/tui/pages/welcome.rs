//! 欢迎页:空会话时的居中视觉组(Logo + 提示 + 快捷键 + 输入框 + Tip + 状态栏)

use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph},
};

use crate::tui::components::input::render_input;
use crate::tui::components::status_bar::render_status_bar;
use crate::tui::text::input_content_rows;
use crate::tui::theme::{ACCENT, ACCENT_DARK, AMBER, BG, HINT_GRAY, LOGO};
use crate::tui::App;

/// 空会话:居中视觉组(Logo + 提示 + 快捷键 + 输入框 + Tip),状态栏在最底部
///
/// 上部从 Logo 往下排,下部输入框/Tip/状态栏从底部往上排,中间留白。
pub(crate) fn draw_welcome(f: &mut Frame, app: &mut App) {
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
            .style(Style::default().fg(HINT_GRAY).add_modifier(Modifier::ITALIC))
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
    let input_w = (area.width as u32 * 70 / 100) as u16;
    let input_x = area.x + area.width.saturating_sub(input_w) / 2;
    // 内容区行数随文本动态增长(wrap_width = 内容区宽 = input_w - 左竖线1列)
    let content_rows = input_content_rows(&app.buffer, input_w.saturating_sub(1) as usize);
    let input_h = content_rows as u16 + 2; // + Build + 半行
    let input_y = tip_y.saturating_sub(input_h);
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
        Paragraph::new(Line::from(vec![
            Span::styled(
                "Tip",
                Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " 用自然语言描述你的需求，我会帮你完成。",
                Style::default().fg(HINT_GRAY),
            ),
        ]))
        .alignment(Alignment::Center)
        .style(Style::default().bg(BG)),
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
