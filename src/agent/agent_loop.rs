use crate::llm::{
    ChatResponse, LlmClient, Toolbox, assistant, assistant_with_tool_calls, tool_result,
};
use async_openai::types::chat::ChatCompletionMessageToolCalls;
use futures::{StreamExt, stream};
use tokio::sync::{mpsc, oneshot};

use super::memory::Memory;
use super::{AskReply, EngineEvent};
use crate::llm::tools::{Action, Tool};

/// agent 循环:chat ↔ 工具,直到模型给出最终回复(Stop)
/// 通过 Memory 的 add/view 与记忆交互,压缩由 Memory 内部接管,本循环不感知
/// 通过 emit 向前端推送事件流(token 增量/工具进度/Ask 确认/完成/出错)
// TODO: 阈值软截止 —— 达到轮次/时间阈值后暂停,询问用户(继续/停止/注入指令),而非强制中断
pub(crate) async fn agent_loop(
    client: &LlmClient,
    toolbox: &Toolbox,
    memory: &mut Memory,
    emit: mpsc::Sender<EngineEvent>,
) -> anyhow::Result<()> {
    // 循环上限:防模型反复调工具无限循环(如工具一直失败、模型钻牛角尖)
    const MAX_TURNS: usize = 20;
    // 并行执行上限:同时最多跑几个工具(read 有 spawn_blocking 同步 IO,防阻塞池打爆)
    const MAX_CONCURRENT: usize = 6;

    for turn in 0..MAX_TURNS {
        // view 内部:等待进行中的压缩、组装压缩视图(硬截断兜底)
        let messages = memory.view().await;
        let msg_count = messages.len();
        tracing::info!(turn, msg_count, "agent 轮次开始");
        // 流式调用:回调里 try_send TokenDelta(channel 满则丢弃,避免阻塞 LLM 流)
        let (response, usage) = client
            .chat_stream(messages, |token| {
                let _ = emit.try_send(EngineEvent::TokenDelta(token.to_string()));
            })
            .await?;

        match response {
            ChatResponse::Stop(msg) => {
                // 模型自然结束:落记忆(带 usage),推送 Done,返回
                let content = msg.content.clone().unwrap_or_default();
                tracing::info!(turn, content_len = content.chars().count(), "模型结束(Stop)");
                memory.add_response(assistant(&content), usage, client);
                let _ = emit.send(EngineEvent::Done(content)).await;
                return Ok(());
            }
            ChatResponse::ToolCalls(msg) => {
                // 回灌 assistant 消息(带 tool_calls + usage):防失忆,并触发后台压缩
                let tool_calls = msg.tool_calls.clone().unwrap_or_default();
                tracing::info!(turn, tool_count = tool_calls.len(), "模型请求工具调用");
                memory.add_response(
                    assistant_with_tool_calls(tool_calls.clone()),
                    usage,
                    client,
                );

                // 预解析 + 预检:拆成两组——Allow 组并发执行,Ask/Deny 组串行
                // (单确认条 UI,Ask 需逐个问);未知/custom 当场记错误,不参与执行。
                // 结果最终统一按原顺序回灌,模型视角与串行一致。
                let total = tool_calls.len();
                let mut parallel: Vec<(usize, String, String, String, &dyn Tool)> = Vec::new();
                let mut serial: Vec<(usize, String, String, String)> = Vec::new();
                for (idx, call) in tool_calls.iter().enumerate() {
                    let (id, name, args) = match call {
                        ChatCompletionMessageToolCalls::Function(f) => (
                            f.id.clone(),
                            f.function.name.clone(),
                            f.function.arguments.clone(),
                        ),
                        ChatCompletionMessageToolCalls::Custom(c) => {
                            memory.add(tool_result(&c.id, "不支持 custom tool 调用"));
                            continue;
                        }
                    };
                    match toolbox.find(&name) {
                        None => {
                            memory.add(tool_result(&id, &format!("未知工具: {name}")));
                            continue;
                        }
                        Some(tool) => match tool.assess(&args) {
                            Action::Allow => parallel.push((idx, id, name, args, tool)),
                            // Ask/Deny 走 handle_tool_call(内部含授权检查/Ask 询问/Deny 拒绝)
                            _ => serial.push((idx, id, name, args)),
                        },
                    }
                }
                tracing::info!(
                    turn,
                    parallel = parallel.len(),
                    serial = serial.len(),
                    "工具调用:并行组 + 串行组混合执行"
                );

                // 并行组:先全部发 ToolStart,再并发执行
                for (_, id, name, args, _) in &parallel {
                    let _ = emit
                        .send(EngineEvent::ToolStart {
                            id: id.clone(),
                            name: name.clone(),
                            args: args.clone(),
                        })
                        .await;
                }
                let futures: Vec<_> = parallel
                    .iter()
                    .enumerate()
                    .map(|(i, (_, _, _, args, tool))| {
                        let args = args.clone();
                        async move {
                            // 返回 (组内下标, 结果):buffer_unordered 按完成顺序返回,需用下标归位
                            let content = match tool.execute(&args).await {
                                Ok(s) => s,
                                Err(e) => format!("工具执行出错: {e}"),
                            };
                            (i, content)
                        }
                    })
                    .collect();
                // 并发限流:同时最多 MAX_CONCURRENT 个工具执行(防阻塞池/子进程被打爆)
                let results: Vec<_> =
                    stream::iter(futures).buffer_unordered(MAX_CONCURRENT).collect().await;

                // 串行组:逐个执行(handle_tool_call 内部 emit ToolStart + Ask 询问)
                let mut serial_results: Vec<(usize, String, String)> = Vec::new();
                for (idx, id, name, args) in &serial {
                    let content = match handle_tool_call(toolbox, &emit, id, name, args).await {
                        Ok(s) => s,
                        Err(e) => format!("工具执行出错: {e}"),
                    };
                    serial_results.push((*idx, id.clone(), content));
                }

                // 按原顺序合并回灌:memory + ToolResult 事件
                let mut ordered: Vec<Option<(String, String)>> = vec![None; total];
                for (i, content) in results {
                    let (idx, id, _, _, _) = &parallel[i];
                    ordered[*idx] = Some((id.clone(), content));
                }
                for (idx, id, content) in serial_results {
                    ordered[idx] = Some((id, content));
                }
                for entry in ordered.into_iter().flatten() {
                    let (id, content) = entry;
                    memory.add(tool_result(&id, &content));
                    let _ = emit
                        .send(EngineEvent::ToolResult {
                            id: id.clone(),
                            content: content.clone(),
                        })
                        .await;
                }
                // 继续下一轮,把工具结果喂回模型
            }
            ChatResponse::Length(msg) => {
                // 被截断:无完整回复,落记忆后报错,由上层决定是否续传
                let content = msg.content.clone().unwrap_or_default();
                tracing::warn!(turn, "回复被截断(length),考虑增大 max_tokens 或压缩记忆");
                memory.add(assistant(&content));
                let _ = emit
                    .send(EngineEvent::Error(
                        "回复被截断(length),考虑增大 max_tokens 或压缩记忆".to_string(),
                    ))
                    .await;
                return Err(anyhow::anyhow!(
                    "回复被截断(length),考虑增大 max_tokens 或压缩记忆"
                ));
            }
            ChatResponse::Filtered(msg) => {
                let content = msg.content.clone().unwrap_or_default();
                tracing::warn!(turn, "回复被过滤或结束原因为空");
                memory.add(assistant(&content));
                let _ = emit
                    .send(EngineEvent::Error("回复被过滤或结束原因为空".to_string()))
                    .await;
                return Err(anyhow::anyhow!("回复被过滤或结束原因为空"));
            }
        }
    }

    tracing::warn!("达到最大轮次 {MAX_TURNS},agent 循环未收敛");
    let _ = emit
        .send(EngineEvent::Error(format!(
            "达到最大轮次 {MAX_TURNS},agent 循环未收敛"
        )))
        .await;
    Err(anyhow::anyhow!("达到最大轮次 {MAX_TURNS},agent 循环未收敛"))
}

