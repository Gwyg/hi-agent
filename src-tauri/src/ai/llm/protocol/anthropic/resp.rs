//! 响应:SSE 每一帧 → 一片增量。
//!
//! 报文形状参照 Anthropic Messages 的 streaming 文档(message_start / content_block_delta / message_stop)。
//!
//! # 与 OpenAI 侧的三处不同
//!
//! 1. **没有 `[DONE]`**,靠 `message_stop` 结束;
//! 2. **帧里带 `type`**,而不是靠 `choices`/`delta` 的有无来推断;
//! 3. **`signature_delta` 单独成帧** —— 思考的签名与文本分两条事件到达,
//!    这里把它收成 `BlockDelta::Signature`,怎么合并等累积器定型。
//!
//! 调用方传进来的仍然是**一行 `data:` 的内容**(`event:` 行已被 transport 丢掉)。
//! 判定一律以 `data.type` 为准 —— 它和 `event:` 的值恒等,读一个就够。
//!
//! # struct 只活在这一层
//!
//! `Frame` / `BlockStart` 这些名字不许出现在 `anthropic` 之外。上层拿到的是
//! `types::Chunk`,增量统一是 `BlockDelta`。

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use super::super::Dialect;
use crate::ai::llm::types::{BlockDelta, Chunk, FinishReason, ToolCallDelta, Usage};

/// 一帧。用一个大结构体接住所有事件的字段,再按 `type` 分支取用 ——
/// 每种事件只用到其中两三个字段,拆成 enum 反而要写六份几乎相同的 struct。
#[derive(Deserialize)]
pub(super) struct Frame {
    #[serde(rename = "type")]
    pub kind: String,
    /// block 相关事件的块序号。工具调用靠它配对 `content_block_start` 与后续增量。
    pub index: Option<u32>,
    /// 仅 `message_start`。
    pub message: Option<MessageMeta>,
    /// 仅 `content_block_start`。
    pub content_block: Option<BlockStart>,
    /// `content_block_delta` / `message_delta`。
    pub delta: Option<RawDelta>,
    /// `message_start`(嵌在 message 里)/ `message_delta`(顶层)。
    pub usage: Option<RawUsage>,
    /// 仅 `error`。形状是 `{"type":"error","error":{...}}`,
    /// 所以这里接的是内层的 `error` 对象本身。
    pub error: Option<ErrorBody>,
}

#[derive(Deserialize)]
pub(super) struct MessageMeta {
    pub usage: Option<RawUsage>,
}

/// `content_block_start` 里的初始块。
///
/// 只有 `tool_use` 需要读 —— 它的 `id` / `name` **只在首帧给一次**,后面全是参数片段。
/// `text` / `thinking` 的首帧值是空串,没有可取的内容。
#[derive(Deserialize)]
pub(super) struct BlockStart {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: Option<String>,
    pub name: Option<String>,
}

