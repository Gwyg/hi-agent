use async_openai::{
    Client,
    config::OpenAIConfig,
    types::chat::{
        ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls,
        ChatCompletionRequestMessage, ChatCompletionResponseMessage, ChatCompletionStreamOptions,
        ChatCompletionTools, CompletionUsage, CreateChatCompletionRequestArgs, FinishReason,
        FunctionCall, Role,
    },
};
use futures::StreamExt;
use std::collections::BTreeMap;

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

/// 流式 tool_call 累积器:按 index 聚合分片到达的 id/name/arguments
#[derive(Default)]
struct ToolCallAcc {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

/// 空字符串转 None,适配 OpenAI message 字段的 Option 语义
fn empty_to_none(s: String) -> Option<String> {
    if s.is_empty() { None } else { Some(s) }
}

/// 按 finish_reason 把 message 归类到 ChatResponse,chat/chat_stream 共用
fn classify(
    message: ChatCompletionResponseMessage,
    finish_reason: Option<FinishReason>,
) -> ChatResponse {
    match finish_reason {
        Some(FinishReason::Stop) => ChatResponse::Stop(message),
        Some(FinishReason::ToolCalls) => ChatResponse::ToolCalls(message),
        Some(FinishReason::Length) => ChatResponse::Length(message),
        Some(FinishReason::ContentFilter) | Some(FinishReason::FunctionCall) | None => {
            ChatResponse::Filtered(message)
        }
    }
}

#[derive(Clone)]
pub struct LlmClient {
    inner: Client<OpenAIConfig>,
    model: String,
    /// 工具 schema 集合(纯数据,不持可执行对象),构造请求时 clone 喂给 LLM
    tool_defs: Vec<ChatCompletionTools>,
}

impl LlmClient {
    /// `tool_defs` 来自 Toolbox::definitions(),client 不持工具对象,只管 schema
    /// model/base_url 经 config 分层解析(env > 项目 > 用户 > 默认);api_key 仅走 env
    /// config 未初始化(测试)时用默认 LlmConfig,行为退回内置默认
    pub fn new(tool_defs: Vec<ChatCompletionTools>) -> Self {
        let llm = crate::config::get()
            .map(|c| c.llm)
            .unwrap_or_default();
        let config = OpenAIConfig::new()
            .with_api_key(llm.api_key())
            .with_api_base(llm.base_url());
        Self {
            inner: Client::with_config(config),
            model: llm.model(),
            tool_defs,
        }
    }

    /// 非流式聊天(预留:当前 agent_loop 用 chat_stream,压缩摘要将用它)
    /// 返回 (响应分类, token 用量)。usage 为 None 表示接口未返回统计
    #[allow(dead_code)]
    pub async fn chat(
        &self,
        messages: Vec<ChatCompletionRequestMessage>,
    ) -> anyhow::Result<(ChatResponse, Option<CompletionUsage>)> {
        self.chat_inner(messages, true).await
    }

    /// 非流式聊天,不挂 tools schema。用于压缩摘要——摘要器无需工具,少喂一坨 schema 省 token。
    pub async fn chat_no_tools(
        &self,
        messages: Vec<ChatCompletionRequestMessage>,
    ) -> anyhow::Result<(ChatResponse, Option<CompletionUsage>)> {
        self.chat_inner(messages, false).await
    }

    /// 非流式聊天内核:with_tools 决定是否附带工具 schema
    async fn chat_inner(
        &self,
        messages: Vec<ChatCompletionRequestMessage>,
        with_tools: bool,
    ) -> anyhow::Result<(ChatResponse, Option<CompletionUsage>)> {
        let msg_count = messages.len();
        tracing::info!(model = %self.model, msg_count, with_tools, "LLM 非流式请求");
        let mut builder = CreateChatCompletionRequestArgs::default();
        builder.model(&self.model).messages(messages);
        if with_tools && !self.tool_defs.is_empty() {
            builder.tools(self.tool_defs.clone());
        }
        let request = builder.build()?;
        let mut response = self.inner.chat().create(request).await.map_err(|e| {
            tracing::error!("LLM 非流式请求失败: {e:#}");
            anyhow::anyhow!("LLM 请求失败: {e}")
        })?;
        let usage = response.usage.take();
        let choice = response
            .choices
            .pop()
            .ok_or_else(|| anyhow::anyhow!("empty response"))?;
        let finish_reason = choice.finish_reason;
        let result = classify(choice.message, finish_reason);
        tracing::info!(
            model = %self.model,
            msg_count,
            finish = ?finish_reason,
            prompt_tokens = usage.as_ref().map(|u| u.prompt_tokens).unwrap_or(0),
            completion_tokens = usage.as_ref().map(|u| u.completion_tokens).unwrap_or(0),
            "LLM 非流式响应"
        );
        Ok((result, usage))
    }

