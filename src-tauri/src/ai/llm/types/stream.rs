//! 流式响应:**一条增量 + 唯一一个收尾者**。
//!
//! # 为什么没有「片」这个类型
//!
//! 一个 SSE 事件翻译后可能产出多条增量(OpenAI 一帧里正文 / 思考 / 工具调用
//! 可以并存),也可能只产出一条(Anthropic 一帧一件事)。「一片」既不是稳定的
//! 数据单位,也不是累积单位 —— 帧边界由**翻译函数的调用边界**表达就够了,
//! 再包一层结构体只是把控制流信息编码进数据结构。所以本文件不定义 `Chunk`:
//! 协议层直接交出 `Vec<Delta>`,收尾者逐项接收。
//!
//! # 两个概念,两件事
//!
//! - `Delta` 是**过程数据**:往这条 assistant 消息的某个槽位追加了什么。
//!   它不进历史、不进 `ChatResponse`,也不带 `role` —— 流式里只有模型在输出,
//!   不会冒出 system / user / tool 消息。
//! - `StreamAccumulator` 是**唯一收集者**,且**不消费流**:它被动接收每一项,
//!   不遍历、不缓冲整条流。上层在同一个循环里一边把 `Delta` 逐项推前端,
//!   一边 `push` 给它;流结束时由传输层调 `finish()` 收成 `ChatResponse`。
//!   这样「逐 token 渲染」和「收成完整响应」是同一批数据的两个出口,
//!   不需要第二次遍历,也不需要上层自己懂 `index` 归组 / 跨帧合并。
//!
//! # 归谁管
//!
//! - **结束是动作,不是数据**:`[DONE]` / `message_stop` / EOF 不产生 `Delta`,
//!   只触发 `finish()`。错误帧由协议层转 `Err`,心跳与块边界事件直接丢弃。
//! - **`refusal` 不单列变体**:按已定约定,协议层 decode 把拒答片折成
//!   `Delta::Text` 并在该处打 warn,收尾照常 `Msg::assistant(text)`。
//! - **`FinishReason` / `Usage` 是响应侧共用词汇**,定义在 `response.rs`,
//!   本文件反向借用。
//! - **协议层的 `index` 不越过这一层**:`Delta::ToolCall.index` 只在累积过程里
//!   用来归组,`ChatResponse` 里没有「块」这个概念。Anthropic 的
//!   `index → 块类型` 映射属于协议层内部状态,不抬进通用层。
//!
//! # 几处拼接约定
//!
//! 一轮一段思考:思考文本逐片拼接,签名(可能晚到、独立成帧)也按到达顺序拼接,
//! 两者最后一起交给 `Msg::assistant_with`;**没有思考文本时签名没有落点,
//! 由 `assistant_with` 明确丢弃**。多个正文 / 思考块一律直接相接、不插分隔符 ——
//! 块边界在协议层就被丢掉了,插字符等于偷偷改写模型输出。
//! `Usage` 按字段非零覆盖合并(`0` = 没单列 / 没提供),不能整份替换。
//!
//! 与 `types/` 整层一致:不 derive `Serialize` / `Deserialize`,不加转发型访问器。
//! `Usage::total_tokens()` 是唯一的计算方法 —— 总量口径归 `Usage` 自己,
//! 不让调用方各自相加。整层 `pub(crate)`:中间层承上启下,上下游都要认识。

use std::collections::BTreeMap;

use super::message::{Msg, ToolCall};
use super::response::{ChatResponse, FinishReason, Usage};

/// 一条增量:往这条 assistant 消息的某个槽位追加了什么。
#[derive(Debug, Clone, PartialEq)]
pub enum Delta {
    /// 正文片段。拒答文本已由协议层折到这里,不单列。
    Text(String),
    /// 思考片段。
    Thinking(String),
    /// 思考签名片段(Anthropic 的 `signature_delta`),可能比思考文本晚到。
    Signature(String),
    /// 工具调用分片:首片给 `id` / `name`,`arguments` 逐片拼接。
    ToolCall {
        /// 聚合键:同一调用跨多片靠它归组。**只在过程里用**,不进最终结果。
        index: u32,
        id: Option<String>,
        name: Option<String>,
        /// 这一片没有参数就是空串 —— 与 `AssistantMsg.content` 一样,
        /// 空 = 空串,不用 `Option`。
        arguments: String,
    },
    /// 结束原因。后到覆盖先到;整条流都没给时由 `finish()` 归一。
    Finish(FinishReason),
    /// token 用量,可能是「部分填充」的一半:Anthropic 的 input 在
    /// `message_start`、output 在 `message_delta`,两处各带一半。
    Usage(Usage),
}

