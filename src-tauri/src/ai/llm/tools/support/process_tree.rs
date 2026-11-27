//! 子进程树回收:命令退出后,把它派生出来的一切进程一起清掉。
//!
//! `kill_on_drop` 只杀直接子进程(shell 本身),它 fork 出去的 `node` / `sleep 300` 会变成
//! 孤儿继续占端口、锁文件。两个平台的原生手段不同:
//!
//! - Windows:把 shell 绑进 JobObject 并设 `KILL_ON_JOB_CLOSE`,关句柄即杀全树;
//! - Unix:让 shell 自成一个进程组(`process_group(0)`),按组发 `SIGKILL`。
//!
//! 对外只暴露 `configure` 与 `ProcessTree`,各两份 `cfg` 实现——调用处不写平台判断。
//!
//! 语义:**命令退出即清理**。`npm run dev &` 这类「留个常驻服务」的用法在两个平台上都不
//! 成立,bash 工具只跑前台命令。

use tokio::process::Command;

/// spawn 前的平台准备。
///
/// Unix 让 shell 自成一个进程组(pgid = 它自己的 pid),这是下面 `Drop` 里 `kill(-pgid)`
/// 能安全工作的前提——不设的话子进程继承 agent 的进程组,负号会把 agent 自己一起杀掉。
/// Windows 靠 JobObject,不需要预先配置。
#[cfg(unix)]
pub fn configure(cmd: &mut Command) {
    cmd.process_group(0);
}

#[cfg(windows)]
pub fn configure(_cmd: &mut Command) {}

/// 持有回收所需的标识;drop 即清理整棵进程树。
/// `new` 拿不到资源时返回 `None`,调用方降级为只有 `kill_on_drop` 的行为。
///
/// Unix:shell 已是进程组组长,故 pgid 就是它自己的 pid;`kill(-pgid, SIGKILL)` 一次清掉
/// 组内所有进程。局限是子进程若自己调用 `setsid`(`daemon()` / `start-stop-daemon`)会脱组
/// 逃逸——覆盖那类情况只能靠进程树遍历,代价远超收益。
#[cfg(unix)]
pub struct ProcessTree {
    pgid: i32,
}

#[cfg(unix)]
impl ProcessTree {
    pub fn new(pid: u32) -> Option<Self> {
        // pid 超出 i32 只在 pid 空间极端配置下才可能,拿不到就降级
        i32::try_from(pid).ok().map(|pgid| Self { pgid })
    }
}

#[cfg(unix)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        // SAFETY: kill 只读传入的整数参数,不碰内存。负号表示「进程组」而非单个 pid。
        // 组已空时返回 ESRCH,无害;pgid 被复用的时间窗需 pid 回绕,极小。
        unsafe { libc::kill(-self.pgid, libc::SIGKILL) };
    }
}

/// Windows:把 shell 绑进一个"关闭即杀整棵进程树"的 JobObject。
/// `KILL_ON_JOB_CLOSE` 保证即便异常路径没显式调用,守卫 drop 关闭句柄时也会杀光整棵树。
#[cfg(windows)]
pub struct ProcessTree {
    job: windows_sys::Win32::Foundation::HANDLE,
}

// HANDLE 是进程内全局有效的内核对象引用,跨线程搬运其值安全(仅作为数值传给
// CloseHandle,从不按内存解引用);tokio 任务可能在多线程间迁移 future,故显式标注 Send。
// Sync 不需要(单所有者,仅 &self 调用)。
#[cfg(windows)]
unsafe impl Send for ProcessTree {}

#[cfg(windows)]
impl ProcessTree {
    /// 把 pid 指定的进程(及其未来子进程)绑进 JobObject;任一步失败返回 None。
    ///
    /// 失败是可能的:当前进程若已身处某个 Job,`AssignProcessToJobObject` 在旧系统上
    /// 不允许嵌套(CI / 容器 / 某些终端会预先放进 Job),此时降级为 `kill_on_drop`。
    pub fn new(pid: u32) -> Option<Self> {
        use windows_sys::Win32::Foundation::*;
        use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
        use windows_sys::Win32::System::JobObjects::*;
        use windows_sys::Win32::System::Threading::*;
        unsafe {
            let job = CreateJobObjectW(
                std::ptr::null::<SECURITY_ATTRIBUTES>(),
                std::ptr::null::<u16>(),
            );
            if job.is_null() {
                return None;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                CloseHandle(job);
                return None;
            }
            let proc = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, FALSE, pid);
            if proc.is_null() {
                CloseHandle(job);
                return None;
            }
            let ok = AssignProcessToJobObject(job, proc);
            CloseHandle(proc);
            if ok == 0 {
                CloseHandle(job);
                return None;
            }
            Some(ProcessTree { job })
        }
    }
}

#[cfg(windows)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        // SAFETY: 句柄由 new 创建且仅在此处置一次;KILL_ON_JOB_CLOSE 已设置,
        // 关闭句柄即终止整棵进程树。
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.job) };
    }
}
