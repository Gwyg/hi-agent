//! 对话页:有消息时 Header → 消息列表 → 输入框 → Tip → 状态栏

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::Paragraph,
};

use crate::tui::components::input::render_input;
use crate::tui::components::status_bar::render_status_bar;
use crate::tui::components::tool_card::tool_card_line;
use crate::tui::text::{display_model_name, input_content_rows, wrap_line};
use crate::tui::theme::{ACCENT, AMBER, BG, HINT_GRAY, PRIMARY, SPINNER, USER};
use crate::tui::{App, Block, Role};

/// 有消息时:Header(1) → 消息列表(Min) → 输入框(动态) → Tip(1) → 状态栏(1)
pub(crate) fn draw_chat(f: &mut Frame, app: &mut App) {
    // 输入框高度随文本动态增长(wrap_width = 全宽 - 左竖线1列)
    let content_rows = input_content_rows(&app.buffer, f.area().width.saturating_sub(1) as usize);
    let input_h = content_rows as u16 + 2; // + Build + 半行
    // 待确认工具时,输入框上方占 1 行确认条;否则 0 行(不影响布局)
    let ask_h = if app.pending_ask.is_some() { 1 } else { 0 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),       // Header
            Constraint::Min(1),          // 消息列表
            Constraint::Length(ask_h),   // 确认条(pending_ask 为 None 时高度 0)
            Constraint::Length(input_h), // 输入框(随文本动态增高)
            Constraint::Length(1),       // Tip
            Constraint::Length(1),       // 状态栏
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
    let last_idx = app.messages.len().saturating_sub(1);
    for (i, msg) in app.messages.iter().enumerate() {
        all_lines.push(role_label(&msg.role));
        // 按到达顺序遍历 blocks:文本段与工具卡片交错呈现,保留时序
        for block in &msg.blocks {
            match block {
                Block::Text(content) => {
                    // 保留换行:split('\n') 每行独立折行;代码块(``` 包裹)加 │ 前缀和边框
                    let mut in_code = false;
                    for raw in content.split('\n') {
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
                                all_lines.push(Line::styled(
                                    format!("  {l}"),
                                    Style::default().fg(PRIMARY),
                                ));
                            }
                        }
                    }
                }
                Block::Tool(tc) => {
                    all_lines.push(tool_card_line(tc, app.frame));
                }
            }
        }
        // Thinking:仅最后一条 assistant、本轮在等模型(awaiting_model)、且无 Running 工具时,
        // 末尾补一行 spinner(有工具在跑则由工具卡片自己转,不重复)
        if i == last_idx
            && msg.role == Role::Assistant
            && msg.awaiting_model
            && !msg.has_running_tool()
        {
            let spinner = SPINNER[app.frame as usize];
            let elapsed = app
                .thinking_since
                .map(|t| format!("{:.1}s", t.elapsed().as_secs_f32()))
                .unwrap_or_default();
            all_lines.push(Line::styled(
                format!("  {spinner} Thinking… {elapsed}"),
                Style::default().fg(HINT_GRAY).add_modifier(Modifier::ITALIC),
            ));
        }
        all_lines.push(Line::raw("")); // 消息间距
    }

    // 智能滚动:auto_scroll 贴底;手动滚回最底部时恢复跟随
    let visible_rows = msg_area.height as usize;
    let total = all_lines.len();
    let max_scroll = total.saturating_sub(visible_rows);
    if app.auto_scroll {
        app.scroll_offset = max_scroll;
    } else if app.scroll_offset >= max_scroll {
        app.auto_scroll = true;
        app.scroll_offset = max_scroll;
    }
    let scroll = app.scroll_offset.min(max_scroll);
    let visible: Vec<Line> = all_lines.into_iter().skip(scroll).take(visible_rows).collect();
    f.render_widget(
        Paragraph::new(Text::from(visible)).style(Style::default().bg(BG)),
        msg_area,
    );

    // 确认条(输入框上方):⚠ prompt + 可选项,当前高亮项反色;←/→ 移动,Enter 确认,Esc 拒绝
    if let Some(pending) = &app.pending_ask {
        let mut labels = vec!["允许"];
        if pending.persistable {
            labels.push("总是允许");
        }
        labels.push("拒绝");

        let mut spans = vec![
            Span::styled(
                " ⚠ 确认执行  ",
                Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
            ),
            Span::styled(pending.prompt.clone(), Style::default().fg(PRIMARY)),
            Span::raw("   "),
        ];
        for (i, label) in labels.iter().enumerate() {
            let style = if i == pending.selected {
                Style::default()
                    .fg(BG)
                    .bg(AMBER)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(HINT_GRAY)
            };
            spans.push(Span::styled(format!(" {label} "), style));
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(
            "  (←/→ 选择, Enter 确认, Esc 拒绝)",
            Style::default().fg(HINT_GRAY),
        ));
        f.render_widget(
            Paragraph::new(Line::from(spans)).style(Style::default().bg(BG)),
            chunks[2],
        );
    }

    // 输入框
    render_input(f, app, chunks[3]);
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
        chunks[4],
    );
    // 状态栏
    render_status_bar(f, app, chunks[5]);
}

fn role_label(role: &Role) -> Line<'static> {
    match role {
        Role::User => Line::styled(
            "▸ 你".to_string(),
            Style::default().fg(USER).add_modifier(Modifier::BOLD),
        ),
        Role::Assistant => Line::styled(
            "◇ hi".to_string(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Role::System => Line::styled("● system".to_string(), Style::default().fg(HINT_GRAY)),
    }
}
