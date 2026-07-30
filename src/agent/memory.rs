use async_openai::types::chat::ChatCompletionRequestMessage;

/// 会话记忆:跨轮保留 messages,支持多轮对话
/// 对 agent 透明:agent 只管 add 产生的新消息、用 view 取可发 LLM 的消息
/// 压缩/存储等维护逻辑都在内部,agent 无感
pub struct Memory {
    messages: Vec<ChatCompletionRequestMessage>,
    // TODO: 注入 Compactor(策略: Truncate/Summarize) —— add 时按需压缩,届时 add 改 async
    // TODO: 持久化 —— 文件/DB 存储,跨进程保留
}

impl Memory {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
        }
    }

    /// 追加消息(agent 产生新消息时调用)
    // TODO: 压缩 —— 超窗时调 Compactor 摘要/截断旧消息
    pub fn add(&mut self, msg: ChatCompletionRequestMessage) {
        self.messages.push(msg);
    }

    /// 提供给 LLM 调用的当前消息视图(保证不超窗)
    // TODO: 返回压缩后的可用消息
    pub fn view(&self) -> &[ChatCompletionRequestMessage] {
        &self.messages
    }
}

impl Default for Memory {
    fn default() -> Self {
        Self::new()
    }
}
