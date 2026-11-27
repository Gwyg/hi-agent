// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ai;
mod commands;
mod config;
mod services;
// 存储层：SQLite 单库，程序生成的结构化数据都住在这里。
// 写入侧（工作区恢复、每轮消息落库）与读取侧（会话列表、打开某个会话、往前翻页）都已接。
mod store;
mod util;

use anyhow::Context;
use std::sync::Arc;
use tauri::Manager;
use tauri_plugin_dialog::DialogExt;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .invoke_handler(commands::handler())
        .setup(|app| {
            // 启动只有一个分叉点：要么完整成功，要么弹窗后退出。
            if let Err(e) = bootstrap(app) {
                fatal(app, &e);
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// 启动的全部步骤。**任何一步失败都往上抛，由 `setup` 统一兜住。**
///
/// 致命性由是否用 `?` 表达，不需要额外的错误分类：想拦启动就 `?` / `bail!`，
/// 想降级运行就 `warn!` 后继续（WAL 不可用、老库 `auto_vacuum` 改不了、api_key 解不开
/// 都属于后者，都在各自模块里处理完了）。
fn bootstrap(app: &mut tauri::App) -> anyhow::Result<()> {
    config::init().context("初始化配置失败")?;

    let store = store::open_and_report().context("初始化存储层失败")?;

    // 工作区必须在装进 state 之前定下来：它既决定工具层的边界根，也决定断点记哪一行。
    // 顺序也是固定的——目录要先查库（工作区是 workspace 表里的一行），没有库就无从判断。
    services::workspace::init(&store).context("确定工作区失败")?;
    // 用 `Arc` 装进 state：后台清理线程比一轮对话活得久，必须拿到 `'static` 的句柄。
    app.manage(Arc::new(store));

    // 窗口在 `tauri.conf.json` 里是 `visible: false`，只在这里 `show`。
    // 取不到就说明创建失败了——此时若静默继续，用户会得到一个「没有窗口的进程」，
    // 再双击图标只会把它再叫醒一次，症状是「什么都没发生」。
    let win = app.get_webview_window("main").context("主窗口创建失败")?;
    position_window(&win);
    Ok(())
}

/// 致命错误：记日志 → 刷盘 → 弹窗 → 退出。
///
/// 顺序不能换：弹窗会阻塞到用户点确定，而用户很可能就在这一刻去翻日志。
/// 只有一个「确定」按钮，点它、点窗口关闭、按回车都会返回，然后进程结束。
fn fatal(app: &tauri::App, err: &anyhow::Error) -> ! {
    tracing::error!(error = ?err, "启动失败，程序即将退出");
    // `process::exit` 跳过所有 `Drop`，非阻塞日志的缓冲要靠这一步主动刷出去。
    config::logging::flush();

    app.dialog()
        .message(truncate(&format!("{err:#}"), 800))
        .title("hi-agent 无法启动")
        .buttons(tauri_plugin_dialog::MessageDialogButtons::Ok)
        .kind(tauri_plugin_dialog::MessageDialogKind::Error)
        .blocking_show();

    std::process::exit(1);
}

/// 弹窗正文超过 `max_chars` 就截断。
///
/// 因果链逐层 `with_context` 之后可能上千字，模态框里读不了那么长；完整内容在日志里。
fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let head: String = text.chars().take(max_chars).collect();
    format!("{head}…\n\n（完整错误已写入日志）")
}

/// 定位并展示主窗口。
///
/// 窗口在 `tauri.conf.json` 中设为 `visible: false`(先不可见创建),这里先按真实屏幕尺寸
/// 计算"居中再往右下轻移约 5%"的目标位置,`set_position` 后再 `show()`,
/// 保证窗口第一帧就停在目标位置,不会先闪现默认位置再跳过来。
///
/// 定位失败不报错:位置不理想远好过打不开窗口,所以每一步都是 `let _ =`。
fn position_window(win: &tauri::WebviewWindow) {
    if let (Ok(Some(monitor)), Ok(w)) = (win.current_monitor(), win.inner_size()) {
        let m = monitor.size();
        // 居中后再往右下轻移约屏幕 5%(不要太多),任意分辨率下都只是"稍微偏右下"
        let mut x =
            (m.width / 2).saturating_sub(w.width / 2) as i32 + (m.width as f64 * 0.05) as i32;
        let mut y =
            (m.height / 2).saturating_sub(w.height / 2) as i32 + (m.height as f64 * 0.05) as i32;
        // 防止越界(窗口超出屏幕右下)
        x = x.clamp(0, (m.width.saturating_sub(w.width)) as i32);
        y = y.clamp(0, (m.height.saturating_sub(w.height)) as i32);
        let _ = win.set_position(tauri::PhysicalPosition::new(x, y));
    }
    let _ = win.show();
}
