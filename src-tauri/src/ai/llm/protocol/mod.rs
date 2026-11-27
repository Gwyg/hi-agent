//! 协议转化层：中立形状 ↔ 线上报文。
//!
//! # 范围（2026-11-10 定）
//!
//! 本项目**只接 DeepSeek 的 OpenAI 兼容端点**，不做多模型 / 多厂商适配。各家私有扩展
//! （思考字段形状、历史思考回传规则、顶层额外键、工具与长度键）没有统一规范，也无法
//! 从请求或配置推断，只能靠持续维护一张会过期的表；这个成本与「一次只配一个端点」的
//! 桌面应用不匹配。因此**不建 `rules/` 端点方言表、不做 (端点, 模型) 匹配**，形状差异
//! 按 DeepSeek 口径在 `openai/req.rs` 里一次写死。
//!
//! `anthropic/` 是一份完整但未接入链路的协议实现（连请求侧分派也没有），
//! 保留它不增加维护成本，也不为它继续投入。
//!
//! # 本层已完整（2026-11-10）
//!
//! - **请求侧**：`encode(&ChatRequest) -> Result<String, ProtocolError>`，规则在编码期
//!   生效，直接出可当 HTTP body 的字符串。
//! - **响应侧**：`decode_full(&str) -> ChatResponse` 与
//!   `decode_frame(&str) -> Vec<Delta>`，只认 DeepSeek 形状。
//! - **三个入口都是自由函数，没有协议枚举**：本项目只有一个协议，加一个只有
//!   一个有效取值的枚举再 `match` 分派，只是给未来留空壳。
//! - 两端都是纯翻译：不碰 HTTP、不持状态、可脱离网络单测。发字节 / 切 SSE 帧
//!   归 `llm/deepseek.rs`（无状态函数，不另建 transport 模块）。
//!
//! # 分层
//!
//! 目录按**协议**分，是因为换协议要整棵重建消息树；端点差异只是同一套结构里的
//! 若干取值，不构成协议的子类，所以不放在协议目录下。
//! `anthropic/resp.rs` 仍是上一版存档，未声明、不参与编译；`openai/resp.rs`
//! 已按当前形状重写并声明。
#![allow(dead_code)]
mod anthropic;
mod openai;

use std::fmt;

use crate::ai::llm::types::{ChatRequest, ChatResponse, Delta};

/// 编解码失败。请求侧两种、响应侧三种，都是真实会发生的路径。
#[derive(Debug)]
pub(crate) enum ProtocolError {
    /// Anthropic 的 `tool_use.input` 要对象，历史 `arguments` 原文却不是合法 JSON。
    InvalidToolArguments {
        name: String,
        source: serde_json::Error,
    },
    /// 最终 `Value` 转字符串失败。
    Serialize(serde_json::Error),
    /// 响应体 / SSE 帧不是合法 JSON。
    Parse(serde_json::Error),
    /// 响应缺关键字段（如非流式没有 `choices[0]`）。
    Missing(&'static str),
    /// 上游在 200 body 或 SSE 帧里返回 `error`。**保留可读原文**，
    /// 不建端点方言表之后这是用户判断该怎么改配置的唯一依据。
    Upstream(String),
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidToolArguments { name, source } => {
                write!(f, "工具 `{name}` 的 arguments 不是合法 JSON：{source}")
            }
            Self::Serialize(source) => write!(f, "请求体序列化失败：{source}"),
            Self::Parse(source) => write!(f, "响应解析失败：{source}"),
            Self::Missing(field) => write!(f, "响应缺少字段：{field}"),
            Self::Upstream(text) => write!(f, "上游返回错误：{text}"),
        }
    }
}

impl std::error::Error for ProtocolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidToolArguments { source, .. }
            | Self::Serialize(source)
            | Self::Parse(source) => Some(source),
            Self::Missing(_) | Self::Upstream(_) => None,
        }
    }
}

/// 中立请求 → 可直接作为 HTTP body 的 JSON 字符串。
///
/// 对外唯一入口，外面不直接摸 `openai::`。形状差异已按 DeepSeek 口径写死在
/// `openai/req.rs`，不需要调用方传一个协议枚举再 `match` 分派。
pub(crate) fn encode(req: &ChatRequest) -> Result<String, ProtocolError> {
    openai::encode(req)
}

/// 非流式完整响应 → 一次调用的产出。
pub(crate) fn decode_full(body: &str) -> Result<ChatResponse, ProtocolError> {
    openai::decode_full(body)
}

/// 流式单帧 `data` → 这一帧带来的所有增量。
/// `[DONE]` 由调用方判掉，错误帧在这里转 `Err`，不额外包一层「解码结果」枚举。
pub(crate) fn decode_frame(data: &str) -> Result<Vec<Delta>, ProtocolError> {
    openai::decode_frame(data)
}
