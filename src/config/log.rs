//! 日志配置:日志目录 / 保留天数 / 级别
//!
//! 日志按天滚动写入 `~/.hi-agent/log/hi-agent.log.YYYY-MM-DD`,
//! 启动时清理超过保留期的旧文件。

use serde::Deserialize;

/// 默认保留天数
const DEFAULT_RETENTION_DAYS: u32 = 7;

/// 日志配置
#[derive(Deserialize, Clone, Default)]
pub struct LogConfig {
    /// 保留天数,默认 7。0 表示永不清理
    #[serde(default)]
    pub retention_days: Option<u32>,
}

impl LogConfig {
    /// 解析最终保留天数:env(HI_AGENT_LOG_RETENTION_DAYS)> 文件 > 默认
    pub fn retention_days(&self) -> u32 {
        std::env::var("HI_AGENT_LOG_RETENTION_DAYS")
            .ok()
            .and_then(|s| s.parse().ok())
            .or(self.retention_days)
            .unwrap_or(DEFAULT_RETENTION_DAYS)
    }
}
