//! DeepSeek Chat Completions：响应侧解码。
//!
//! 两个纯函数，都不碰 HTTP、不持状态，输入是**一段完整的 JSON 文本**：
//!
//! - [`decode_full`]：非流式响应 `&str` → [`ChatResponse`]；
//! - [`decode_frame`]：流式一帧 `data` → `Vec<Delta>`（一帧可能同时带正文 / 思考 /
//!   工具调用分片 / 结束原因 / 用量，所以返回的是多条而不是一条）。
//!
//! 流式收尾仍是 `types/stream.rs` 的 `StreamAccumulator`：上层在同一个循环里
//! 一边把 `Delta` 逐项推前端、一边 `push`，`[DONE]` 时 `finish()`。结束是动作、
//! 不是增量，所以 `[DONE]` 不产生 `Delta`，由调用方 break。
//!
//! # 不建线上形状的结构体
//!
//! 响应先解成一棵 `Value`，再由两个函数**直接按 `types/` 的模型取值组装**：
//! `Msg::assistant_with` / `ToolCall::new` / `Usage` / `Delta` 都是终点，中间没有
//! 一层 `RawMessage` → `Message` 的搬运。给中立层加字段，就是在这里加一行取值；
//! 端点加了新字段，则是天然忽略（和 `deny_unknown_fields` 放开等价）。
//!
//! 取值口径统一由下面几个小 helper 兜住：**缺键 / `null` / 空串 / 类型不符都算
//! 「没有」**，不让端点多给一个字段或换一种写法就把整条响应解析挂掉。唯一会
//! 报错的是三件真错误：不是合法 JSON（`Parse`）、非流式没有 `choices[0]`
//! （`Missing`）、上游在 200 里塞了 `error`（`Upstream`）。
//! 代价是「字段类型不对」不会报错而是退化为缺省值 —— 这是只接一个端点、
//! 又要灵活好维护的主动取舍，不是漏判。
//!
//! # DeepSeek 相对标准 OpenAI 的几处形状
//!
//! - `reasoning_content` 与 `content` 平级，出现在 `message` / `delta` 上。它是
//!   **带 `tools` 的请求里必须逐轮原样回传**的字段（否则上游 400），所以解码时
//!   必须完整收进 `Thinking`，不能丢、也不能并进正文。
//! - `usage.prompt_cache_hit_tokens` 是输入缓存命中量，属于 `prompt_tokens` 的
//!   子集；`completion_tokens_details.reasoning_tokens` 属于 `completion_tokens`
//!   的子集。两者都不额外累加（口径见 `types/response.rs`）。
//! - `finish_reason` 除规范值外还可能是 `insufficient_system_resource` / `aborted`，
//!   一律落 `FinishReason::Other`，由上层决定怎么处理 —— 一个状态值不该让整条
//!   响应解析失败。
//! - 上游也可能在 200 的 body / SSE 帧里塞 `error` 对象，这里转成
//!   `ProtocolError::Upstream`，不静默吞掉。

use serde_json::Value;
use tracing::warn;

use crate::ai::llm::protocol::ProtocolError;
use crate::ai::llm::types::{ChatResponse, Delta, FinishReason, Msg, ToolCall, Usage};

/// 缺键 / `null` 时统一落到这个值，调用方就不必到处判 `Option<&Value>`。
static NULL: Value = Value::Null;

/// 非流式完整响应 → 一次调用的产出。
pub(super) fn decode_full(body: &str) -> Result<ChatResponse, ProtocolError> {
    let raw: Value = serde_json::from_str(body).map_err(ProtocolError::Parse)?;
    if let Some(e) = error_of(&raw) {
        return Err(ProtocolError::Upstream(upstream_text(e)));
    }

    // 一次调用只有一个回复，只看 choices[0]；这是全文件唯一的硬结构要求。
    let choice = raw
        .pointer("/choices/0")
        .ok_or(ProtocolError::Missing("choices[0]"))?;
    let message = at(choice, "message");

    let (content, refusal_only) = merge_refusal(text(message, "content"), text(message, "refusal"));
    if refusal_only {
        warn!("DeepSeek 返回 refusal 且 content 为空，已折进正文");
    }

    let tool_calls = array(message, "tool_calls")
        .iter()
        .map(|c| {
            let f = at(c, "function");
            ToolCall::new(
                text(c, "id").unwrap_or_default(),
                text(f, "name").unwrap_or_default(),
                text(f, "arguments").unwrap_or_default(),
            )
        })
        .collect();

    Ok(ChatResponse {
        message: Msg::assistant_with(
            content,
            tool_calls,
            text(message, "reasoning_content"),
            None,
        ),
        finish_reason: finish_reason(text(choice, "finish_reason")),
        usage: usage(at(&raw, "usage")),
    })
}

