//! 日志初始化:tracing subscriber 安装。
//!
//! - 文件层：始终写入 `~/.hi-agent/logs/hi-agent.log.<YYYY-MM-DD>`(按天滚动、无颜色、带 target)。
//! - stdout 层：仅开发环境(`debug_assertions`)启用,让 `tauri dev` 终端实时可见;
//!   生产构建是 `windows_subsystem = "windows"`,无控制台,故不挂 stdout。
//! - 过滤级别优先读 `RUST_LOG` 环境变量,缺省 `info`。
//!
//! 非阻塞写入器返回的 `WorkerGuard` 由模块级 `OnceLock` 持有至进程退出,
//! 否则后台写入线程会提前结束、日志不落盘——因此 `init()` 无需把 guard 层层回传给 `run()`。

use std::sync::{Mutex, OnceLock};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::util::app_dir;

/// 持有 guard 至进程退出(非阻塞写入线程靠它续命);`OnceLock` 保证只 set 一次、不提前 drop。
///
/// 包一层 `Mutex<Option<_>>` 是为了 [`flush`]:启动失败时会走 `std::process::exit`,
/// 那条路径**跳过所有 `Drop`**,guard 不主动取出来 drop 的话,缓冲里最后几条(通常正好是
/// 致命错误本身)不会落盘。
static LOG_GUARD: OnceLock<Mutex<Option<WorkerGuard>>> = OnceLock::new();

pub fn init() {
    let log_dir = app_dir().join("logs");
    let _ = std::fs::create_dir_all(&log_dir);

    let file_appender = tracing_appender::rolling::daily(&log_dir, "hi-agent.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // 文件层：无 ANSI 颜色、带 target，便于在编辑器/终端里 grep。
    let file_layer = fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_target(true);

    let subscriber = tracing_subscriber::registry()
        .with(env_filter)
        .with(file_layer);

    // 开发环境额外镜像到 stdout，保证开发期可见。
    #[cfg(debug_assertions)]
    let subscriber = subscriber.with(fmt::layer().with_writer(std::io::stdout).with_ansi(true));

    subscriber.init();
    // 持有 guard 至进程退出,否则非阻塞写入线程提前结束、日志不落盘。
    let _ = LOG_GUARD.set(Mutex::new(Some(guard)));
}

/// 进程即将退出前强制把缓冲刷进文件。
///
/// 只给「启动失败 → `std::process::exit(1)`」这条路径用:那条路径不跑 `Drop`,
/// 而非阻塞写入器只在 `WorkerGuard` drop 时才把剩余日志交给后台线程。
/// 正常退出不必调——`main` 返回时静态变量照常析构。
///
/// 幂等:第二次调用时 `Option` 已是 `None`,什么也不做。
pub fn flush() {
    let Some(slot) = LOG_GUARD.get() else {
        return;
    };
    if let Ok(mut guard) = slot.lock() {
        drop(guard.take());
    }
}
