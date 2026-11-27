//! `ChatRequest` → Anthropic Messages 请求体。
//!
//! # 这里不是字段改名，是重建消息树
//!
//! 四处结构性改写，都不是 `#[serde(rename)]` 能解决的：
//!
//! 1. 没有 system 角色，所有系统提示摘到顶层 `system`；
//! 2. 没有 `role:"tool"`，工具结果折进 user 的 `tool_result` 块；
//! 3. `content` 恒为块数组，纯文本也要包成 `[{"type":"text",...}]`；
//! 4. `arguments` 字符串要 parse 成 `tool_use.input` 对象，失败必须报错。
//!
//! 通用层形状偏向 OpenAI，所以折叠集中在这一侧一次做完；反向（Anthropic →
//! 通用层）缺「块属于哪个角色」的信息，那也是当初按 OpenAI 定形状的理由。
//!
//! # 拼接约定
//!
//! 多个文本块一律**直接相接、不插分隔符**：这里体现为「各自成块」，
//! 与非流式 / 流式解码的相接规则一致。插字符等于偷偷改写模型输出。
//!
//! # 明确丢弃的字段
//!
//! - `ThinkingMode` / `Effort`：这套协议没有对应键，端点要开思考属于方言。
//!   项目已否决多厂商适配，本目录是存档实现，不再为它接入规则。
//! - `include_usage`：响应天然带 usage，丢弃此开关。
//! - `ToolDef::Custom`：本协议没有这个概念，原样发会 400，跳过。
//! - `Thinking.signature`：缺签名的 thinking 块会被服务端拒收，所以只在有签名时
//!   回传思考；没有签名则打 warn 并跳过该块（不阻塞整次请求）。

use serde_json::{json, Map, Value};

use crate::ai::llm::protocol::ProtocolError;
use crate::ai::llm::types::{ChatRequest, Content, Msg, ToolDef};

/// Anthropic 的 `max_tokens` 必填，而 `ChatRequest` 允许不显式指定。
const DEFAULT_MAX_TOKENS: u32 = 4096;

pub(super) fn encode(req: &ChatRequest) -> Result<String, ProtocolError> {
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    body.insert(
        "max_tokens".into(),
        json!(req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS)),
    );
    body.insert("stream".into(), json!(req.stream));
    if let Some(t) = req.temperature {
        body.insert("temperature".into(), json!(t));
    }

    // 多条 system 各自成块，不插入分隔符；没有 system 消息就不写这个键。
    let system: Vec<Value> = req
        .messages
        .iter()
        .filter_map(|m| match m {
            Msg::System(s) => Some(text_block(&s.content)),
            _ => None,
        })
        .collect();
    if !system.is_empty() {
        body.insert("system".into(), Value::Array(system));
    }

    body.insert("messages".into(), Value::Array(messages(&req.messages)?));

    let tools: Vec<Value> = req
        .tools
        .iter()
        .filter_map(|d| match d {
            ToolDef::Function {
                name,
                description,
                parameters,
            } => Some(json!({
                "name": name,
                "description": description,
                "input_schema": parameters.clone(),
            })),
            ToolDef::Custom { .. } => None,
        })
        .collect();
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }

    serde_json::to_string(&Value::Object(body)).map_err(ProtocolError::Serialize)
}