/// 增量。字段按事件类型互斥,取哪个由 `delta.type` 决定。
///
/// 注意 `type` 是 `Option`:只有 `content_block_delta` 的 delta 带它,
/// `message_delta` 的 delta 只有 `{"stop_reason", "stop_sequence"}` 两个键。
#[derive(Deserialize)]
pub(super) struct RawDelta {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    /// `text_delta`
    pub text: Option<String>,
    /// `thinking_delta`
    pub thinking: Option<String>,
    /// `input_json_delta` —— 工具参数的 JSON **片段**,不是完整 JSON。
    pub partial_json: Option<String>,
    /// `signature_delta` —— 思考凭据,与 `thinking_delta` 分两路到达。
    pub signature: Option<String>,
    /// `message_delta` 的 `stop_reason`(与文本增量不在同一种 delta 里)。
    pub stop_reason: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct RawUsage {
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
}

#[derive(Deserialize)]
pub(super) struct ErrorBody {
    #[serde(rename = "type")]
    pub kind: String,
    pub message: Option<String>,
}

/// 一行 `data:` → 一片增量。
///
/// `dialect` 在读方向用不上:Anthropic 的字段名由格式固定,没有厂商变体。
/// 参数保留是为了与 `encode` 对称,以及将来真出现方言差异时不必改签名。
pub(super) fn decode(data: &str, _dialect: &Dialect) -> Result<Chunk> {
    let frame: Frame = serde_json::from_str(data)?;

    match frame.kind.as_str() {
        // 服务端报错必须转成 Err —— 当成正常帧会被「安静地」当成流结束,
        // 用户看到的是对话突然停了,而不是「服务过载,请重试」。
        "error" => {
            let body = frame.error.context("error 事件缺 error 字段")?;
            bail!(
                "Anthropic 返回错误 {}: {}",
                body.kind,
                body.message.unwrap_or_default()
            )
        }
        // 输入 token 在这一帧就给全了;输出 token 只有 1(占位),真实值在 message_delta。
        "message_start" => Ok(Chunk {
            delta: Vec::new(),
            finish_reason: None,
            usage: frame.message.and_then(|m| m.usage).map(to_usage),
        }),
        // 工具调用的 id / name 只在这里出现一次。
        "content_block_start" => Ok(Chunk {
            delta: block_start_delta(frame.index, frame.content_block),
            finish_reason: None,
            usage: None,
        }),
        "content_block_delta" => Ok(Chunk {
            delta: delta_of(frame.index, frame.delta),
            finish_reason: None,
            usage: None,
        }),
        // 结束原因与最终输出 token 同在这一帧。
        "message_delta" => {
            let stop = frame
                .delta
                .as_ref()
                .and_then(|d| d.stop_reason.clone())
                .map(parse_stop_reason);
            Ok(Chunk {
                delta: Vec::new(),
                finish_reason: stop,
                usage: frame.usage.map(to_usage),
            })
        }
        // ping(心跳)/ content_block_stop / message_stop:都不产生输出。
        _ => Ok(Chunk::default()),
    }
}

fn block_start_delta(index: Option<u32>, block: Option<BlockStart>) -> Vec<BlockDelta> {
    let (Some(index), Some(block)) = (index, block) else {
        return Vec::new();
    };
    // 文本块与思考块的首帧只有空串,没有内容可产出。
    if block.kind != "tool_use" {
        return Vec::new();
    }
    vec![BlockDelta::ToolCall(ToolCallDelta {
        index,
        id: non_empty(block.id),
        name: non_empty(block.name),
        arguments: None,
    })]
}

fn delta_of(index: Option<u32>, delta: Option<RawDelta>) -> Vec<BlockDelta> {
    let Some(delta) = delta else {
        return Vec::new();
    };
    let mut out = Vec::new();
    match delta.kind.as_deref() {
        Some("text_delta") => {
            if let Some(s) = non_empty(delta.text) {
                out.push(BlockDelta::Text(s));
            }
        }
        Some("thinking_delta") => {
            if let Some(s) = non_empty(delta.thinking) {
                out.push(BlockDelta::Thinking(s));
            }
        }
        Some("signature_delta") => {
            if let Some(s) = non_empty(delta.signature) {
                out.push(BlockDelta::Signature(s));
            }
        }
        // 参数片段必须带 index 才能拼回去;缺 index 的片段无处安放,丢弃。
        Some("input_json_delta") => {
            if let (Some(index), Some(partial)) = (index, non_empty(delta.partial_json)) {
                out.push(BlockDelta::ToolCall(ToolCallDelta {
                    index,
                    id: None,
                    name: None,
                    arguments: Some(partial),
                }));
            }
        }
        // citations_delta 等暂不支持的类型:忽略,不让它打断整条流。
        _ => {}
    }
    out
}

fn to_usage(raw: RawUsage) -> Usage {
    let input = raw.input_tokens.unwrap_or_default();
    let output = raw.output_tokens.unwrap_or_default();
    Usage {
        prompt_tokens: input,
        completion_tokens: output,
        total_tokens: input + output,
        // Anthropic 不单列思考 token,它计在 output_tokens 里。
        reasoning_tokens: None,
    }
}

/// `stop_reason` 取值见文档第 7 节。规范外的值降级成 `Other`,不让一条流失败。
fn parse_stop_reason(s: String) -> FinishReason {
    match s.as_str() {
        // stop_sequence 是命中停止词,pause_turn 是长任务中途暂停 —— 都属正常结束。
        "end_turn" | "stop_sequence" | "pause_turn" => FinishReason::Stop,
        "tool_use" => FinishReason::ToolCalls,
        "max_tokens" => FinishReason::Length,
        "refusal" => FinishReason::ContentFilter,
        _ => FinishReason::Other(s),
    }
}

/// 空串不是内容。`content_block_start` 的 `text` / `thinking` 首帧就是 `""`。
fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::llm::protocol::Protocol;

    fn d() -> Dialect {
        Dialect::for_model(Protocol::Anthropic, "claude")
    }

    fn decode(s: &str) -> Chunk {
        super::decode(s, &d()).unwrap()
    }

