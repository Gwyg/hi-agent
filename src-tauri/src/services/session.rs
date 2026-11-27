//! 会话用例：列出本工作区的会话、打开一个已有会话、往前翻页。
//!
//! 这一层只负责一件事：**「打开会话」必须是一个原子动作** —— 换会话指针、换模型
//! 上下文、读首屏消息，三件事一起发生。少做一件，界面和后端就会各说各话：
//! 只读消息不换上下文，用户看到 A 的记录，模型却拿着 B 的记忆接着答。
//!
//! # 展示与模型要的不是同一批数据
//!
//! - 界面要**全量**：已压缩掉的部分用户仍要能往回翻到自己的原话；
//! - 模型只要**水位之后**的：之前的内容已经进了摘要，再装一遍等于同一段对话算两次。
//!
//! 所以一次打开要读两遍库。别试图把两份合成一份 —— 那是在两个消费方之间
//! 拉一条隐式依赖，日后任何一方改口径都会误伤另一方。

use anyhow::{bail, Context, Result};

use crate::services::{conversation, workspace};
use crate::store::{message, session, Store};

/// 首屏取多少轮对话。一轮 = 一条 user 到下一个 user 之前（含中间全部工具调用）。
///
/// **不按行数**：工具调用密集时一轮能到四十行，按行切会把一轮劈成两半 ——
/// 上一页末尾只剩几张工具卡，下一页开头找不到它们对应的提问。
const FIRST_TURNS: usize = 15;

/// 往上翻一页取多少轮。
const OLDER_TURNS: usize = 10;

/// 一次打开的结果：会话本身 + 首屏那一页。
pub struct Opened {
    pub session: session::Session,
    pub page: message::Page,
}

/// 本工作区的会话，最近的在前。
///
/// 不分页：每工作区只留 20 个（见 `services::conversation` 的会话清理），
/// 一次列全比翻页少两次往返，也不会漏掉某条。
pub fn list(store: &Store, limit: i64) -> Result<Vec<session::Session>> {
    let ws = workspace::current().context("工作区未初始化")?;
    store.tx(|tx| session::list_by_workspace(tx, &ws.id, limit))
}

/// 打开一个已有会话。
///
/// 会话必须属于**当前工作区**：不属于就当它不存在。换目录之后还能点开别的目录的
/// 对话，用户看到的是「在 A 的项目里读 B 的聊天记录」，比打不开更让人困惑。
///
/// 失败时顺手清掉指向它的断点：那个会话可能已被清理，留着指针只会让每次启动
/// 都白失败一次。
pub async fn open(store: &Store, id: &str) -> Result<Opened> {
    let ws = workspace::current().context("工作区未初始化")?;

    let loaded = store.tx(|tx| {
        let row = session::load(tx, id)?.context("会话不存在")?;
        if row.workspace_id != ws.id {
            bail!("会话不属于当前工作区");
        }
        // 首屏按轮切；上下文只要水位之后的
        let page = message::load_turns(tx, id, None, FIRST_TURNS)?;
        let context_msgs = message::load(tx, id, row.summary_upto_seq)?;
        Ok((row, page, context_msgs))
    });

    let (row, page, context_msgs) = match loaded {
        Ok(v) => v,
        Err(e) => {
            if crate::config::resume::get().session.as_deref() == Some(id) {
                if let Err(e) = crate::config::resume::clear_session() {
                    tracing::warn!(error = %e, "清理失效的会话断点失败");
                }
            }
            return Err(e);
        }
    };

    conversation::adopt(
        &row.id,
        &row.workspace_id,
        row.summary.clone(),
        context_msgs,
    )
    .await;
    Ok(Opened { session: row, page })
}

/// 往前翻一页。
///
/// **不碰模型上下文**：往回看历史是纯展示行为，不该改变下一轮要发给模型的东西。
pub fn older(store: &Store, session_id: &str, before_seq: i64) -> Result<message::Page> {
    store.tx(|tx| message::load_turns(tx, session_id, Some(before_seq), OLDER_TURNS))
}

/// 上次打开的会话 id（断点）。启动时前端据此决定要不要自动打开它。
pub fn resume_id() -> Option<String> {
    crate::config::resume::get().session
}

/// 用户手写的标题上限。
///
/// 自动标题截到 10 字（侧边栏一行的宽度），改名不该沿用那个口径——用户主动写的
/// 名字短了就没意义。40 字足够表达，再长在列表里也只能靠省略号。
const TITLE_MAX_CHARS: usize = 40;

/// 改会话标题。
///
/// 空标题一律拒绝：列表里留一个点不中的空行没有意义，也让删掉重来变成唯一出路。
/// 连续空白压成一个空格，与自动标题同一个口径（见 `conversation::title_of`）。
pub fn rename(store: &Store, id: &str, title: &str) -> Result<()> {
    let title = normalize_title(title)?;
    let ws = workspace::current().context("工作区未初始化")?;
    store.tx(|tx| {
        let row = session::load(tx, id)?.context("会话不存在")?;
        // 与 `open` 同一个判断：别的目录的会话当它不存在，不让界面跨目录改名
        if row.workspace_id != ws.id {
            bail!("会话不属于当前工作区");
        }
        session::rename(tx, id, &title)
    })
}

/// 删除一个会话（消息一起删）。
///
/// 两件收尾必须做，少一件都会留下指向不存在数据的指针：
/// - 断点指着它 → 清掉，否则每次启动都白失败一次；
/// - 它正被模型用着 → 清内存上下文，否则下一条消息会往已经不存在的那一行里写。
pub async fn delete(store: &Store, id: &str) -> Result<()> {
    let ws = workspace::current().context("工作区未初始化")?;
    store.tx(|tx| {
        let row = session::load(tx, id)?.context("会话不存在")?;
        if row.workspace_id != ws.id {
            bail!("会话不属于当前工作区");
        }
        session::delete(tx, id)
    })?;

    if crate::config::resume::get().session.as_deref() == Some(id) {
        if let Err(e) = crate::config::resume::clear_session() {
            tracing::warn!(error = %e, "清理被删会话的断点失败");
        }
    }
    if conversation::current_id().as_deref() == Some(id) {
        conversation::reset().await;
    }
    Ok(())
}

/// 标题归一：trim、连续空白压成一个空格、按字符截断。
fn normalize_title(raw: &str) -> Result<String> {
    let text = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        bail!("标题不能为空");
    }
    Ok(text.chars().take(TITLE_MAX_CHARS).collect())
}
