//! `ChatRequest` → OpenAI Chat Completions 请求体。
//!
//! # 形状判定全在这一层
//!
//! 通用层只存语义，线上形状由这里一次决定：
//!
//! - `user.content` 是 `Vec<Content>`：一块压成标量字符串，多块走 part 数组；
//! - assistant 的 `content` 恒为字符串：空正文写 `""`、**不能省略键**；
//! - 思考是消息上的平级 `reasoning_content`；`signature` 不是 DeepSeek 概念，明确丢弃；
//! - `ToolDef` 的两个变体各拼自己的 wire 包裹层（`function` / `custom`）。
//!
//! # 按 DeepSeek 口径固定下来的取舍
//!
//! 本项目不做端点方言表，以下不是「待扩展」，就是最终决定：
//!
//! - `ThinkingMode`（`Auto` / `Off` / `On`）一律不发：OpenAI Chat Completions 标准里
//!   没有思考开关键，DeepSeek 虽另有私有开关（`thinking` / `reasoning_effort: "none"`），
//!   但本项目默认开思考、不暴露关思考能力，故不主动发；形状发错反而可能 400。
//! - `include_usage` 只在流式时推导成规范内的 `stream_options`（DeepSeek 要求
//!   `stream:true` 才能发这个键）；非流式响应本来就有 usage。
//! - **历史思考一律回传**：DeepSeek 把 `reasoning_content` 做成 load-bearing，工具循环里
//!   少回传一轮就会 400，必须原样带回，不做「只在工具轮次回传」这类收窄。
//! - `stream_options` / `max_tokens` 等顶层键按规范形状发，不为迁就其他端点加开关。
//! - assistant 的 `content` 不写 `null`：DeepSeek 官方「思考模式 + 工具调用」示例里
//!   只调工具那一轮是 `content: ''`，回传历史时 `messages.append(message)` 原样带回；
//!   不跟随 OpenAI 生态常见的 `null` 写法。响应侧的 `null` / `""` / 缺键解码时仍一律折成空串。

use serde_json::{json, Map, Value};

use crate::ai::llm::protocol::ProtocolError;
use crate::ai::llm::types::{ChatRequest, Content, Msg, ToolDef};

pub(super) fn encode(req: &ChatRequest) -> Result<String, ProtocolError> {
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    body.insert("messages".into(), Value::Array(messages(&req.messages)));
    body.insert("stream".into(), json!(req.stream));

    if let Some(n) = req.max_tokens {
        body.insert("max_tokens".into(), json!(n));
    }
    if let Some(t) = req.temperature {
        body.insert("temperature".into(), json!(t));
    }
    if let Some(e) = req.effort {
        body.insert("reasoning_effort".into(), json!(e.as_str()));
    }
    if req.stream && req.include_usage {
        body.insert("stream_options".into(), json!({ "include_usage": true }));
    }
    if !req.tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools(&req.tools)));
    }

    serde_json::to_string(&Value::Object(body)).map_err(ProtocolError::Serialize)
}

/// 一条中立消息 → 一条 OpenAI 消息。四种角色一一对应，没有消息树重建。
fn messages(msgs: &[Msg]) -> Vec<Value> {
    msgs.iter()
        .map(|m| match m {
            Msg::System(s) => json!({ "role": "system", "content": s.content }),
            Msg::User(u) => json!({ "role": "user", "content": user_content(&u.content) }),
            Msg::Assistant(a) => {
                let mut obj = Map::new();
                obj.insert("role".into(), json!("assistant"));
                obj.insert("content".into(), json!(a.content));

                if let Some(t) = &a.thinking {
                    if !t.text.is_empty() {
                        obj.insert("reasoning_content".into(), json!(t.text));
                    }
                }
                if !a.tool_calls.is_empty() {
                    let calls: Vec<Value> = a
                        .tool_calls
                        .iter()
                        .map(|c| {
                            json!({
                                "id": c.id,
                                "type": "function",
                                "function": {
                                    "name": c.name,
                                    "arguments": c.arguments,
                                },
                            })
                        })
                        .collect();
                    obj.insert("tool_calls".into(), Value::Array(calls));
                }
                Value::Object(obj)
            }
            Msg::Tool(t) => json!({
                "role": "tool",
                "tool_call_id": t.call_id,
                "content": t.content,
            }),
        })
        .collect()
}