/// 流式单帧 `data` → 这一帧带来的所有增量。
///
/// `[DONE]` 由调用方在交给本函数前判掉；这里只保证万一传进来也不会被当成
/// 非法 JSON 报错。空 `data` / 纯注释同样返回空集合。
pub(super) fn decode_frame(data: &str) -> Result<Vec<Delta>, ProtocolError> {
    let data = data.trim();
    if data.is_empty() || data == "[DONE]" || data.starts_with(':') {
        return Ok(Vec::new());
    }

    let frame: Value = serde_json::from_str(data).map_err(ProtocolError::Parse)?;
    if let Some(e) = error_of(&frame) {
        return Err(ProtocolError::Upstream(upstream_text(e)));
    }

    let mut out = Vec::new();
    for choice in array(&frame, "choices") {
        let delta = at(choice, "delta");
        // 思考排在正文前:正常流里思考先输出完才轮到正文,万一同一帧两者并存,
        // 前端按事件顺序拼时间线也不会出现「先结论后思考」。
        if let Some(s) = text(delta, "reasoning_content") {
            out.push(Delta::Thinking(s));
        }
        if let Some(s) = text(delta, "content") {
            out.push(Delta::Text(s));
        }
        // refusal 与 content 互斥但可同时为空；它是同一个文本槽位，折进正文，
        // 让收尾照常产出 Msg::assistant(text)，types/ 不为此加字段。
        if let Some(r) = text(delta, "refusal") {
            warn!("DeepSeek 流式返回 refusal，已折进正文");
            out.push(Delta::Text(r));
        }
        for call in array(delta, "tool_calls") {
            let f = at(call, "function");
            out.push(Delta::ToolCall {
                // 同一调用跨多片靠它归组；规范要求必带，缺省兜 0。
                index: count(call, "index"),
                id: text(call, "id"),
                name: text(f, "name"),
                arguments: text(f, "arguments").unwrap_or_default(),
            });
        }
        if let Some(reason) = text(choice, "finish_reason") {
            out.push(Delta::Finish(finish_reason(Some(reason))));
        }
    }
    // usage 在最后一个带 finish_reason 的帧里（没有单独的 usage-only 帧），
    // 但 choices 为空的纯用量帧也要接住，不能因为循环没跑就丢。
    if let Some(u) = usage(at(&frame, "usage")) {
        out.push(Delta::Usage(u));
    }
    Ok(out)
}

// ============ 取值 helper：全部「线上形状」知识都在这里 ============

/// 取子字段。缺键、`null`、父节点不是对象，都返回同一个空值。
fn at<'a>(v: &'a Value, key: &str) -> &'a Value {
    v.get(key).filter(|child| !child.is_null()).unwrap_or(&NULL)
}

/// 取字符串字段。空串按「没有」处理，和 `types/` 里「空正文 = 空串」对齐。
fn text(v: &Value, key: &str) -> Option<String> {
    at(v, key)
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// 取计数 / 下标字段。负数、浮点、超出 `u32` 都当 0，不截断出脏值。
fn count(v: &Value, key: &str) -> u32 {
    at(v, key)
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(0)
}

/// 取数组字段。不是数组就当空数组，流式帧里 `delta` / `tool_calls` 常缺省。
fn array<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    at(v, key).as_array().map(Vec::as_slice).unwrap_or(&[])
}

/// `error: null` 不是错误；只有真的给了对象 / 文本才算上游报错。
fn error_of(raw: &Value) -> Option<&Value> {
    raw.get("error").filter(|e| !e.is_null())
}

// ============ 归一化 ============

/// `refusal` 与 `content` 互斥、也可能都为空；返回（正文，是否只有 refusal）。
fn merge_refusal(content: Option<String>, refusal: Option<String>) -> (String, bool) {
    match (content, refusal) {
        (Some(c), _) => (c, false),
        (None, Some(r)) => (r, true),
        (None, None) => (String::new(), false),
    }
}

fn finish_reason(raw: Option<String>) -> FinishReason {
    match raw.as_deref() {
        None => FinishReason::Unknown,
        Some("stop") => FinishReason::Stop,
        Some("tool_calls") => FinishReason::ToolCalls,
        Some("length") => FinishReason::Length,
        Some("content_filter") => FinishReason::ContentFilter,
        Some(other) => FinishReason::Other(other.to_string()),
    }
}

/// 全 0 / 全缺省视为「没给用量」，不编造 `Some(Usage::default())`。
fn usage(v: &Value) -> Option<Usage> {
    let u = Usage {
        input_tokens: count(v, "prompt_tokens"),
        output_tokens: count(v, "completion_tokens"),
        reasoning_tokens: count(at(v, "completion_tokens_details"), "reasoning_tokens"),
        cached_tokens: count(v, "prompt_cache_hit_tokens"),
    };
    (u != Usage::default()).then_some(u)
}