/// 按 `index` 归组中的半成品工具调用。累积器私有,不对外。
#[derive(Debug, Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// 唯一收尾者:把一串不完整的增量收成一次调用的完整产出。
///
/// 字段私有是因为它**有状态、有组装规则**,和 `Msg` / `ChatResponse` 那种
/// 「纯数据、没有不变式要护」的形状不是一回事。对外只有 `push` 与 `finish`。
#[derive(Debug, Default)]
pub struct StreamAccumulator {
    text: String,
    thinking: String,
    signature: Option<String>,
    /// `BTreeMap` 让收尾时天然按原下标升序输出,工具结果回灌顺序不会乱。
    tool_calls: BTreeMap<u32, PartialToolCall>,
    finish_reason: Option<FinishReason>,
    usage: Option<Usage>,
}

impl StreamAccumulator {
    /// 接收一条增量。上层推前端与这里共用同一个 `Delta`,不产生第二次遍历。
    pub fn push(&mut self, delta: Delta) {
        match delta {
            Delta::Text(s) => self.text.push_str(&s),
            Delta::Thinking(s) => self.thinking.push_str(&s),
            Delta::Signature(s) => self.signature.get_or_insert_with(String::new).push_str(&s),
            Delta::ToolCall {
                index,
                id,
                name,
                arguments,
            } => {
                let partial = self.tool_calls.entry(index).or_default();
                if let Some(id) = id {
                    partial.id = id;
                }
                if let Some(name) = name {
                    partial.name = name;
                }
                partial.arguments.push_str(&arguments);
            }
            Delta::Finish(reason) => self.finish_reason = Some(reason),
            Delta::Usage(usage) => merge_usage(&mut self.usage, usage),
        }
    }

    /// 流结束(`[DONE]` / `message_stop` / EOF)时调用,产出可直接进历史的响应。
    ///
    /// 组装走 `Msg::assistant_with` 这一个入口,与解析层、非流式解码共用,
    /// 不在这里手拼 `AssistantMsg` / `Thinking`。没收到结束原因时归一成
    /// `FinishReason::Unknown` —— 由上层决定怎么处理,通用层不做分流策略。
    /// 取消 / 错误路径不调用本方法,因此不会产出半截响应被误当成功。
    pub fn finish(self) -> ChatResponse {
        let Self {
            text,
            thinking,
            signature,
            tool_calls,
            finish_reason,
            usage,
        } = self;
        let thinking = if thinking.is_empty() {
            None
        } else {
            Some(thinking)
        };
        let tool_calls = tool_calls
            .into_values()
            .map(|p| ToolCall::new(p.id, p.name, p.arguments))
            .collect();
        ChatResponse {
            message: Msg::assistant_with(text, tool_calls, thinking, signature),
            finish_reason: finish_reason.unwrap_or(FinishReason::Unknown),
            usage,
        }
    }
}

