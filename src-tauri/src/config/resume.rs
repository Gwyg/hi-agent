//! 启动断点：上次停在哪个工作区、哪个会话。
//!
//! **这段不是数据源，只是指针。** 工作区的真身在 SQLite `workspace` 表（path 唯一），
//! 会话的真身在 `session` 表；所以这里只存路径和 id，名字 / 标题一概不存副本 ——
//! 一旦存了副本，用户在界面改名后就得维护两份，而两份迟早对不上。
//!
//! 整段可以随时删：删了就当从没停过，启动按「最近打开的工作区」重新推断；
//! 写失败也只当没写（调用方只告警），因为它影响的是下次启动，不是本次运行。
//!
//! 写入时机是**状态变化时**，不是关闭时：强杀、断电、系统关机都不会给你关闭回调，
//! 而切工作区 / 点会话这些动作一定会经过程序。

use serde::Deserialize;
use std::io;
use std::sync::RwLock;

use crate::util::Value;

/// app.toml `[resume]` 段。
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ResumeConfig {
    /// 上次用的工作区：**绝对路径**（不是 id）。库文件丢了也能靠它知道去过哪个目录。
    #[serde(default)]
    pub workspace: Option<String>,
    /// 上次打开的会话 id。启动时由 `services::session` 消费。
    #[serde(default)]
    pub session: Option<String>,
}

static RESUME: RwLock<ResumeConfig> = RwLock::new(ResumeConfig {
    workspace: None,
    session: None,
});

/// 启动注入（`config::init` 调用一次）。
pub(crate) fn init(section: &ResumeConfig) {
    if let Ok(mut guard) = RESUME.write() {
        *guard = section.clone();
    }
}

/// 当前断点。只在启动阶段读（运行期要的是「现在是什么」，那是 services 的状态）。
pub fn get() -> ResumeConfig {
    RESUME.read().map(|c| c.clone()).unwrap_or_default()
}

/// 切工作区：写 `workspace`，**同时删掉 `session`**。
///
/// 会话归属于某个工作区，换了目录还指着上一个会话，下次启动就会把用户送进一个
/// 不属于当前工作区的对话里。两个键一次写完（一个事务 + 一次落盘），不留半写状态。
pub fn set_workspace(path: &str) -> io::Result<()> {
    crate::config::persist(
        &["resume"],
        &[("workspace", Some(Value::from(path))), ("session", None)],
    )?;
    if let Ok(mut guard) = RESUME.write() {
        guard.workspace = Some(path.to_string());
        guard.session = None;
    }
    Ok(())
}

/// 记下当前会话。写点都在**会话真的存在**之后：首条消息落库、或用户点开某个历史会话。
///
/// 与 `set_workspace` 相反，这里只动 `session` 一个键 —— 工作区没变。
pub fn set_session(id: &str) -> io::Result<()> {
    // 值没变就不落盘：这个方法每轮结束都会被调用一次，而 `persist` 是整文件重写，
    // 绝大多数轮次的 id 和上次一样，没必要每次都改一遍文件。
    if RESUME
        .read()
        .map(|c| c.session.as_deref() == Some(id))
        .unwrap_or(false)
    {
        return Ok(());
    }
    crate::config::persist(&["resume"], &[("session", Some(Value::from(id)))])?;
    if let Ok(mut guard) = RESUME.write() {
        guard.session = Some(id.to_string());
    }
    Ok(())
}

/// 清掉会话指针。会话已被清理 / 打开失败时用：留着它只会让下次启动再失败一遍。
pub fn clear_session() -> io::Result<()> {
    crate::config::persist(&["resume"], &[("session", None)])?;
    if let Ok(mut guard) = RESUME.write() {
        guard.session = None;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 段形状：两个键都可缺，缺了就是 None（不报错）
    #[test]
    fn parses_with_missing_keys() {
        let empty: ResumeConfig = toml::from_str("").expect("空段可用");
        assert!(empty.workspace.is_none());

        let full: ResumeConfig = toml::from_str(
            r#"
workspace = "D:/proj/hi-agent"
session = "0192f3a1"
"#,
        )
        .expect("完整段可用");
        assert_eq!(full.workspace.as_deref(), Some("D:/proj/hi-agent"));
        assert_eq!(full.session.as_deref(), Some("0192f3a1"));
    }
}
