/// 示例命令（Tauri 官方脚手架遗留）。
/// 当前项目已不需要，可整体删除本文件并在 `commands/mod.rs` 移除对应导出。
#[tauri::command]
pub fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}
