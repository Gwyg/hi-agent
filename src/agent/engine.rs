use crate::llm::{LlmClient, Toolbox};

use super::memory::Memory;
use super::safety::SafetyChecker;

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
    // TODO: memory.add(user) → agent_loop(&mut memory)(过 safety 检查) → memory.add(assistant) → 返回回复
    pub async fn run_turn(&mut self, _user_input: &str) -> anyhow::Result<String> {
        todo!("run_turn: memory.add(user) → agent_loop(&mut memory) → memory.add(assistant) → 返回回复")
    }
}
