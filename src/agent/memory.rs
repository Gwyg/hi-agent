use async_openai::types::chat::ChatCompletionRequestMessage;

/// 粗略字符上限(用 Debug 串估算,偏保守)
/// 约 15k-25k tokens,留足余量给 system 和回复
/// DeepSeek 窗口 64k,保守取 60k 字符
const MAX_CHARS: usize = 100_000;

/// 会话记忆:内存版,跨轮保留 messages
/// 超窗时自动截断:保留 system + 从 user 边界开始的尾部消息
/// user 边界对齐保证 tool_calls/tool_result 配对不被切断(否则 LLM 报错)
// TODO: 持久化(文件/DB) —— 跨进程保留
// TODO: 摘要压缩(Compactor) —— 超窗时摘要旧消息而非直接丢弃
pub struct Memory {
    messages: Vec<ChatCompletionRequestMessage>,
}

impl Memory {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
        }
    }

    /// 追加消息(agent 产生新消息时调用)
    pub fn add(&mut self, msg: ChatCompletionRequestMessage) {
        self.messages.push(msg);
    }

    /// 返回发给 LLM 的消息视图,保证不超窗
    ///
    /// 截断策略:
    /// 1. 总字符数未超 MAX_CHARS → 返回全部
    /// 2. 超限 → 保留首条 system + 从后往前累加到 MAX_CHARS 的尾部消息
    /// 3. 截断点对齐到 user 边界(跳过开头落单的 tool/assistant),保证配对完整
    pub fn view(&self) -> Vec<ChatCompletionRequestMessage> {
        if self.messages.len() <= 1 {
            return self.messages.clone();
        }
        let total: usize = self.messages.iter().map(msg_chars).sum();
        if total <= MAX_CHARS {
            return self.messages.clone();
        }
        // 超限:保留首条 system,从后往前找截断点
        let system = self.messages[0].clone();
        let rest = &self.messages[1..];
        let mut acc = 0;
        let mut cut = 0;
        for (i, msg) in rest.iter().enumerate().rev() {
            acc += msg_chars(msg);
            if acc > MAX_CHARS {
                cut = i + 1; // i 超限,从 i+1 开始保留
                break;
            }
        }
        // 对齐到 user 边界:跳过开头落单的 tool/assistant(tool_calls 配对被切的残骸)
        let mut start = cut;
        while start < rest.len() && !is_user(&rest[start]) {
            start += 1;
        }
        let mut result = vec![system];
        result.extend_from_slice(&rest[start..]);
        result
    }

    /// 清空对话历史(保留 system prompt,重置会话上下文)
    /// 待 TUI 接入 /clear 命令时使用
    #[allow(dead_code)]
    pub fn clear(&mut self) {
        if let Some(system) = self.messages.first() {
            if matches!(system, ChatCompletionRequestMessage::System(_)) {
                let system = system.clone();
                self.messages.clear();
                self.messages.push(system);
                return;
            }
        }
        // 无 system(罕见),全清
        self.messages.clear();
    }
}

impl Default for Memory {
    fn default() -> Self {
        Self::new()
    }
}

/// 估算消息字符数(用 Debug 串,偏保守,无需访问私有字段)
fn msg_chars(msg: &ChatCompletionRequestMessage) -> usize {
    format!("{msg:?}").len()
}

fn is_user(msg: &ChatCompletionRequestMessage) -> bool {
    matches!(msg, ChatCompletionRequestMessage::User(_))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{assistant, system, tool_result, user};

    #[test]
    fn under_limit_returns_all() {
        let mut m = Memory::new();
        m.add(system("sys"));
        m.add(user("hi"));
        assert_eq!(m.view().len(), 2);
    }

    #[test]
    fn over_limit_keeps_system_first() {
        let mut m = Memory::new();
        m.add(system("sys"));
        // 填充大量消息触发截断
        for i in 0..1000 {
            m.add(user(&"x".repeat(200)));
            m.add(assistant(&"y".repeat(200)));
            let _ = i;
        }
        let view = m.view();
        // 首条必为 system
        assert!(matches!(view[0], ChatCompletionRequestMessage::System(_)));
        // 总字符数应在 MAX_CHARS 附近(不超过 MAX_CHARS + 单条最大长度)
        let total: usize = view.iter().map(msg_chars).sum();
        assert!(total < MAX_CHARS + 5_000, "截断后总字符 {total} 过大");
    }

    #[test]
    fn clear_keeps_system() {
        let mut m = Memory::new();
        m.add(system("sys prompt"));
        m.add(user("hi"));
        m.add(assistant("hello"));
        m.clear();
        let view = m.view();
        assert_eq!(view.len(), 1, "clear 后应只剩 system");
        assert!(matches!(view[0], ChatCompletionRequestMessage::System(_)));
    }

    #[test]
    fn clear_empty_no_panic() {
        let mut m = Memory::new();
        m.clear(); // 不应 panic
    }
}
