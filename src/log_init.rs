//! 日志初始化:按天滚动文件 + 启动时清理过期日志
//!
//! 产物:`~/.hi-agent/log/hi-agent.log.YYYY-MM-DD`(跨天自动新文件)
//! 清理:启动时扫日志目录,删早于 today - retention_days 的旧文件
//! env:`HI_AGENT_LOG_DIR` 覆盖日志目录(默认 ~/.hi-agent/log)

use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_appender::rolling::{RollingFileAppender, Rotation};

/// 初始化日志系统:返回 (NonBlocking writer, guard)
///
/// guard 必须 hold 到进程结束才保证尾部日志 flush,main 末尾 drop
/// 注意:此处只建 appender,不做清理(清理需在 subscriber init 后调,日志才生效)
pub fn init(log_dir: PathBuf) -> (NonBlocking, WorkerGuard) {
    // 按天滚动:跨天自动新建 hi-agent.log.YYYY-MM-DD
    let appender = RollingFileAppender::new(Rotation::DAILY, &log_dir, "hi-agent.log");
    tracing_appender::non_blocking(appender)
}

/// 日志目录解析:env(HI_AGENT_LOG_DIR)> 默认 ~/.hi-agent/log;兜底当前目录 log/
pub fn resolve_log_dir() -> PathBuf {
    if let Ok(d) = std::env::var("HI_AGENT_LOG_DIR") {
        return PathBuf::from(d);
    }
    dirs::home_dir()
        .map(|h| h.join(".hi-agent").join("log"))
        .unwrap_or_else(|| PathBuf::from("log"))
}

/// 清理过期日志文件:扫描目录,删早于保留期的文件
///
/// 文件名格式:`hi-agent.log.YYYY-MM-DD`(tracing-appender daily rotation 产物)
/// 不匹配的文件不动,避免误删用户其他文件
/// 必须在 tracing subscriber init 后调,清理日志才落盘
pub fn cleanup_old_logs(dir: &Path, retention_days: u32) {
    if retention_days == 0 {
        return; // 0 = 永不清理
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return, // 目录不存在或无权读,静默跳过
    };
    let cutoff = std::time::SystemTime::now() - Duration::from_secs(86_400 * retention_days as u64);
    let prefix = "hi-agent.log.";
    for entry in entries.flatten() {
        let path = entry.path();
        // 只处理文件,跳过子目录
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // 必须是 hi-agent.log.YYYY-MM-DD 形式(含前缀 + 日期后缀)
        if !name.starts_with(prefix) {
            continue;
        }
        let date_part = &name[prefix.len()..];
        // 期望 YYYY-MM-DD(10 字符)。tracing-appender daily 格式
        if date_part.len() != 10 {
            continue;
        }
        // 按文件修改时间判定,早于 cutoff 删
        match entry.metadata().and_then(|m| m.modified()) {
            Ok(modified) if modified < cutoff => {
                if let Err(e) = std::fs::remove_file(&path) {
                    tracing::warn!("清理旧日志失败 {}: {e}", path.display());
                } else {
                    tracing::info!("清理过期日志: {}", path.display());
                }
            }
            _ => {}
        }
    }
}
