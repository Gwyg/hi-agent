//! 状态栏组件:左 cwd · mode,右 版本号

use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::Style,
    widgets::Paragraph,
};

use crate::tui::text::display_cwd;
use crate::tui::theme::{BG, HINT_GRAY};
use crate::tui::{App, Mode, Role};

/// 状态栏:左 cwd · mode,右 版本号
pub(crate) fn render_status_bar(f: &mut Frame, app: &App, area: Rect) {
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

/// 状态栏模式:Thinking 且有 tool_calls 时显示"工具调用中"
fn mode_str(app: &App) -> String {
    match app.mode {
        Mode::Input => "Input".to_string(),
        Mode::Thinking => {
            if let Some(last) = app.messages.last() {
                if last.role == Role::Assistant && last.has_tool() {
                    return "工具调用中".to_string();
                }
            }
            "Thinking".to_string()
        }
        Mode::Quit => "Quit".to_string(),
    }
}