    /// 首帧只带 input_tokens,没有可展示内容。
    #[test]
    fn message_start_carries_input_tokens() {
        let c = decode(
            r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"usage":{"input_tokens":12,"output_tokens":1}}}"#,
        );
        assert!(!c.has_output());
        assert!(c.finish_reason.is_none());
        let u = c.usage.unwrap();
        assert_eq!(u.prompt_tokens, 12);
        assert_eq!(u.completion_tokens, 1);
    }

    /// 工具调用首帧:给 id 与 name,没有参数。
    #[test]
    fn tool_use_block_start_gives_id_and_name() {
        let c = decode(
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"bash","input":{}}}"#,
        );
        let tc = c.tool_calls().next().unwrap();
        assert_eq!(tc.index, 1);
        assert_eq!(tc.id.as_deref(), Some("toolu_1"));
        assert_eq!(tc.name.as_deref(), Some("bash"));
        assert_eq!(tc.arguments, None);
        assert!(!c.has_output());
    }

    /// 文本块首帧是空串,不该产出一个空增量。
    #[test]
    fn text_block_start_is_empty() {
        let c = decode(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        );
        assert!(!c.has_output());
        assert!(c.tool_calls().next().is_none());
    }

    /// 三种增量各走各的 `BlockDelta` 变体。
    #[test]
    fn three_delta_kinds() {
        let text = decode(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"结论"}}"#,
        );
        assert_eq!(text.text(), Some("结论"));

        let think = decode(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"要先"}}"#,
        );
        assert_eq!(think.thinking(), Some("要先"));

        let args = decode(
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"comm"}}"#,
        );
        let tc = args.tool_calls().next().unwrap();
        assert_eq!(tc.index, 1);
        assert_eq!(tc.arguments.as_deref(), Some(r#"{"comm"#));
        assert_eq!(tc.id, None);
    }

    /// 签名单独成帧 —— 它不进 thinking,而是留给累积器拼成凭据。
    #[test]
    fn signature_delta_is_separate_from_text() {
        let c = decode(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"EqQBCgIY"}}"#,
        );
        assert_eq!(c.signature(), Some("EqQBCgIY"));
        assert_eq!(c.thinking(), None);
        assert!(!c.has_output());
    }

    /// 结束帧:stop_reason 与输出 token 同时到达。
    #[test]
    fn message_delta_gives_stop_reason_and_output_tokens() {
        let c = decode(
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":34}}"#,
        );
        assert_eq!(c.finish_reason, Some(FinishReason::ToolCalls));
        let u = c.usage.unwrap();
        assert_eq!(u.completion_tokens, 34);
        // 这一帧没有 input_tokens —— 0 表示「未提供」,累积器按字段合并
        assert_eq!(u.prompt_tokens, 0);
    }

    /// 停止词结束算正常结束,不是错误。
    #[test]
    fn stop_sequence_is_a_normal_end() {
        let c = decode(r#"{"type":"message_delta","delta":{"stop_reason":"stop_sequence"}}"#);
        assert_eq!(c.finish_reason, Some(FinishReason::Stop));
    }

    /// 规范外的 stop_reason 只降级,不中断。
    #[test]
    fn unknown_stop_reason_is_tolerated() {
        let c = decode(
            r#"{"type":"message_delta","delta":{"stop_reason":"model_context_window_exceeded"}}"#,
        );
        assert_eq!(
            c.finish_reason,
            Some(FinishReason::Other("model_context_window_exceeded".into()))
        );
    }

    /// ping 与收尾帧都是空增量,不能报错。
    #[test]
    fn keep_alive_frames_are_empty() {
        for s in [
            r#"{"type":"ping"}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_stop"}"#,
        ] {
            let c = decode(s);
            assert!(!c.has_output(), "{s}");
            assert!(c.finish_reason.is_none(), "{s}");
        }
    }

    /// 服务端错误必须转成 Err —— 当成正常帧会让对话「安静地」停住。
    #[test]
    fn error_event_becomes_err() {
        let err = super::decode(
            r#"{"type":"error","error":{"type":"overloaded_error","message":"过载"}}"#,
            &d(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("overloaded_error"), "{err}");
        assert!(err.contains("过载"), "{err}");
    }

    /// 空串不是内容(与 OpenAI 侧同一条规矩)。
    #[test]
    fn empty_text_is_not_content() {
        let c = decode(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":""}}"#,
        );
        assert_eq!(c.text(), None);
    }
}
