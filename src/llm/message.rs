use async_openai::types::chat::{
    ChatCompletionRequestAssistantMessageArgs, ChatCompletionRequestMessage,
    ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestToolMessageArgs,
    ChatCompletionRequestUserMessageArgs,
};

/// 构造 system 消息(系统提示,一般放消息列表最前)
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

/// 构造 tool 消息(工具执行结果)
/// `tool_call_id` 必须与 assistant 消息里 tool_calls 的 id 一一对应
pub fn tool_result(tool_call_id: &str, content: &str) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::Tool(
        ChatCompletionRequestToolMessageArgs::default()
            .tool_call_id(tool_call_id)
            .content(content)
            .build()
            .expect("valid tool message"),
    )
}
