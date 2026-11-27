//! 时间原语：全项目统一的时间口径。
//!
//! 库里所有时间戳都是 `INTEGER` 毫秒（Unix epoch）。**不用 SQLite 的 `datetime()` 字符串**：
//! 字符串排序依赖格式是否一致、带不带时区、补不补零，一旦有一行写歪，排序就静默出错；
//! 整数比较天然正确，也便于做「最近 N 天」这类区间过滤。
//!
//! 只用系统时钟（不做单调时钟、不做 NTP 校准）：用途是**记录发生时刻**，不是测量间隔。
//! 用户改系统时间会让新记录的时间戳跳变——排序以业务自己保证的单调序号（如消息 `seq`）为准，
//! 时间戳只用于展示与粗粒度过滤。

/// 当前 Unix 毫秒时间戳。
// 仓储尚未接入，眼下只有测试在用；理由与配套豁免见 `util/mod.rs` 的 re-export。
#[allow(dead_code)]
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        // 系统时间早于 1970（时钟被改坏）时退回 0，而不是 panic：写不进去比写个 0 更糟
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::now_ms;

    #[test]
    fn is_monotonic_and_in_a_sane_range() {
        let a = now_ms();
        let b = now_ms();
        assert!(b >= a, "时间不应倒退");
        // 2020-09-13 之后、2100 之前：足以发现「当成秒用了」这类错误（差 1000 倍）
        assert!(a > 1_600_000_000_000, "{a} 像秒级而不是毫秒级");
        assert!(a < 4_102_444_800_000, "{a} 超出合理范围");
    }
}
