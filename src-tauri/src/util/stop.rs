//! 协作式停止信号:让跑在别处的长循环能被叫停。
//!
//! `spawn_blocking` 起的工作线程不随 future 一起 drop——超时或取消时,外层只是不再等它,
//! 线程还在往下跑。信号是唯一的收手渠道:线程内每轮轮询 `is_stopped`。
//!
//! `new()` 出来的那份是 owner,它消失时置位;`clone()` 出来的都是普通句柄,只共享同一个标志,
//! 消失时什么也不做。所以 owner 挂在 `execute` 的栈上就能覆盖返回 / 超时 / 取消 / panic 四条路,
//! 不用记得手动停。
//!
//! 置位是单向的:任何一方置位都只让「结束」来得更早,不带原因也不带来源。
//!
//! 只覆盖「循环还在转」的情况:线程阻塞在单个系统调用里时轮询不到,那种只能等 OS 返回。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// 停止信号。同一份标志的多个持有者共享状态,但只有一个 owner。
pub struct StopSignal {
    flag: Arc<AtomicBool>,
    is_owner: bool,
}

impl StopSignal {
    /// owner 版本:消失时置位。
    pub fn new() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
            is_owner: true,
        }
    }

    /// 置位。单向:置位后不可复位。
    pub fn stop(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }

    /// 轮询。放在长循环的每一轮里,越靠前越早收手。
    ///
    /// `Relaxed` 是够的:这里只传递「该停了」这一个方向的状态,没有任何数据靠它同步——
    /// 循环里碰的其他数据都由各自的锁或所有权保证可见性。
    pub fn is_stopped(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }
}

impl Default for StopSignal {
    fn default() -> Self {
        Self::new()
    }
}

/// 手写而非 derive:derive 会把 `is_owner: true` 一起抄过去,副本就都成了 owner,
/// 任何一个消失都会置位。整个方案只靠这里把标志改回 false。
impl Clone for StopSignal {
    fn clone(&self) -> Self {
        Self {
            flag: Arc::clone(&self.flag),
            is_owner: false,
        }
    }
}

impl Drop for StopSignal {
    fn drop(&mut self) {
        if self.is_owner {
            self.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::StopSignal;

    /// owner 消失才置位;普通句柄消失什么也不做
    #[test]
    fn only_owner_stops_on_drop() {
        let owner = StopSignal::new();
        let watcher = owner.clone();

        drop(owner.clone()); // 副本消失
        assert!(!watcher.is_stopped());

        drop(owner); // owner 消失
        assert!(watcher.is_stopped());
    }

    /// 句柄看得见 owner 的置位
    #[test]
    fn handle_sees_owner_stop() {
        let owner = StopSignal::new();
        let handle = owner.clone();
        assert!(!handle.is_stopped());

        owner.stop();
        assert!(handle.is_stopped());
    }

    /// 句柄也能置位,owner 同样看得见:能力对等,只有生命周期不同
    #[test]
    fn owner_sees_handle_stop() {
        let owner = StopSignal::new();
        let handle = owner.clone();

        handle.stop();
        assert!(owner.is_stopped());
    }
}
