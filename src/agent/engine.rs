use crate::llm::{LlmClient, Toolbox, user};
use tokio::sync::mpsc;

use super::EngineEvent;
use super::agent_loop::agent_loop;
use super::memory::Memory;

/// 引擎:驱动 agent 对话的执行能力层
/// 持有 client/toolbox/memory,管理一轮对话的完整生命周期
pub struct Engine {
    client: LlmClient,
    toolbox: Toolbox,
    memory: Memory,
}

impl Engine {
    pub fn new(client: LlmClient, toolbox: Toolbox) -> Self {
        // system prompt 由核心记忆层(Core)在 Memory::new 时自动加载
        Self {
            client,
            toolbox,
            memory: Memory::new(),
        }
    }

    /// 执行一轮对话:user_input 进,事件流通过 emit 推给前端
    ///
    /// 前端(TUI/Web/CLI)持有 mpsc::Receiver<EngineEvent> 消费事件:
    /// - TokenDelta:流式追加到当前 assistant 消息
    /// - ToolStart/ToolResult:更新工具调用卡片
    /// - Ask:需前端回传 AskReply(oneshot),不处理则 reply drop → agent_loop 暂拒
    /// - Done:一轮结束,assistant 消息标记 Completed
    /// - Error:出错
    pub async fn run_turn_stream(
        &mut self,
        user_input: &str,
        emit: mpsc::Sender<EngineEvent>,
    ) -> anyhow::Result<()> {
        let input_preview: String = user_input.chars().take(200).collect();
        tracing::info!(input_len = user_input.chars().count(), input = %input_preview, "用户输入,开始新一轮");
        self.memory.add(user(user_input));
        agent_loop(&self.client, &self.toolbox, &mut self.memory, emit).await
    }
}