/// 重建消息树：一条中立 `Msg` 可能折进相邻消息，也可能被摘走（system）。
fn messages(msgs: &[Msg]) -> Result<Vec<Value>, ProtocolError> {
    let mut out: Vec<Map<String, Value>> = Vec::new();
    for m in msgs {
        match m {
            Msg::System(_) => {} // 已提到顶层
            Msg::User(u) => {
                let blocks: Vec<Value> = u.content.iter().map(|c| text_block(text_of(c))).collect();
                push_role(&mut out, "user", blocks);
            }
            Msg::Assistant(a) => {
                let mut blocks = Vec::new();
                if let Some(t) = &a.thinking {
                    match &t.signature {
                        Some(sig) => blocks.push(json!({
                            "type": "thinking",
                            "thinking": t.text,
                            "signature": sig,
                        })),
                        None => tracing::warn!(
                            "Anthropic 思考块缺签名，跳过回传（发过去会被服务端拒收）"
                        ),
                    }
                }
                if !a.content.is_empty() {
                    blocks.push(text_block(&a.content));
                }
                for c in &a.tool_calls {
                    let input: Value = serde_json::from_str(&c.arguments).map_err(|source| {
                        ProtocolError::InvalidToolArguments {
                            name: c.name.clone(),
                            source,
                        }
                    })?;
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": c.id,
                        "name": c.name,
                        "input": input,
                    }));
                }
                push_role(&mut out, "assistant", blocks);
            }
            Msg::Tool(t) => push_role(
                &mut out,
                "user",
                vec![json!({
                    "type": "tool_result",
                    "tool_use_id": t.call_id,
                    "content": t.content,
                })],
            ),
        }
    }
    Ok(out.into_iter().map(Value::Object).collect())
}

/// 协议要求 user / assistant 严格交替：相邻同角色合并成一条消息的多个块。
/// 空块列表整条丢弃（协议不接受空 `content` 数组）。
fn push_role(out: &mut Vec<Map<String, Value>>, role: &str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    if let Some(last) = out.last_mut() {
        if last.get("role").and_then(Value::as_str) == Some(role) {
            if let Some(Value::Array(content)) = last.get_mut("content") {
                content.extend(blocks);
                return;
            }
        }
    }
    let mut m = Map::new();
    m.insert("role".into(), json!(role));
    m.insert("content".into(), Value::Array(blocks));
    out.push(m);
}

