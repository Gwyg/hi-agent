//! TUI 主题:配色 + 图标常量

use ratatui::style::Color;

/// spinner 6 帧旋转(参考 Codex 风格)
pub(crate) const SPINNER: [&str; 6] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴"];

/// 空会话 Logo:方块字母 "hi"
pub(crate) const LOGO: &[&str] = &["█  █   █", "█  █   █", "████   █", "█  █   █", "█  █   █"];

/// 背景:纯黑
pub(crate) const BG: Color = Color::Rgb(0x0a, 0x0a, 0x0a);
/// 输入盒底色
pub(crate) const INPUT_BG: Color = Color::Rgb(0x1a, 0x1a, 0x1a);
/// 主文字(输入内容)
pub(crate) const PRIMARY: Color = Color::Rgb(0xe5, 0xe7, 0xeb);
/// 强调青(Header / AI 角色标签 / 光标)
pub(crate) const ACCENT: Color = Color::Rgb(0x22, 0xd3, 0xee);
/// 用户角色标签(淡紫 violet-400,与 AI 青色区分)
pub(crate) const USER: Color = Color::Rgb(0xa7, 0x8b, 0xfa);
/// 深青(左竖线 / Logo 底部 / Build 标签)
pub(crate) const ACCENT_DARK: Color = Color::Rgb(0x06, 0xb6, 0xd4);
/// 辅助文字(placeholder / 快捷键 / 状态栏)
pub(crate) const HINT_GRAY: Color = Color::Rgb(0x6b, 0x72, 0x80);
/// 琥珀(工具 Running)
pub(crate) const AMBER: Color = Color::Rgb(0xf5, 0x9e, 0x0b);
/// 红(错误 / 工具 Error)
pub(crate) const RED: Color = Color::Rgb(0xef, 0x44, 0x44);
