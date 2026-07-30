use async_openai::{
    Client,
    config::OpenAIConfig,
    error::OpenAIError,
    types::chat::{
        ChatCompletionRequestMessage, ChatCompletionResponseMessage, ChatCompletionTools,
        CreateChatCompletionRequestArgs, CreateChatCompletionStreamResponse, FinishReason,
    },
};
use futures::Stream;
use std::pin::Pin;

/// chat 调用的返回结果,按结束原因分类,便于 agent 循环分流
pub enum ChatResponse {
    /// 模型自然结束(stop)
    Stop(ChatCompletionResponseMessage),
    /// 模型要调用工具(tool_calls)
    ToolCalls(ChatCompletionResponseMessage),
    /// 被截断(length)
    Length(ChatCompletionResponseMessage),
    /// 内容过滤或未知原因(content_filter / function_call / None)
    Filtered(ChatCompletionResponseMessage),
}

pub struct LlmClient {
    inner: Client<OpenAIConfig>,
    model: String,
    /// 工具 schema 集合(纯数据,不持可执行对象),构造请求时 clone 喂给 LLM
    tool_defs: Vec<ChatCompletionTools>,
}

impl LlmClient {
    /// `tool_defs` 来自 Toolbox::definitions(),client 不持工具对象,只管 schema
    pub fn new(tool_defs: Vec<ChatCompletionTools>) -> Self {
        let config = OpenAIConfig::new()
            .with_api_key(std::env::var("API_KEY").unwrap_or_default())
            .with_api_base(
                std::env::var("BASE_URL")
                    .unwrap_or_else(|_| "https://api.openai.com/v1".to_string()),
            );
        Self {
            inner: Client::with_config(config),
            model: std::env::var("MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string()),
            tool_defs,
        }
    }

    /// 用自定义 config + 工具 schema
    pub fn with_config(config: OpenAIConfig, tool_defs: Vec<ChatCompletionTools>) -> Self {
        Self {
            inner: Client::with_config(config),
            model: std::env::var("MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string()),
            tool_defs,
        }
    }

    pub async fn chat(
        &self,
        messages: Vec<ChatCompletionRequestMessage>,
    ) -> anyhow::Result<ChatResponse> {
        let request = CreateChatCompletionRequestArgs::default()
            .model(&self.model)
            .messages(messages)
            .tools(self.tool_defs.clone())
            .build()?;
        let mut response = self.inner.chat().create(request).await?;
        let choice = response
            .choices
            .pop()
            .ok_or_else(|| anyhow::anyhow!("empty response"))?;
        let outcome = match choice.finish_reason {
            Some(FinishReason::Stop) => ChatResponse::Stop(choice.message),
            Some(FinishReason::ToolCalls) => ChatResponse::ToolCalls(choice.message),
            Some(FinishReason::Length) => ChatResponse::Length(choice.message),
            Some(FinishReason::ContentFilter) | Some(FinishReason::FunctionCall) | None => {
                ChatResponse::Filtered(choice.message)
            }
        };
        Ok(outcome)
    }

    pub async fn chat_stream(
        &self,
        messages: Vec<ChatCompletionRequestMessage>,
    ) -> anyhow::Result<
        Pin<Box<dyn Stream<Item = Result<CreateChatCompletionStreamResponse, OpenAIError>> + Send>>,
    > {
        let request = CreateChatCompletionRequestArgs::default()
            .model(&self.model)
            .messages(messages)
            .tools(self.tool_defs.clone())
            .build()?;
        let stream = self.inner.chat().create_stream(request).await?;
        Ok(Box::pin(stream))
    }
}
