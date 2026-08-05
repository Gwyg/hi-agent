use async_openai::types::chat::{
    ChatCompletionMessageToolCalls, ChatCompletionRequestAssistantMessageArgs,
    ChatCompletionRequestMessage, ChatCompletionRequestSystemMessageArgs,
    ChatCompletionRequestToolMessageArgs, ChatCompletionRequestUserMessageArgs,
};

/// 构造 system 消息(系统提示,一般放消息列表最前)。
/// 纯封装:一段文本 → System 消息。多段合并等业务归调用方(如 Core)。
pub fn system(content: &str) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::System(
        ChatCompletionRequestSystemMessageArgs::default()
            .content(content)
            .build()
            .expect("valid system message"),
    )
}

/// 构造 user 消息(用户输入)
pub fn user(content: &str) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::User(
        ChatCompletionRequestUserMessageArgs::default()
            .content(content)
            .build()
            .expect("valid user message"),
    )
}

/// 构造 assistant 消息(AI 回复,用于多轮对话历史)
pub fn assistant(content: &str) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::Assistant(
        ChatCompletionRequestAssistantMessageArgs::default()
            .content(content)
            .build()
            .expect("valid assistant message"),
    )
}

/// 构造带 tool_calls 的 assistant 消息(agent 循环回灌"模型说要调工具"那条消息,防模型失忆)
/// `tool_calls` 直接用模型返回的 tool_calls 原样塞回,保留 id/name/arguments 对应关系
pub fn assistant_with_tool_calls(
    tool_calls: Vec<ChatCompletionMessageToolCalls>,
) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::Assistant(
        ChatCompletionRequestAssistantMessageArgs::default()
            .tool_calls(tool_calls)
            .build()
            .expect("valid assistant message with tool_calls"),
    )
}

/// 构造 tool 消息(工具执行结果)
/// `tool_call_id` 必须与 assistant 消息里 tool_calls 的 id 一一对应
/// 内容过统一截断层:单条结果超上限即截头 + 提示,防单个工具输出撑爆上下文
pub fn tool_result(tool_call_id: &str, content: &str) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::Tool(
        ChatCompletionRequestToolMessageArgs::default()
            .tool_call_id(tool_call_id)
            .content(truncate_tool_output(content))
            .build()
            .expect("valid tool message"),
    )
}

/// 单条工具结果字符上限(约 ~1.5 万 token)。通用兜底:各工具自身另有限制
/// (read 2000 行、bash MAX_OUTPUT),此层拦住 search 等无独立限制的工具与未来新工具。
const MAX_TOOL_RESULT_CHARS: usize = 60_000;

/// 工具输出统一截断:超上限保留头部 + 截断提示,引导模型用更精确参数分批获取。
fn truncate_tool_output(content: &str) -> String {
    let total = content.chars().count();
    if total <= MAX_TOOL_RESULT_CHARS {
        return content.to_string();
    }
    let head: String = content.chars().take(MAX_TOOL_RESULT_CHARS).collect();
    format!(
        "{head}\n\n[工具输出过长已截断:共 {total} 字符,仅显示前 {MAX_TOOL_RESULT_CHARS} 字符。\
         如需完整内容,用更精确的参数分批获取(read 用 offset/limit 分页、search 缩小范围)]"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_output_unchanged() {
        let s = "hello world";
        assert_eq!(truncate_tool_output(s), s);
    }

    #[test]
    fn long_output_truncated_with_hint() {
        let s = "x".repeat(MAX_TOOL_RESULT_CHARS + 500);
        let out = truncate_tool_output(&s);
        assert!(out.chars().count() < s.chars().count(), "应被截短");
        assert!(out.contains("工具输出过长已截断"), "应带截断提示");
        assert!(out.contains(&(MAX_TOOL_RESULT_CHARS + 500).to_string()), "应报总字符数");
    }
}
