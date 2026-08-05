use async_openai::types::chat::ChatCompletionRequestMessage;

mod archive;
mod core;
mod working;

use crate::llm::LlmClient;
use core::Core;
use working::Working;

/// 会话记忆:三层架构门面。
///
///   Core     核心记忆:常驻上下文(system prompt + 未来关键状态块),不参与压缩
///   Working  工作记忆:对话历史 + 滚动压缩
///   Archive  归档记忆:跨会话持久化(本期占位,不实现)
///
/// 对 agent 循环只暴露:add / add_response / view。
/// 压缩、token 记账、并发、system 注入全藏在内部。
///
/// view 输出 = Core.system + Working.view()
pub struct Memory {
    core: Core,
    working: Working,
}

impl Memory {
    pub fn new() -> Self {
        Self {
            core: Core::new(),
            working: Working::new(),
        }
    }

    /// 追加普通对话消息(user / tool 结果)
    pub fn add(&mut self, msg: ChatCompletionRequestMessage) {
        self.working.push(msg);
    }

    /// 通报模型回复 + 其 token 开销(同一事件一起灌入)。
    /// 落消息、记 usage,并在超阈值时启动后台压缩。
    pub fn add_response(
        &mut self,
        reply: ChatCompletionRequestMessage,
        usage: Option<async_openai::types::chat::CompletionUsage>,
        client: &LlmClient,
    ) {
        self.working.push(reply);
        self.working.on_response(usage, client);
    }

    /// 返回发给 LLM 的消息视图:单条 system(核心提示词 + 工作层摘要) + 近期原始消息。
    ///
    /// 门面只组装结构:向 Core 递工作层摘要,由 Core 合并进单条 system(不碰构建细节)。
    /// 记忆逻辑增长时(三级召回 / 状态块)在此多递几段即可,接口不变。
    pub async fn view(&mut self) -> Vec<ChatCompletionRequestMessage> {
        let (summary, messages) = self.working.view().await;
        let mut out = Vec::with_capacity(2 + messages.len());
        out.push(self.core.system(summary.as_deref().as_slice()));
        // 协议适配:多数厂商要求 system 后必接 user。
        // 压缩切点可能落在 assistant 组后(单 user 长任务),此处补占位 user 兜底。
        if needs_placeholder_user(&messages) {
            out.push(crate::llm::user("(继续上文)"));
        }
        out.extend(messages);
        out
    }
}

/// 判断是否需要在 system 后补占位 user。
/// 首条非 user(含空)时返回 true——压缩切在 assistant 组后或会话初始时兜底协议。
fn needs_placeholder_user(messages: &[ChatCompletionRequestMessage]) -> bool {
    !matches!(
        messages.first(),
        Some(ChatCompletionRequestMessage::User(_))
    )
}

impl Default for Memory {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{assistant, user};

    #[test]
    fn placeholder_needed_when_first_not_user() {
        assert!(needs_placeholder_user(&[assistant("hi")]));
    }

    #[test]
    fn placeholder_needed_when_empty() {
        assert!(needs_placeholder_user(&[]));
    }

    #[test]
    fn no_placeholder_when_user_first() {
        assert!(!needs_placeholder_user(&[user("hi")]));
    }
}