/// 按字段合并:`0` = 没单列 / 没提供,非零才覆盖,不能整份替换
/// (Anthropic 的 input / output 分两处到,整份替换会丢掉先到的那一半)。
/// 全 0 的一帧视为「没给」,不把 `None` 编造成 `Some(全 0)`。
fn merge_usage(slot: &mut Option<Usage>, incoming: Usage) {
    let has_any = incoming.input_tokens > 0
        || incoming.output_tokens > 0
        || incoming.reasoning_tokens > 0
        || incoming.cached_tokens > 0;
    if !has_any {
        return;
    }
    let merged = slot.get_or_insert_with(Usage::default);
    if incoming.input_tokens > 0 {
        merged.input_tokens = incoming.input_tokens;
    }
    if incoming.output_tokens > 0 {
        merged.output_tokens = incoming.output_tokens;
    }
    if incoming.reasoning_tokens > 0 {
        merged.reasoning_tokens = incoming.reasoning_tokens;
    }
    if incoming.cached_tokens > 0 {
        merged.cached_tokens = incoming.cached_tokens;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(deltas: impl IntoIterator<Item = Delta>) -> ChatResponse {
        let mut acc = StreamAccumulator::default();
        for delta in deltas {
            acc.push(delta);
        }
        acc.finish()
    }

    /// 正文 / 思考 / 签名分别落到自己的槽位,正文与思考都是逐片拼接。
    #[test]
    fn text_thinking_and_signature_land_on_the_assistant_msg() {
        let reply = collect([
            Delta::Text("改".into()),
            Delta::Text("好了".into()),
            Delta::Thinking("先看".into()),
            Delta::Thinking("目录".into()),
            Delta::Signature("sig".into()),
            Delta::Finish(FinishReason::Stop),
        ]);
        let Msg::Assistant(m) = &reply.message else {
            panic!("expected assistant");
        };
        assert_eq!(m.content, "改好了");
        assert!(m.tool_calls.is_empty());
        let thinking = m.thinking.as_ref().expect("thinking");
        assert_eq!(thinking.text, "先看目录");
        assert_eq!(thinking.signature.as_deref(), Some("sig"));
        assert_eq!(reply.finish_reason, FinishReason::Stop);
        assert_eq!(reply.usage, None);
    }

    /// 乱序到达的分片按 index 归组,收尾按原下标升序输出,参数逐片拼接不是覆盖。
    #[test]
    fn tool_calls_are_grouped_by_index_in_original_order() {
        let reply = collect([
            Delta::ToolCall {
                index: 1,
                id: Some("c2".into()),
                name: Some("write".into()),
                arguments: "{\"p\"".into(),
            },
            Delta::ToolCall {
                index: 0,
                id: Some("c1".into()),
                name: Some("bash".into()),
                arguments: "{\"cmd\"".into(),
            },
            Delta::ToolCall {
                index: 1,
                id: None,
                name: None,
                arguments: ":1}".into(),
            },
            Delta::ToolCall {
                index: 0,
                id: None,
                name: None,
                arguments: ":\"ls\"}".into(),
            },
            Delta::Finish(FinishReason::ToolCalls),
        ]);
        let Msg::Assistant(m) = &reply.message else {
            panic!("expected assistant");
        };
        assert_eq!(m.content, "");
        assert_eq!(
            m.tool_calls,
            vec![
                ToolCall::new("c1", "bash", "{\"cmd\":\"ls\"}"),
                ToolCall::new("c2", "write", "{\"p\":1}"),
            ]
        );
        assert_eq!(reply.finish_reason, FinishReason::ToolCalls);
    }

    /// 跨帧按字段合并;中间那帧全 0 不该清掉已有值,也不该凭空造出用量。
    #[test]
    fn usage_is_merged_field_by_field() {
        let reply = collect([
            Delta::Usage(Usage {
                input_tokens: 294,
                ..Default::default()
            }),
            Delta::Usage(Usage::default()),
            Delta::Usage(Usage {
                output_tokens: 61,
                reasoning_tokens: 19,
                ..Default::default()
            }),
            Delta::Usage(Usage {
                cached_tokens: 100,
                ..Default::default()
            }),
        ]);
        assert_eq!(
            reply.usage,
            Some(Usage {
                input_tokens: 294,
                output_tokens: 61,
                reasoning_tokens: 19,
                cached_tokens: 100,
            })
        );
    }

    /// 空流也要能收尾:空正文的 assistant + Unknown,不编造 usage。
    #[test]
    fn missing_finish_becomes_unknown_and_never_invents_usage() {
        let reply = collect(Vec::<Delta>::new());
        let Msg::Assistant(m) = &reply.message else {
            panic!("expected assistant");
        };
        assert_eq!(m.content, "");
        assert!(m.thinking.is_none());
        assert_eq!(reply.finish_reason, FinishReason::Unknown);
        assert_eq!(reply.usage, None);
    }

    /// 只有签名、没有思考文本时由 `assistant_with` 明确丢弃,不造空思考。
    #[test]
    fn signature_without_thinking_is_dropped() {
        let reply = collect([Delta::Signature("sig".into())]);
        let Msg::Assistant(m) = &reply.message else {
            panic!("expected assistant");
        };
        assert!(m.thinking.is_none());
    }

    #[test]
    fn later_finish_overrides_earlier() {
        let reply = collect([
            Delta::Finish(FinishReason::Length),
            Delta::Finish(FinishReason::Stop),
        ]);
        assert_eq!(reply.finish_reason, FinishReason::Stop);
    }
}