/// 输出只有两态：多块 → part 数组，其余(0 / 1 块) → 标量字符串。
/// 0 块用空串兜住，不单开分支 —— `Vec` 允许为空，`UserMsg.content` 也是 `pub`。
/// 对 `Content` 的解构只在这里：将来加变体时，这个 `match` 会强制补分支。
fn user_content(parts: &[Content]) -> Value {
    let texts: Vec<&str> = parts
        .iter()
        .map(|c| match c {
            Content::Text { text } => text.as_str(),
        })
        .collect();
    if texts.len() > 1 {
        Value::Array(
            texts
                .iter()
                .map(|t| json!({ "type": "text", "text": t }))
                .collect(),
        )
    } else {
        json!(texts.first().copied().unwrap_or(""))
    }
}

/// 变体名即工具类型，包裹层键名在这里补，通用层不认识 `function` / `custom`。
fn tools(defs: &[ToolDef]) -> Vec<Value> {
    defs.iter()
        .map(|d| match d {
            ToolDef::Function {
                name,
                description,
                parameters,
            } => json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": description,
                    "parameters": parameters,
                },
            }),
            ToolDef::Custom {
                name,
                description,
                format,
            } => {
                let mut custom = Map::new();
                custom.insert("name".into(), json!(name));
                custom.insert("description".into(), json!(description));
                if let Some(f) = format {
                    custom.insert("format".into(), f.clone());
                }
                json!({ "type": "custom", "custom": Value::Object(custom) })
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::llm::types::{Effort, ToolCall};
    use serde_json::json;

    fn body(req: &ChatRequest) -> Value {
        let s = encode(req).unwrap();
        serde_json::from_str(&s).unwrap()
    }

    #[test]
    fn minimal_request_has_only_the_baseline_keys() {
        let v = body(&ChatRequest::new("m", vec![Msg::user("你好")]));
        assert_eq!(v["model"], "m");
        assert_eq!(v["stream"], false);
        assert_eq!(v["messages"][0]["role"], "user");
        assert_eq!(v["messages"][0]["content"], "你好");
        assert!(v.get("tools").is_none());
        assert!(v.get("max_tokens").is_none());
        assert!(v.get("temperature").is_none());
        assert!(v.get("stream_options").is_none());
    }

    /// 一块是标量，多块才是数组 —— 紧凑化落在编码层，不进通用层。
    #[test]
    fn user_content_scalar_for_one_block_array_for_many() {
        let one = body(&ChatRequest::new("m", vec![Msg::user("你好")]));
        assert_eq!(one["messages"][0]["content"], "你好");

        let many = body(&ChatRequest::new(
            "m",
            vec![Msg::user_content(vec![
                Content::text("第一段"),
                Content::text("第二段"),
            ])],
        ));
        assert_eq!(many["messages"][0]["content"][0]["type"], "text");
        assert_eq!(many["messages"][0]["content"][0]["text"], "第一段");
        assert_eq!(many["messages"][0]["content"][1]["text"], "第二段");
    }

    /// 0 块不该 panic：兜成空串标量，和 1 块同属「非数组」那一支。
    #[test]
    fn empty_user_content_falls_back_to_empty_string() {
        let v = body(&ChatRequest::new("m", vec![Msg::user_content(vec![])]));
        assert_eq!(v["messages"][0]["content"], "");
    }

    /// 带工具调用且正文为空：content 仍是空字符串，键不省略。
    /// DeepSeek 官方工具调用示例里这一轮就是 `content: ''`。
    #[test]
    fn assistant_with_only_tool_calls_writes_empty_content() {
        let v = body(&ChatRequest::new(
            "m",
            vec![Msg::assistant_with(
                "",
                vec![ToolCall::new("c1", "bash", r#"{"cmd":"ls"}"#)],
                None,
                None,
            )],
        ));
        assert_eq!(v["messages"][0]["content"], "");
        assert!(v["messages"][0].get("content").is_some());
        assert_eq!(v["messages"][0]["tool_calls"][0]["id"], "c1");
        assert_eq!(v["messages"][0]["tool_calls"][0]["type"], "function");
        assert_eq!(
            v["messages"][0]["tool_calls"][0]["function"]["arguments"],
            r#"{"cmd":"ls"}"#
        );
    }

    /// 思考是平级键，签名在这套协议里明确丢弃。
    #[test]
    fn thinking_is_a_sibling_key_and_signature_is_dropped() {
        let v = body(&ChatRequest::new(
            "m",
            vec![Msg::assistant_with(
                "结论",
                Vec::new(),
                Some("先看目录".into()),
                Some("sig".into()),
            )],
        ));
        assert_eq!(v["messages"][0]["content"], "结论");
        assert_eq!(v["messages"][0]["reasoning_content"], "先看目录");
        assert!(v["messages"][0].get("signature").is_none());
    }

    /// 四种角色各落到自己的 wire 形状，工具结果是独立消息。
    #[test]
    fn four_roles_map_one_to_one() {
        let v = body(&ChatRequest::new(
            "m",
            vec![
                Msg::system("你是助手"),
                Msg::user("看下目录"),
                Msg::assistant_with("", vec![ToolCall::new("c1", "bash", "{}")], None, None),
                Msg::tool("c1", "a.txt"),
            ],
        ));
        let msgs = v["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "c1");
        assert_eq!(msgs[3]["content"], "a.txt");
    }

    /// 两种工具声明各拼自己的包裹层，`format: None` 时不写这个键。
    #[test]
    fn tool_defs_keep_their_own_wire_wrapper() {
        let v = body(
            &ChatRequest::new("m", vec![Msg::user("x")]).with_tools(vec![
                ToolDef::function("bash", "跑命令", json!({ "type": "object" })),
                ToolDef::custom("writer", "写点东西", None),
            ]),
        );
        assert_eq!(v["tools"][0]["type"], "function");
        assert_eq!(v["tools"][0]["function"]["parameters"]["type"], "object");
        assert_eq!(v["tools"][1]["type"], "custom");
        assert_eq!(v["tools"][1]["custom"]["name"], "writer");
        assert!(v["tools"][1]["custom"].get("format").is_none());
    }

    /// `stream_options` 是规范内字段，只在流式且调用方要 usage 时出现。
    #[test]
    fn stream_options_follows_stream_and_include_usage() {
        let streaming = body(&ChatRequest::new("m", vec![Msg::user("x")]).with_stream(true));
        assert_eq!(streaming["stream_options"]["include_usage"], true);

        let no_usage = body(
            &ChatRequest::new("m", vec![Msg::user("x")])
                .with_stream(true)
                .with_include_usage(false),
        );
        assert!(no_usage.get("stream_options").is_none());

        let non_stream = body(&ChatRequest::new("m", vec![Msg::user("x")]));
        assert!(non_stream.get("stream_options").is_none());
    }

    /// 可选顶层字段按意图原样落键，`Effort` 的档位名不能改写。
    #[test]
    fn optional_top_level_fields_are_verbatim() {
        let v = body(
            &ChatRequest::new("m", vec![Msg::user("x")])
                .with_max_tokens(8192)
                .with_temperature(0.4)
                .with_effort(Effort::Max),
        );
        assert_eq!(v["max_tokens"], 8192);
        assert_eq!(v["temperature"], 0.4_f32);
        assert_eq!(v["reasoning_effort"], "max");
    }
}
