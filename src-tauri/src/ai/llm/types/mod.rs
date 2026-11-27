//! LLM 中立模型:应用层描述「说了什么、想干什么」的**唯一入口**。
//!
//! # 范围（2026-11-10 定）
//!
//! 本项目按 **DeepSeek 专用 agent** 开发，只接 `https://api.deepseek.com/chat/completions`
//! 的 OpenAI Chat Completions 形状，不考虑其他大模型 / 协议，也不建端点方言表。
//!
//! **本层不是兼容层，是解耦层**：agent 循环、上下文压缩、工具回灌都基于这些类型，
//! 让领域代码不认识线上 JSON 字段名、测试不必构造报文。所以「只做 DeepSeek」不等于
//! 把这层删掉直接拼 JSON —— 那会把 agent 绑死在字段名上。
//!
//! 各文件注释里残留的 Anthropic 对比（块数组、thinking 签名、`tool_use` 等）是
//! 2026-11-07~09 定形状时的**推导依据与历史记录**，不代表当前支持范围；对应实现留在
//! `protocol/anthropic/` 作为未接入链路的存档。专用化清理时一并收掉。
//!
//! # 一条消息按角色分开
//!
//! `Msg` 四个变体各包一个具名结构体：`SystemMsg` / `AssistantMsg` / `ToolMsg`
//! 的 `content` 是 `String`，只有 `UserMsg` 是 `Vec<Content>`；`AssistantMsg`
//! 另带 `tool_calls` 与 `thinking`，`ToolMsg` 另带 `call_id`。
//! 这与 DeepSeek 报文一致：`content` 与 `tool_calls` 平级、工具结果是独立消息、
//! 思考写平级 `reasoning_content`，每块都有确定落点。
//!
//! `Vec<Content>` 说「由哪几块组成」，`Content` 目前只有文本 —— 工具相关不是内容块，
//! 而是消息自己的字段。历史消息、一次请求、一轮结果都用这同一个 `Msg`，
//! 中间没有第二套「结果类型」。`Content` 保留为枚举是因为 `deepseek-flash` 支持 vision，
//! 加图片时只需加一个变体、不改消息结构。
//!
//! # 三条规矩
//!
//! 1. **只存语义，不存线上形状，也不存磁盘形状**。没有线上报文里的包裹层、
//!    没有「content 是标量还是数组」的判断 —— 形状由 `protocol/` 负责
//!    （user 单块压标量；assistant 的 `content` 恒为字符串、空正文写 `""` 且不省略键）。
//!    对话历史怎么落盘尚未设计，`types/` 整层不 derive `Serialize` / `Deserialize`。
//! 2. **明确丢弃要写清楚**。`ChatRequest::ThinkingMode`（当前一律不发）、
//!    `Thinking.signature` / `Delta::Signature`（DeepSeek 没有，编码时丢弃）、
//!    `ToolDef::Custom`（DeepSeek 不认）都是已定策略，不假装无损；
//!    这些是通用化残留，专用化清理时删。
//! 3. **按角色定形状，只有 user 需要 `Vec`**。正文就是一段字符串，流式累积分片
//!    直接 `push_str`，不必先判断形态。
//!
//! # 对外词汇
//!
//! - `Msg` / `SystemMsg` / `UserMsg` / `AssistantMsg` / `ToolMsg` /
//!   `Content` / `ToolCall` / `Thinking`:上层说「这一轮说了什么」;
//! - `ToolDef`:上层说「我们有哪些工具」。自身是按类型分派的枚举,变体名即类型,
//!   每种类型的专属字段直接写在变体里;
//! - `ChatResponse`:一次调用的产出 = 那条 `Msg` + 结束原因 + token 用量。
//!   消息本体只有 `Msg` 这一份,不重复、不转发,也没有第二套「结果消息」词汇;
//! - `Delta` / `StreamAccumulator`:流式的**一条增量**与**唯一收尾者**。
//!   没有「片」这个类型 —— 一个 SSE 事件翻译后直接交出 `Vec<Delta>`;
//!   收尾者被动接收每一项,不消费流,结束时产出同一个 `ChatResponse`。
//!
//! 消息经 `Msg` 的构造器创建:调用方只交解析出的原始值,由方法负责组装
//! (`assistant_with` 是带工具调用 / 思考的完整入口)。这是职责边界,不是
//! 可见性限制 —— 字段仍全 `pub`,因为没有不变式要护。
//!
//! **整层 `pub(crate)`**:`types/` 是承上启下的中间层,上下游(agent / services /
//! transport / protocol)都要认识它的词汇 —— 请求由调用方组装、增量由上层逐项
//! 推前端、结果交给 agent 分流。用 `pub(super)` 挡在 `llm` 内会让这层名存实亡。
//! `FinishReason` / `Usage` 定义在 `response.rs`,`stream.rs` 反向引用;
//! `StreamAccumulator` 字段仍私有(有状态、有组装规则),那是封装,不是可见性边界。
//!
//! **已接线**:agent / context / tools / services 都基于本层词汇,编解码与累加器
//! 既有生产调用点也有单元测试。下面两行 allow 等「专用化清理」收掉残留变体
//! (`Delta::Signature` / `Thinking.signature` / `ToolDef::Custom` / `ThinkingMode` 等)
//! 之后再删,现在硬删会刷一屏 dead_code 警告。
#![allow(dead_code)]
#![allow(unused_imports)]

mod message;
mod request;
mod response;
mod stream;
mod tool;

// ============ 全部词汇:整层 pub(crate),上下游都要认识 ============

pub(crate) use message::{
    AssistantMsg, Content, Msg, SystemMsg, Thinking, ToolCall, ToolMsg, UserMsg,
};
pub(crate) use request::{ChatRequest, Effort, ThinkingMode};
pub(crate) use response::{ChatResponse, FinishReason, Usage};
pub(crate) use stream::{Delta, StreamAccumulator};
pub(crate) use tool::ToolDef;