/// 处理单个工具调用:emit ToolStart → find → assess → (授权/询问) → execute
/// 内部推送 ToolStart / Ask 事件;返回执行结果 content(Ok)或错误原因(Err)
/// 调用方(agent_loop)负责 memory.add(tool_result) 和 emit ToolResult
async fn handle_tool_call(
    toolbox: &Toolbox,
    emit: &mpsc::Sender<EngineEvent>,
    id: &str,
    name: &str,
    args: &str,
) -> anyhow::Result<String> {
    // 通知前端:工具开始
    let _ = emit
        .send(EngineEvent::ToolStart {
            id: id.to_string(),
            name: name.to_string(),
            args: args.to_string(),
        })
        .await;

    // args 摘要:防刷屏 + 不泄完整密钥,截到 200 字符
    let args_summary: String = args.chars().take(200).collect();
    tracing::info!(tool = name, id, args = %args_summary, "工具调用开始");

    let tool = toolbox
        .find(name)
        .ok_or_else(|| anyhow::anyhow!("未知工具: {name}"))?;

    match tool.assess(args) {
        Action::Allow => {
            tracing::debug!(tool = name, id, "assess=Allow,直接执行");
            tool.execute(args).await
        }
        Action::Deny(reason) => {
            tracing::warn!(tool = name, id, reason = %reason, "assess=Deny,拒绝执行");
            Err(anyhow::anyhow!("拒绝执行: {reason}"))
        }
        Action::Ask { persistable, keys } => {
            // persistable=true 且 keys 非空:先查会话授权,全命中跳过询问
            // keys 由 assess 用 AST 拆子命令生成,只含触发 Ask 的子命令(精确,不过度授权)
            let granted =
                persistable && !keys.is_empty() && keys.iter().all(|k| toolbox.grant_check(k));
            if granted {
                tracing::debug!(tool = name, id, "会话授权命中,跳过询问");
                return tool.execute(args).await;
            }
            tracing::info!(tool = name, id, persistable, keys = ?keys, "需用户确认(Ask)");
            // 未授权或 persistable=false,emit Ask 等用户决策
            let (tx_reply, rx_reply) = oneshot::channel();
            let _ = emit
                .send(EngineEvent::Ask {
                    id: id.to_string(),
                    prompt: format!("{name} {args}"),
                    persistable,
                    reply: tx_reply,
                })
                .await;
            match rx_reply.await {
                Ok(AskReply::One) => {
                    tracing::info!(tool = name, id, "用户确认:仅本次");
                    tool.execute(args).await
                }
                Ok(AskReply::Always) => {
                    // 登记授权键(assess 提供的 keys,只含触发 Ask 的子命令)
                    tracing::info!(tool = name, id, keys = ?keys, "用户确认:永久授权(Always)");
                    if persistable {
                        for k in &keys {
                            toolbox.grant_record(k.clone());
                        }
                    }
                    tool.execute(args).await
                }
                Ok(AskReply::Deny) => {
                    tracing::warn!(tool = name, id, "用户拒绝");
                    Err(anyhow::anyhow!("用户拒绝"))
                }
                // 前端未响应(reply 被 drop):暂拒,保持原行为
                Err(_) => {
                    tracing::warn!(tool = name, id, "Ask reply 被 drop,暂拒");
                    Err(anyhow::anyhow!("需用户确认(暂拒): 该操作需人工确认"))
                }
            }
        }
    }
}