    pub async fn chat_stream(
        &self,
        messages: Vec<ChatCompletionRequestMessage>,
        on_token: impl Fn(&str),
    ) -> anyhow::Result<(ChatResponse, Option<CompletionUsage>)> {
        let msg_count = messages.len();
        tracing::info!(model = %self.model, msg_count, "LLM 流式请求开始");
        let mut builder = CreateChatCompletionRequestArgs::default();
        builder
            .model(&self.model)
            .messages(messages)
            .stream_options(ChatCompletionStreamOptions {
                include_usage: Some(true),
                include_obfuscation: None,
            });
        if !self.tool_defs.is_empty() {
            builder.tools(self.tool_defs.clone());
        }
        let request = builder.build()?;
        let mut stream = self.inner.chat().create_stream(request).await.map_err(|e| {
            tracing::error!("LLM 流式请求建立失败: {e:#}");
            anyhow::anyhow!("LLM 流式请求失败: {e}")
        })?;

        let mut content = String::new();
        let mut refusal = String::new();
        let mut tool_calls_map: BTreeMap<u32, ToolCallAcc> = BTreeMap::new();
        let mut finish_reason: Option<FinishReason> = None;
        let mut usage: Option<CompletionUsage> = None;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| {
                tracing::warn!("LLM 流式 chunk 出错: {e:#}");
                anyhow::anyhow!("LLM 流式出错: {e}")
            })?;
            // usage chunk(choices 为空)携带整次请求的 token 统计,单独最后到达
            if let Some(u) = chunk.usage {
                usage = Some(u);
            }
            for choice in chunk.choices {
                // 文本内容增量
                if let Some(text) = choice.delta.content.filter(|t| !t.is_empty()) {
                    content.push_str(&text);
                    on_token(&text);
                }
                // 安全拒绝原因增量
                if let Some(r) = choice.delta.refusal.filter(|r| !r.is_empty()) {
                    refusal.push_str(&r);
                }
                // 工具调用分片,按 index 聚合 id/name/arguments
                if let Some(calls) = choice.delta.tool_calls {
                    for call in calls {
                        let entry = tool_calls_map.entry(call.index).or_default();
                        if let Some(id) = call.id {
                            entry.id = Some(id);
                        }
                        if let Some(func) = call.function {
                            if let Some(name) = func.name {
                                entry.name = Some(name);
                            }
                            if let Some(args) = func.arguments {
                                entry.arguments.push_str(&args);
                            }
                        }
                    }
                }
                // finish_reason 只在最后一个 chunk 出现
                if choice.finish_reason.is_some() {
                    finish_reason = choice.finish_reason;
                }
            }
        }

        if !refusal.is_empty() {
            tracing::warn!(model = %self.model, refusal = %refusal, "LLM 返回安全拒绝");
        }

        let tool_call_count = tool_calls_map.len();
        let tool_calls = if tool_calls_map.is_empty() {
            None
        } else {
            Some(
                tool_calls_map
                    .into_values()
                    .map(|acc| {
                        ChatCompletionMessageToolCalls::Function(ChatCompletionMessageToolCall {
                            id: acc.id.unwrap_or_default(),
                            function: FunctionCall {
                                name: acc.name.unwrap_or_default(),
                                arguments: acc.arguments,
                            },
                        })
                    })
                    .collect(),
            )
        };

        let content_len = content.chars().count();
        let message = ChatCompletionResponseMessage {
            content: empty_to_none(content),
            tool_calls,
            refusal: empty_to_none(refusal),
            annotations: None,
            role: Role::Assistant,
            #[allow(deprecated)]
            function_call: None,
            audio: None,
        };

        tracing::info!(
            model = %self.model,
            finish = ?finish_reason,
            content_len,
            tool_call_count,
            prompt_tokens = usage.as_ref().map(|u| u.prompt_tokens).unwrap_or(0),
            completion_tokens = usage.as_ref().map(|u| u.completion_tokens).unwrap_or(0),
            "LLM 流式响应完成"
        );

        Ok((classify(message, finish_reason), usage))
    }
}
