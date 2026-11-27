//! DeepSeek Chat Completions：本项目当前唯一接入的报文形状。
//!
//! 门面只放对外入口，实现分两半：`req.rs` 编码、`resp.rs` 解码。
//! 两者互不共享翻译逻辑，各自直接对着线上 JSON 写。

mod req;
mod resp;

use crate::ai::llm::protocol::ProtocolError;
use crate::ai::llm::types::{ChatRequest, ChatResponse, Delta};

pub(super) fn encode(req: &ChatRequest) -> Result<String, ProtocolError> {
    req::encode(req)
}

pub(super) fn decode_full(body: &str) -> Result<ChatResponse, ProtocolError> {
    resp::decode_full(body)
}

pub(super) fn decode_frame(data: &str) -> Result<Vec<Delta>, ProtocolError> {
    resp::decode_frame(data)
}
