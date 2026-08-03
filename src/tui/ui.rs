//! 渲染分发:空会话 → 欢迎页,有消息 → 对话页

use ratatui::Frame;

use super::pages;
use super::App;

pub fn draw(f: &mut Frame, app: &mut App) {
    if app.messages.is_empty() {
        pages::welcome::draw_welcome(f, app);
    } else {
        pages::chat::draw_chat(f, app);
    }
}
