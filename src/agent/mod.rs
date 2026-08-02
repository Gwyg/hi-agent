mod agent_loop;
mod engine;
mod memory;

use tokio::sync::oneshot;

pub use engine::Engine;

/// agent 循环向前端推送的事件
///
/// 前端按需消费:TUI 用 mpsc::Receiver,Web 用 ReceiverStream 转 SSE,CLI 直接 print。
/// 所有事件类型统一在此枚举,避免回调方案的多回调签名爆炸 + 单向不可回传问题。
#[derive(Debug)]
#[allow(dead_code)] // 公共事件 API:兼容模式下 collector 只取 Done,其余变体待前端接入后消费
pub enum EngineEvent {
    /// LLM 正文增量(流式输出,来自 chat_stream 的 token 分片)
    TokenDelta(String),
    /// 工具调用开始
    ToolStart {
        id: String,
        name: String,
        args: String,
    },
    /// 工具执行中的增量输出(可选,工具按需推)
    /// 同一 id 下可推 0 到 N 个:bash 推 stdout/stderr 行,edit 推 diff 片段;
    /// read/search 等一次性返回的工具不推,直接 ToolResult 终结
    /// TODO: 待 Tool trait 加流式参数后,agent_loop 在 execute 过程中 emit
    ToolOutputDelta { id: String, chunk: String },
    /// 工具执行结果(成功/失败都走这里,content 已含错误信息)
    ToolResult { id: String, content: String },
    /// 需用户确认(Action::Ask),前端通过 reply 回传决策
    /// reply 被 drop(前端未响应)时,agent_loop 自动 fallback 到"暂拒"以保持原行为
    /// persistable=true 时前端应提供"之后不再问"选项(对应 AskReply::Always)
    Ask {
        id: String,
        prompt: String,
        persistable: bool,
        reply: oneshot::Sender<AskReply>,
    },
    /// 一轮结束,最终回复(Stop)
    Done(String),
    /// 出错(Length/Filtered/未收敛)
    Error(String),
}

/// Ask 事件的用户回答回传
#[derive(Debug)]
#[allow(dead_code)] // 待前端接入 Ask 确认流后构造
pub enum AskReply {
    /// 仅本次同意
    One,
    /// 该类型永久同意(会话级记忆,后续同类型调用免确认)
    /// 仅当 EngineEvent::Ask { persistable: true } 时前端才应回传此值;
    /// agent_loop 收到后应登记到会话授权记忆,工具 assess 下次查询即放行
    /// TODO: 会话级授权记忆存储 + assess 查询(当前等同 One 执行)
    Always,
    /// 拒绝
    Deny,
}
