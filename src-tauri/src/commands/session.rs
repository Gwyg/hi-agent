//! 会话命令：列出会话、打开会话、往前翻页。
//!
//! 本层只做**形状转换**：`Msg` 没有 `Serialize`，而前端也不该去解析库里的
//! `payload` JSON。所以在这里把三种角色摊平成同一套字段（不适用的为 null / 空数组），
//! 前端拿到就能直接建气泡。
//!
//! `arguments` 原样透传字符串：它是模型给的 JSON 原文，前端自己解析出参数
//! （工具卡片的 diff 就是这么算的）。在这里转成对象再序列化会丢键序、吞重复键。

use serde::Serialize;
use std::sync::Arc;
use tauri::State;

use crate::ai::llm::types::{Content, Msg};
use crate::services::session as service;
use crate::store::{message, session, Store};

/// 侧边栏列表里的一行。标题是后端建行时截好的，不需要前端再截一次。
#[derive(Serialize)]
pub struct SessionView {
    pub id: String,
    pub title: String,
}

impl From<session::Session> for SessionView {
    fn from(s: session::Session) -> Self {
        Self {
            id: s.id,
            title: s.title,
        }
    }
}

/// 工具调用的形状。`id` 用来和工具结果配对（`call_id`）。
#[derive(Serialize)]
pub struct ToolCallView {
    pub id: String,
    pub name: String,
    /// 模型给的 JSON 原文
    pub arguments: String,
}

/// 一条消息的扁平形状。
#[derive(Serialize)]
pub struct MessageView {
    pub role: &'static str,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallView>,
    /// 仅 `role == "tool"`：这条结果属于哪个调用
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
}

impl From<Msg> for MessageView {
    fn from(m: Msg) -> Self {
        match m {
            Msg::User(u) => Self {
                role: "user",
                content: u
                    .content
                    .iter()
                    .map(|c| match c {
                        Content::Text { text } => text.as_str(),
                    })
                    .collect::<Vec<_>>()
                    .join(""),
                thinking: None,
                tool_calls: Vec::new(),
                call_id: None,
            },
            Msg::Assistant(a) => Self {
                role: "assistant",
                content: a.content,
                thinking: a.thinking.map(|t| t.text),
                tool_calls: a
                    .tool_calls
                    .into_iter()
                    .map(|c| ToolCallView {
                        id: c.id,
                        name: c.name,
                        arguments: c.arguments,
                    })
                    .collect(),
                call_id: None,
            },
            Msg::Tool(t) => Self {
                role: "tool",
                content: t.content,
                thinking: None,
                tool_calls: Vec::new(),
                call_id: Some(t.call_id),
            },
            // 库里不存 system（每轮由模板现渲染），走到这里是调用方搞错了。
            // 不 panic：宁可让界面少一条不认识的记录，也不该把打开会话整个弄崩。
            Msg::System(_) => Self {
                role: "system",
                content: String::new(),
                thinking: None,
                tool_calls: Vec::new(),
                call_id: None,
            },
        }
    }
}

/// 一页消息。
#[derive(Serialize)]
pub struct MessagePage {
    pub messages: Vec<MessageView>,
    /// 本页之前还有没有更早的
    pub has_more: bool,
    /// 本页最老一行的 seq，翻页时原样传回
    pub oldest_seq: i64,
}

impl From<message::Page> for MessagePage {
    fn from(p: message::Page) -> Self {
        // 占位 tool 行的 seq 是 0，但它永远不会成为一页的第一行（页首一定是真实的 user 行）
        let oldest_seq = p.rows.first().map(|r| r.seq).unwrap_or(0);
        Self {
            messages: p.rows.into_iter().map(|r| r.msg.into()).collect(),
            has_more: p.has_more,
            oldest_seq,
        }
    }
}

/// 打开会话的返回：会话本身 + 首屏那一页。
///
/// 摊平而不是嵌套 `page`：前端拿到的就是 `{ session, messages, has_more, oldest_seq }`，
/// 少一层解构，也和 `load_older` 的返回对齐。
#[derive(Serialize)]
pub struct SessionPage {
    pub session: SessionView,
    #[serde(flatten)]
    pub page: MessagePage,
}

/// 本工作区的会话列表（最近打开在前）。
///
/// `limit` 由服务层按业务上限给，前端不需要传：一个工作区最多 20 个会话，
/// 这个接口没有「要十万行」的用法。
#[tauri::command]
pub fn list_sessions(state: State<'_, Arc<Store>>) -> Result<Vec<SessionView>, String> {
    service::list(state.inner(), 50)
        .map(|list| list.into_iter().map(SessionView::from).collect())
        .map_err(|e| format!("读取会话列表失败: {e:#}"))
}

/// 打开一个已有会话：模型上下文一起切过去，返回首屏消息。
#[tauri::command]
pub async fn open_session(state: State<'_, Arc<Store>>, id: String) -> Result<SessionPage, String> {
    service::open(state.inner(), &id)
        .await
        .map(|o| SessionPage {
            session: o.session.into(),
            page: o.page.into(),
        })
        .map_err(|e| format!("打开会话失败: {e:#}"))
}

/// 往前翻一页。只读消息，不动模型上下文。
#[tauri::command]
pub fn load_older(
    state: State<'_, Arc<Store>>,
    session_id: String,
    before_seq: i64,
) -> Result<MessagePage, String> {
    service::older(state.inner(), &session_id, before_seq)
        .map(MessagePage::from)
        .map_err(|e| format!("读取更早的消息失败: {e:#}"))
}

/// 上次打开过的会话（断点）。没有 / 已被清理时返回 null，前端从空白开始。
#[tauri::command]
pub fn get_resume_session() -> Option<String> {
    service::resume_id()
}

/// 改会话标题。空白标题由服务层拒绝，错误原样给前端。
#[tauri::command]
pub fn rename_session(
    state: State<'_, Arc<Store>>,
    id: String,
    title: String,
) -> Result<(), String> {
    service::rename(state.inner(), &id, &title).map_err(|e| format!("重命名会话失败: {e:#}"))
}

/// 删除一个会话（消息一起删）。
///
/// 删的是当前会话时服务层会把模型上下文一起清掉，前端只需把界面回到空白。
#[tauri::command]
pub async fn delete_session(state: State<'_, Arc<Store>>, id: String) -> Result<(), String> {
    service::delete(state.inner(), &id)
        .await
        .map_err(|e| format!("删除会话失败: {e:#}"))
}
