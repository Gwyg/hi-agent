use async_openai::types::chat::ChatCompletionRequestMessage;

/// 持久化层:消息存储后端。本期单会话、一次性使用,不涉及多会话管理。
///
/// 只存消息,结构稳定(协议定死)。设计原则:消息只按需分段读(range),
/// 不提供全量 all(),避免长会话全量加载。
///
/// TODO(未来): 多会话管理(create/list/delete)、算法状态持久化(blob)、落盘实现。
pub trait Persistence: Send + Sync {
    /// 追加一条消息(永不删)
    fn append(&mut self, msg: ChatCompletionRequestMessage);

    /// 消息总条数,不加载内容
    fn len(&self) -> usize;

    /// 取消息 [start, end) 区间(越界自动裁剪)。落盘实现只反序列化该段。
    fn range(&self, start: usize, end: usize) -> Vec<ChatCompletionRequestMessage>;
}

/// 内存实现:本期唯一实现。未来换落盘只改实现,上层面向 trait 不变。
pub struct InMemoryPersistence {
    messages: Vec<ChatCompletionRequestMessage>,
}

impl InMemoryPersistence {
    pub fn new() -> Self {
        todo!("初始化空消息列表")
    }
}

impl Persistence for InMemoryPersistence {
    fn append(&mut self, _msg: ChatCompletionRequestMessage) {
        todo!("追加到 messages")
    }

    fn len(&self) -> usize {
        todo!("返回 messages 长度")
    }

    fn range(&self, _start: usize, _end: usize) -> Vec<ChatCompletionRequestMessage> {
        todo!("裁剪越界后返回 messages[start..end] 的 clone")
    }
}
