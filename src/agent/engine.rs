use crate::llm::{user, LlmClient, Toolbox};

use super::memory::Memory;
use super::safety::SafetyChecker;
use super::agent_loop::agent_loop;

/// 引擎:驱动 agent 对话的执行能力层
/// 持有 client/toolbox/memory/safety,管理一轮对话的完整生命周期
pub struct Engine {
    client: LlmClient,
    toolbox: Toolbox,
    memory: Memory,
    safety: SafetyChecker,
}

impl Engine {
    pub fn new(client: LlmClient, toolbox: Toolbox) -> Self {
        Self {
            client,
            toolbox,
            memory: Memory::new(),
            safety: SafetyChecker::new(),
        }
    }

    /// 执行一轮对话:user_input 进,最终回复出
    pub async fn run_turn(&mut self, user_input: &str) -> anyhow::Result<String> {
        // TODO: 系统提示词注入 —— 首次 run_turn 时注入 project_root 到 system 消息
        // 让模型知工作目录,传相对路径有锚点。需检查 memory 是否已有 system 避免重复
        self.memory.add(user(user_input));
        agent_loop(&self.client, &self.toolbox, &mut self.memory, &self.safety).await
    }
}