fn text_block(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

fn text_of(c: &Content) -> &str {
    match c {
        Content::Text { text } => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::llm::types::ToolCall;
    use serde_json::json;

    fn body(req: &ChatRequest) -> Value {
        let s = encode(req).unwrap();
        serde_json::from_str(&s).unwrap()
    }

    #[test]
    fn system_is_hoisted_and_max_tokens_is_always_present() {
        let v = body(&ChatRequest::new(
            "claude",
            vec![Msg::system("你是助手"), Msg::user("你好")],
        ));
        assert_eq!(v["system"][0]["type"], "text");
        assert_eq!(v["system"][0]["text"], "你是助手");
        assert_eq!(v["max_tokens"], DEFAULT_MAX_TOKENS);
        assert_eq!(v["messages"].as_array().unwrap().len(), 1);
        assert_eq!(v["messages"][0]["role"], "user");

        let explicit =
            body(&ChatRequest::new("claude", vec![Msg::user("x")]).with_max_tokens(64000));
        assert_eq!(explicit["max_tokens"], 64000);
    }

    #[test]
    fn system_key_is_absent_when_there_is_no_system_message() {
        let v = body(&ChatRequest::new("claude", vec![Msg::user("x")]));
        assert!(v.get("system").is_none());
    }

    /// 多条 system 各自成块，不拼接、不插分隔符。
    #[test]
    fn multiple_system_messages_stay_separate_blocks() {
        let v = body(&ChatRequest::new(
            "claude",
            vec![Msg::system("第一条"), Msg::system("第二条"), Msg::user("x")],
        ));
        let blocks = v["system"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["text"], "第一条");
        assert_eq!(blocks[1]["text"], "第二条");
    }

    /// 纯文本也必须是块数组。
    #[test]
    fn content_is_always_a_block_array() {
        let v = body(&ChatRequest::new("claude", vec![Msg::user("看下目录")]));
        assert_eq!(v["messages"][0]["content"][0]["type"], "text");
        assert_eq!(v["messages"][0]["content"][0]["text"], "看下目录");
    }

    /// 工具轮次：assistant 带 tool_use（input 是对象），结果折进 user 的 tool_result。
    #[test]
    fn tool_call_and_result_fold_into_the_tree() {
        let v = body(&ChatRequest::new(
            "claude",
            vec![
                Msg::user("看下目录"),
                Msg::assistant_with(
                    "",
                    vec![ToolCall::new("toolu_1", "bash", r#"{"command":"ls"}"#)],
                    None,
                    None,
                ),
                Msg::tool("toolu_1", "a.txt"),
            ],
        ));
        let msgs = v["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
        assert_eq!(msgs[1]["content"][0]["id"], "toolu_1");
        assert_eq!(msgs[1]["content"][0]["input"]["command"], "ls");
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[2]["content"][0]["tool_use_id"], "toolu_1");
    }

    /// 连续多条工具结果并成一条 user 消息的多个块；相邻同角色也合并。
    #[test]
    fn adjacent_same_role_is_merged() {
        let v = body(&ChatRequest::new(
            "claude",
            vec![
                Msg::user("第一句"),
                Msg::user("第二句"),
                Msg::tool("c1", "ok1"),
                Msg::tool("c2", "ok2"),
            ],
        ));
        let msgs = v["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["content"].as_array().unwrap().len(), 4);
    }

    /// 带签名的思考块排在最前面；缺签名的块跳过而不是发一个会被拒收的块。
    #[test]
    fn thinking_needs_a_signature_to_be_replayed() {
        let with_sig = body(&ChatRequest::new(
            "claude",
            vec![Msg::assistant_with(
                "结论",
                Vec::new(),
                Some("先想".into()),
                Some("sig".into()),
            )],
        ));
        let blocks = with_sig["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[0]["thinking"], "先想");
        assert_eq!(blocks[0]["signature"], "sig");
        assert_eq!(blocks[1]["type"], "text");

        let without_sig = body(&ChatRequest::new(
            "claude",
            vec![Msg::assistant_with(
                "结论",
                Vec::new(),
                Some("先想".into()),
                None,
            )],
        ));
        let blocks = without_sig["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "text");
    }

    /// `input` 要对象：arguments 不是合法 JSON 必须报错，不能静默发空对象。
    #[test]
    fn invalid_arguments_is_an_error() {
        let req = ChatRequest::new(
            "claude",
            vec![Msg::assistant_with(
                "",
                vec![ToolCall::new("toolu_1", "bash", r#"{"command":"#)],
                None,
                None,
            )],
        );
        let err = match encode(&req) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("参数非法却编码成功了"),
        };
        assert!(err.contains("bash"), "错误信息应带工具名：{err}");
    }

    /// tools 少一层包裹；Custom 在本协议没有对应概念，跳过。
    #[test]
    fn tools_use_input_schema_and_skip_custom() {
        let v = body(
            &ChatRequest::new("claude", vec![Msg::user("x")]).with_tools(vec![
                ToolDef::function("bash", "跑命令", json!({ "type": "object" })),
                ToolDef::custom("writer", "写点东西", None),
            ]),
        );
        let tools = v["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "bash");
        assert_eq!(tools[0]["input_schema"]["type"], "object");
        assert!(tools[0].get("function").is_none());
    }

    /// 这几个开关在本协议没有落点：`stream` 保留，usage 开关与 effort 丢弃。
    #[test]
    fn switches_without_a_counterpart_are_dropped() {
        let v = body(
            &ChatRequest::new("claude", vec![Msg::user("x")])
                .with_stream(true)
                .with_include_usage(true)
                .with_effort(crate::ai::llm::types::Effort::High)
                .with_thinking(crate::ai::llm::types::ThinkingMode::On),
        );
        assert_eq!(v["stream"], true);
        assert!(v.get("stream_options").is_none());
        assert!(v.get("reasoning_effort").is_none());
        assert!(v.get("thinking").is_none());
    }
}
