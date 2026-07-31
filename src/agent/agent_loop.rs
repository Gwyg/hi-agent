use crate::llm::{
    assistant, assistant_with_tool_calls, tool_result, ChatResponse, LlmClient, Toolbox,
};
use async_openai::types::chat::ChatCompletionMessageToolCalls;

use super::memory::Memory;
use super::safety::{SafetyChecker, SafetyVerdict};

/// agent 循环:chat ↔ 工具,直到模型给出最终回复(Stop)
/// 通过 Memory 的 add/view 与记忆交互,压缩由 Memory 内部接管,本循环不感知
// TODO: 阈值软截止 —— 达到轮次/时间阈值后暂停,询问用户(继续/停止/注入指令),而非强制中断
pub(crate) async fn agent_loop(
    client: &LlmClient,
    toolbox: &Toolbox,
    memory: &mut Memory,
    safety: &SafetyChecker,
) -> anyhow::Result<String> {
    // 循环上限:防模型反复调工具无限循环(如工具一直失败、模型钻牛角尖)
    const MAX_TURNS: usize = 20;

    for _ in 0..MAX_TURNS {
        let messages = memory.view().to_vec();
        let response = client.chat(messages).await?;

        match response {
            ChatResponse::Stop(msg) => {
                // 模型自然结束:落记忆,返回最终回复
                let content = msg.content.clone().unwrap_or_default();
                memory.add(assistant(&content));
                return Ok(content);
            }
            ChatResponse::ToolCalls(msg) => {
                // 回灌 assistant 消息(带 tool_calls),防模型失忆重复请求同一工具
                let tool_calls = msg.tool_calls.clone().unwrap_or_default();
                memory.add(assistant_with_tool_calls(tool_calls.clone()));

                // 逐个执行工具调用
                for call in &tool_calls {
                    let (id, name, args) = match call {
                        ChatCompletionMessageToolCalls::Function(f) => {
                            (f.id.clone(), f.function.name.clone(), f.function.arguments.clone())
                        }
                        ChatCompletionMessageToolCalls::Custom(c) => {
                            // custom tool 目前不支持,统一拒
                            memory.add(tool_result(&c.id, "不支持 custom tool 调用"));
                            continue;
                        }
                    };

                    // safety 拦截(目前 stub,接通后可加规则)
                    match safety.check(&name, &args) {
                        SafetyVerdict::Allow => {}
                        SafetyVerdict::Deny(reason) => {
                            memory.add(tool_result(&id, &format!("拒绝执行: {reason}")));
                            continue;
                        }
                        SafetyVerdict::AskUser(reason) => {
                            // 当前无交互注入通道,先按拒绝处理,留 TODO
                            // TODO: 接 CLI 交互层,向用户展示 reason 并等待确认
                            memory.add(tool_result(&id, &format!("需用户确认(暂拒): {reason}")));
                            continue;
                        }
                    }

                    // 分发执行
                    let result = match toolbox.find(&name) {
                        Some(tool) => tool.execute(&args).await,
                        None => Err(anyhow::anyhow!("未知工具: {name}")),
                    };

                    let content = match result {
                        Ok(s) => s,
                        Err(e) => format!("工具执行出错: {e}"),
                    };
                    memory.add(tool_result(&id, &content));
                }
                // 继续下一轮,把工具结果喂回模型
            }
            ChatResponse::Length(msg) => {
                // 被截断:无完整回复,落记忆后报错,由上层决定是否续传
                let content = msg.content.clone().unwrap_or_default();
                memory.add(assistant(&content));
                return Err(anyhow::anyhow!("回复被截断(length),考虑增大 max_tokens 或压缩记忆"));
            }
            ChatResponse::Filtered(msg) => {
                let content = msg.content.clone().unwrap_or_default();
                memory.add(assistant(&content));
                return Err(anyhow::anyhow!("回复被过滤或结束原因为空"));
            }
        }
    }

    Err(anyhow::anyhow!("达到最大轮次 {MAX_TURNS},agent 循环未收敛"))
}
