//! 工具调用卡片组件

use ratatui::{
    style::Style,
    text::{Line, Span},
};

use crate::tui::theme::{AMBER, HINT_GRAY, PRIMARY, RED, SPINNER};
use crate::tui::{ToolCallInfo, ToolStatus};

/// 工具调用卡片:  │ {图标} {工具名} {目标} [{状态}]
pub(crate) fn tool_card_line(tc: &ToolCallInfo, frame: u8) -> Line<'static> {
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
        Span::styled(format!(" [{status_text}]"), Style::default().fg(status_color)),
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
