use crate::llm::{LlmClient, Toolbox};

use super::memory::Memory;

/// agent 循环:chat ↔ 工具,直到模型给出最终回复(Stop)
/// 通过 Memory 的 add/view 与记忆交互,压缩由 Memory 内部接管,本循环不感知
// TODO: 阈值软截止 —— 达到轮次/时间阈值后暂停,询问用户(继续/停止/注入指令),而非强制中断
// TODO: 接入 safety —— 工具调用前过 SafetyChecker 检查
pub(crate) async fn agent_loop(
    client: &LlmClient,
    toolbox: &Toolbox,
    memory: &mut Memory,
) -> anyhow::Result<String> {
    todo!("agent_loop: memory.view() → chat → Stop 返回 / ToolCalls 回灌 memory.add → 循环")
}