/// 把 error 对象拼成可读原文，不建端点方言表之后这是用户唯一的自救依据。
fn upstream_text(e: &Value) -> String {
    let mut parts = Vec::new();
    for key in ["type", "code", "message"] {
        if let Some(v) = e.get(key).filter(|v| !v.is_null()) {
            let value = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            parts.push(format!("{key}={value}"));
        }
    }
    if parts.is_empty() {
        "上游返回 error 帧".to_string()
    } else {
        parts.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::llm::protocol;
    use crate::ai::llm::types::StreamAccumulator;

    fn full(body: &str) -> ChatResponse {
        protocol::decode_full(body).unwrap()
    }

    fn frame(data: &str) -> Vec<Delta> {
        protocol::decode_frame(data).unwrap()
    }

    /// 非流式完整报文：思考、正文、工具调用、用量、结束原因各归其位。
    /// 报文里 id / object / model / index / type / total_tokens 都是不读的字段。
    #[test]
    fn decode_full_keeps_reasoning_separate_from_content() {
        let body = r#"{
            "id":"chat-1","object":"chat.completion","model":"deepseek-flash",
            "choices":[{
                "index":0,
                "message":{
                    "role":"assistant",
                    "content":"已改好",
                    "reasoning_content":"先看目录结构",
                    "tool_calls":[{"id":"c1","type":"function","function":{
                        "name":"read","arguments":"{\"path\":\"a.rs\"}"}}]
                },
                "finish_reason":"tool_calls"
            }],
            "usage":{"prompt_tokens":120,"completion_tokens":34,
                     "prompt_cache_hit_tokens":80,
                     "completion_tokens_details":{"reasoning_tokens":19},
                     "total_tokens":154}
        }"#;
        let resp = full(body);
        let Msg::Assistant(m) = &resp.message else {
            panic!("expected assistant");
        };
        assert_eq!(m.content, "已改好");
        assert_eq!(m.thinking.as_ref().unwrap().text, "先看目录结构");
        assert_eq!(
            m.tool_calls,
            vec![ToolCall::new("c1", "read", r#"{"path":"a.rs"}"#)]
        );
        assert_eq!(resp.finish_reason, FinishReason::ToolCalls);
        assert_eq!(
            resp.usage,
            Some(Usage {
                input_tokens: 120,
                output_tokens: 34,
                reasoning_tokens: 19,
                cached_tokens: 80,
            })
        );
        // 两个细分字段是主数的子集，总量不能重复累加。
        assert_eq!(resp.usage.unwrap().total_tokens(), 154);
    }

    #[test]
    fn decode_full_folds_refusal_into_text() {
        let resp = full(
            r#"{"choices":[{"message":{"role":"assistant","content":null,
                 "refusal":"不能回答"},"finish_reason":"stop"}],"usage":null}"#,
        );
        let Msg::Assistant(m) = &resp.message else {
            panic!("expected assistant");
        };
        assert_eq!(m.content, "不能回答");
        assert_eq!(resp.finish_reason, FinishReason::Stop);
    }

    #[test]
    fn decode_full_missing_finish_becomes_unknown() {
        let resp = full(r#"{"choices":[{"message":{"content":"好"}}]}"#);
        assert_eq!(resp.finish_reason, FinishReason::Unknown);
        assert_eq!(resp.usage, None);
    }

    #[test]
    fn decode_full_unknown_finish_value_is_other_not_an_error() {
        let resp = full(
            r#"{"choices":[{"message":{"content":"x"},
                 "finish_reason":"insufficient_system_resource"}]}"#,
        );
        assert_eq!(
            resp.finish_reason,
            FinishReason::Other("insufficient_system_resource".into())
        );
    }

    #[test]
    fn decode_full_upstream_error_is_not_swallowed() {
        let err = protocol::decode_full(
            r#"{"error":{"message":"余额不足","type":"insufficient_quota","code":"402"}}"#,
        )
        .unwrap_err();
        assert!(matches!(err, ProtocolError::Upstream(ref s) if s.contains("余额不足")));
    }

    /// `error: null` 是端点常见的占位写法，不能被误判成上游报错。
    #[test]
    fn null_error_is_not_an_error() {
        let resp = full(r#"{"error":null,"choices":[{"message":{"content":"好"}}]}"#);
        let Msg::Assistant(m) = &resp.message else {
            panic!("expected assistant");
        };
        assert_eq!(m.content, "好");
    }

    /// 类型不符 / 缺键一律退化为缺省，不让端点多加一个字段就整条解析失败。
    #[test]
    fn unexpected_shapes_fall_back_to_defaults() {
        let resp = full(
            r#"{"choices":[{"message":{"content":42,"refusal":null,
                 "reasoning_content":false,"tool_calls":"not-an-array"},
                 "finish_reason":null}],
                 "usage":{"prompt_tokens":"oops","completion_tokens":7}}"#,
        );
        let Msg::Assistant(m) = &resp.message else {
            panic!("expected assistant");
        };
        assert_eq!(m.content, "");
        assert!(m.thinking.is_none());
        assert!(m.tool_calls.is_empty());
        assert_eq!(resp.finish_reason, FinishReason::Unknown);
        assert_eq!(
            resp.usage,
            Some(Usage {
                output_tokens: 7,
                ..Default::default()
            })
        );
    }

    #[test]
    fn decode_full_without_choices_is_a_missing_field_error() {
        assert!(matches!(
            protocol::decode_full(r#"{"choices":[]}"#),
            Err(ProtocolError::Missing("choices[0]"))
        ));
        assert!(matches!(
            protocol::decode_frame("not json"),
            Err(ProtocolError::Parse(_))
        ));
    }

    /// 一帧里正文、思考、工具调用分片并存 —— 这是返回 Vec 而不是单个 Delta 的原因。
    #[test]
    fn decode_frame_emits_every_parallel_delta() {
        let deltas = frame(
            r#"{"choices":[{"index":0,"delta":{"content":"正在","reasoning_content":"先查","tool_calls":[{"index":0,"id":"c1","function":{"name":"bash","arguments":"{\"cmd\""}}]},"finish_reason":null}]}"#,
        );
        assert_eq!(deltas.len(), 3);
        assert!(deltas.contains(&Delta::Text("正在".into())));
        assert!(deltas.contains(&Delta::Thinking("先查".into())));
        assert!(deltas.contains(&Delta::ToolCall {
            index: 0,
            id: Some("c1".into()),
            name: Some("bash".into()),
            arguments: "{\"cmd\"".into(),
        }));
    }

    #[test]
    fn decode_frame_folds_refusal_and_maps_finish() {
        let deltas = frame(
            r#"{"choices":[{"delta":{"refusal":"拒答"},
                 "finish_reason":"stop"}]}"#,
        );
        assert_eq!(
            deltas,
            vec![
                Delta::Text("拒答".into()),
                Delta::Finish(FinishReason::Stop)
            ]
        );
    }

    /// choices 为空的纯用量帧不能丢；全 0 用量帧不编造 Some。
    #[test]
    fn decode_frame_collects_usage_only_frame() {
        let deltas = frame(
            r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,
                 "prompt_cache_hit_tokens":4}}"#,
        );
        assert_eq!(
            deltas,
            vec![Delta::Usage(Usage {
                input_tokens: 10,
                output_tokens: 5,
                reasoning_tokens: 0,
                cached_tokens: 4,
            })]
        );
        assert!(frame(r#"{"choices":[],"usage":{}}"#).is_empty());
    }

    #[test]
    fn decode_frame_ignores_done_and_empty_data() {
        assert!(frame("[DONE]").is_empty());
        assert!(frame("").is_empty());
        assert!(frame(": ping").is_empty());
    }

    #[test]
    fn decode_frame_upstream_error_becomes_err() {
        assert!(matches!(
            protocol::decode_frame(r#"{"error":{"message":"rate limited"}}"#),
            Err(ProtocolError::Upstream(_))
        ));
    }

    /// 端到端：逐帧解码 → 同一批 Delta 推给累积器 → 完整响应。
    /// 这是流式与非流式口径是否一致的钉子。
    #[test]
    fn frames_feed_accumulator_into_the_same_response() {
        let mut acc = StreamAccumulator::default();
        for data in [
            r#"{"choices":[{"delta":{"reasoning_content":"先看"},"finish_reason":null}]}"#,
            r#"{"choices":[{"delta":{"content":"结论"},"finish_reason":null}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"c2","function":{"name":"write","arguments":"{\"p\""}}]}},{"finish_reason":null}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"bash","arguments":"{\"cmd\":\"ls\"}"}}]}},{"finish_reason":null}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":":1}"}}]}},{"finish_reason":null}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":20,"completion_tokens":9}}"#,
        ] {
            for delta in frame(data) {
                acc.push(delta);
            }
        }
        let resp = acc.finish();
        let Msg::Assistant(m) = &resp.message else {
            panic!("expected assistant");
        };
        assert_eq!(m.content, "结论");
        assert_eq!(m.thinking.as_ref().unwrap().text, "先看");
        assert_eq!(
            m.tool_calls,
            vec![
                ToolCall::new("c1", "bash", r#"{"cmd":"ls"}"#),
                ToolCall::new("c2", "write", r#"{"p":1}"#),
            ]
        );
        assert_eq!(resp.finish_reason, FinishReason::ToolCalls);
        assert_eq!(resp.usage.unwrap().total_tokens(), 29);
    }
}
