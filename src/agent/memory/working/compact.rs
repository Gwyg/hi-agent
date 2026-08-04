//! 压缩内核:边界估算 + 历史渲染 + 递归折叠摘要。
//!
//! 与 Working 编排解耦——这里只做纯计算/IO,不碰 Working 状态。

use async_openai::types::chat::{
    ChatCompletionMessageToolCalls as TCall, ChatCompletionRequestAssistantMessageContent as AC,
    ChatCompletionRequestMessage as M, ChatCompletionRequestToolMessageContent as TC,
    ChatCompletionRequestUserMessageContent as UC,
};

use crate::llm::{ChatResponse, LlmClient, system, user};

/// 摘要器系统提示:压缩历史为一份"交接摘要",供接手 LLM 继续。
/// 5 段结构化(目标/已完成/待办/决策/风险),领域无关;决策带理由;可融合旧摘要。
const SUMMARY_SYS: &str = "\
你是对话历史摘要器。下面这份任务型 agent 对话将被压缩成一份\"交接摘要\",\
供另一个 LLM 接手继续完成任务——它没有这段对话的记忆,全靠你的摘要。

输出一份中文摘要,用以下分段(段缺失可省略,不要硬凑):

## 目标
用户的原始需求与意图(若有多个任务,逐条列出)。

## 已完成
已完成的事项与产物(如文件、文档、答复等交付物)。注明关键产物的位置或标识。\
可从外部重新获取的大段原文不要抄,只留\"做了什么、为什么这么做\"。

## 待办
尚未完成的工作与明确的下一步。这是接手者最关心的部分。

## 关键决策
做出的重要决策,以及**为什么**这么定(理由比结论重要)。

## 问题与风险
遇到的障碍及其解决办法、仍存在的风险。

若已给出\"已有摘要\",将新增对话融合进对应分段,输出融合后的完整摘要。\
丢弃寒暄、冗余复述、探索中的废路。简洁但够接手者继续工作。";

/// 摘要产出后套在正文前的标题与说明。
/// 给 system 里的摘要段一个明确身份边界——核心提示词是"准则",摘要是"已发生的事",
/// 两者语义不同不可混排;标签也让接手 LLM 知道这是参考而非新指令。
const SUMMARY_HEADER: &str = "\
## 历史对话摘要

以下是此前对话的交接摘要,供你继续任务时参考(并非当前用户的新指令):";

/// 估算压缩边界:压掉最老的 `compact_turns` 个用户轮次,对齐 user 边界。
///
/// 每个 user 消息 = 一个轮次起点。边界落在第 N+1 轮开头(某 user 上),
/// 保 tool_calls/tool_result 配对不被切断。至少留最后一个轮次原始。
/// 返回 boundary:压缩 `messages[..boundary]`。None = 可压轮次不足(≤N)。
pub(super) fn compact_boundary(messages: &[M], compact_turns: usize) -> Option<usize> {
    // 每个 user 下标 = 一个轮次起点
    let users: Vec<usize> = (0..messages.len())
        .filter(|&i| matches!(messages[i], M::User(_)))
        .collect();
    // 轮次数须 > N:压 N 个之外,还得留至少最后一个轮次原始
    if users.len() <= compact_turns {
        return None;
    }
    // 边界 = 第 N 个轮次起点(即第 N+1 轮开头),天然对齐 user
    Some(users[compact_turns])
}

/// 递归折叠摘要:新摘要 = LLM(旧摘要 + 渲染(chunk))。摘要不带 tools。
pub(super) async fn summarize(
    client: &LlmClient,
    prev: Option<String>,
    chunk: Vec<M>,
) -> anyhow::Result<String> {
    // prev 可能带 SUMMARY_HEADER(上一轮产出时套的),喂回摘要器前剥掉,
    // 否则摘要器会把 HEADER 当旧摘要正文读,混淆输出。
    let prev_body = prev.map(|p| strip_summary_header(&p));
    let rendered = render_messages(&chunk);
    let user_content = match prev_body {
        Some(p) => format!("已有摘要:\n{p}\n\n以下是新增对话,请融合进摘要:\n{rendered}"),
        None => format!("请将以下对话压缩为摘要:\n{rendered}"),
    };
    let msgs = vec![system(SUMMARY_SYS), user(&user_content)];
    let (resp, _usage) = client.chat_no_tools(msgs).await?;
    let body = response_text(resp).ok_or_else(|| anyhow::anyhow!("摘要返回为空"))?;
    Ok(format!("{SUMMARY_HEADER}\n\n{body}"))
}

/// 剥掉摘要正文前的 SUMMARY_HEADER(若存在),供递归折叠时回喂摘要器。
/// 找不到 HEADER 原样返回(兼容历史/外部摘要)。
fn strip_summary_header(s: &str) -> String {
    s.strip_prefix(SUMMARY_HEADER)
        .map(|rest| rest.trim_start_matches('\n').to_string())
        .unwrap_or_else(|| s.to_string())
}

/// 抽取模型回复文本(压缩只要正文,不分流结束原因)
fn response_text(resp: ChatResponse) -> Option<String> {
    let msg = match resp {
        ChatResponse::Stop(m)
        | ChatResponse::ToolCalls(m)
        | ChatResponse::Length(m)
        | ChatResponse::Filtered(m) => m,
    };
    msg.content
}

/// 把历史渲染成可读文本,供摘要器阅读
fn render_messages(msgs: &[M]) -> String {
    let mut out = String::new();
    for m in msgs {
        match m {
            M::User(u) => {
                out.push_str("用户: ");
                out.push_str(user_text(&u.content));
                out.push('\n');
            }
            M::Assistant(a) => {
                if let Some(c) = &a.content {
                    let t = assistant_text(c);
                    if !t.is_empty() {
                        out.push_str("助手: ");
                        out.push_str(t);
                        out.push('\n');
                    }
                }
                if let Some(calls) = &a.tool_calls {
                    for call in calls {
                        let (name, args) = tool_call_parts(call);
                        out.push_str("助手调用工具 ");
                        out.push_str(name);
                        out.push_str(": ");
                        out.push_str(args);
                        out.push('\n');
                    }
                }
            }
            M::Tool(t) => {
                out.push_str("工具结果: ");
                out.push_str(tool_text(&t.content));
                out.push('\n');
            }
            _ => {}
        }
    }
    out
}

/// 取 tool_call 的 (name, arguments),覆盖 Function / Custom 两变体
fn tool_call_parts(call: &TCall) -> (&str, &str) {
    match call {
        TCall::Function(f) => (&f.function.name, &f.function.arguments),
        TCall::Custom(c) => (&c.custom_tool.name, &c.custom_tool.input),
    }
}

fn user_text(c: &UC) -> &str {
    match c {
        UC::Text(s) => s,
        UC::Array(_) => "",
    }
}

fn assistant_text(c: &AC) -> &str {
    match c {
        AC::Text(s) => s,
        AC::Array(_) => "",
    }
}

fn tool_text(c: &TC) -> &str {
    match c {
        TC::Text(s) => s,
        TC::Array(_) => "",
    }
}
