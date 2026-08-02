use crate::llm::{LlmClient, Toolbox, system, user};
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
        let mut memory = Memory::new();
        // 注入系统提示词(首轮即生效,跨轮保留在 memory 头部)
        memory.add(system(&build_system_prompt()));
        Self {
            client,
            toolbox,
            memory,
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
        self.memory.add(user(user_input));
        agent_loop(&self.client, &self.toolbox, &mut self.memory, emit).await
    }
}

/// 构造系统提示词:身份 + 工作目录 + 行为准则
/// 工具详情不在此重复(模型从 tools schema 获取),只给高层准则
fn build_system_prompt() -> String {
    let root = crate::llm::tools::sandbox::project_root()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "未知".to_string());
    format!(
        "你是 hi-agent,一个编程助手。\n\n\
         工作目录:{root}\n\n\
         行为准则:\n\
         1. 优先用专用工具(read/write/edit/search_files),它们更安全。bash 是兜底,专用工具做不到时再用。\n\
         2. 改文件前先 read 确认内容,避免误改。\n\
         3. 简述意图和结果,不啰嗦。\n\
         4. 不确定时询问用户,不擅自做有风险的操作。\n\
         5. 回复用中文。"
    )
}
