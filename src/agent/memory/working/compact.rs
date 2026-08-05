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
用户的原始需求与意图(若有多个任务,逐条列出)。后续用户追加/修改/废弃的任务须反映为最新意图。

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

/// 估算一条消息的字节数(Debug 串长度,粗略代表大小)。仅用于压缩边界估算,不计费。
/// 注意是字节非字符:中文 1 字 = 3 字节,让中英差异下"字节/token"比更接近,利于统一估算。
pub(super) fn est_bytes(m: &M) -> usize {
    format!("{m:?}").len()
}

fn is_user(m: &M) -> bool {
    matches!(m, M::User(_))
}

fn is_tool(m: &M) -> bool {
    matches!(m, M::Tool(_))
}

fn is_assistant(m: &M) -> bool {
    matches!(m, M::Assistant(_))
}

/// 规划压缩范围:头部连续段进摘要,尾部连续段保留。
///
/// 返回被压缩的 chunk(头部 `messages[0..cut]` 的副本),`messages` 不动。
/// 调用方 apply 时 `drain(0..chunk.len())`,保留 `[chunk.len()..)`。
///
/// 算法:按组正向扫描累字节,累够即切。AI 和其所有 tool 结果整组同进同出,天然配对闭合。
/// - 切点优先落在 user 边界(切后 messages[0]=user,国内外协议合规);
/// - 单 user 长任务扫不到第二个 user 时,切在累够处的完整组之后(切后可能以 AI 开头,
///   由协议适配层补占位 user 兜底);
/// - 尾部保护:保留部分至少 `min_keep_tail_bytes`,保住近期原文。
///
/// None = 没找到满足条件的切点(可压区不足或扫完仍累不够),交 hard_truncate 兜底。
pub(super) fn compact_range(
    messages: &[M],
    reclaim_bytes: usize,
    min_keep_tail_bytes: usize,
) -> Option<Vec<M>> {
    let n = messages.len();
    if n < 2 {
        return None;
    }

    let mut reclaimed = 0usize;
    let mut i = 0usize;

    while i < n {
        let m = &messages[i];

        if is_user(m) {
            // 切点候选:累够 + 尾部留足 → 切在此 user 前(它保留,messages[0]=user 合规)
            if reclaimed >= reclaim_bytes && tail_bytes(&messages[i..]) >= min_keep_tail_bytes {
                return Some(messages[..i].to_vec());
            }
            reclaimed += est_bytes(m);
            i += 1;
        } else if is_assistant(m) {
            // 整组扫描:assistant + 其紧随的所有 tool 结果(天然配对完整)
            let ge = group_end(messages, i);
            reclaimed += (i..ge).map(|j| est_bytes(&messages[j])).sum::<usize>();

            // 累够 + 尾部留足:优先找下个 user 切;没有就切在组后(适配层补占位)
            if reclaimed >= reclaim_bytes {
                let tail = &messages[ge..];
                if tail_bytes(tail) >= min_keep_tail_bytes {
                    if let Some(next_user) = tail.iter().position(is_user) {
                        return Some(messages[..ge + next_user].to_vec()); // 切在某 user 前
                    }
                    return Some(messages[..ge].to_vec()); // 切在组后,适配层补占位 user
                }
            }
            i = ge;
        } else {
            i += 1;
        }
    }
    None
}

/// 找 assistant 组的结束下标:assistant + 其紧随的所有 tool 结果
fn group_end(messages: &[M], start: usize) -> usize {
    let mut j = start + 1;
    while j < messages.len() && is_tool(&messages[j]) {
        j += 1;
    }
    j
}

/// 计算切片的总字节数
fn tail_bytes(msgs: &[M]) -> usize {
    msgs.iter().map(est_bytes).sum()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{assistant, assistant_with_tool_calls, tool_result, user};
    use async_openai::types::chat::{
        ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls, FunctionCall,
    };

    fn big(prefix: &str) -> String {
        format!("{prefix}-{}", "x".repeat(1000))
    }

    /// 构造带 N 条 tool_call 的 assistant(一次调用多个工具的规范形态)
    fn assistant_tc(ids: &[&str]) -> M {
        let calls: Vec<_> = ids
            .iter()
            .map(|id| {
                ChatCompletionMessageToolCalls::Function(ChatCompletionMessageToolCall {
                    id: id.to_string(),
                    function: FunctionCall {
                        name: "dummy".to_string(),
                        arguments: "{}".to_string(),
                    },
                })
            })
            .collect();
        assistant_with_tool_calls(calls)
    }

    // 多 user 样本:切点优先落在某 user 边界(满足国内外协议)
    // [U1, a_tc(c1,c2), T(c1), T(c2), U2, a_tc(c3,c4), T(c3), T(c4), U3, a_tc(c5,c6), T(c5), T(c6)]
    fn sample() -> Vec<M> {
        vec![
            user(&big("task1")),
            assistant_tc(&["c1", "c2"]),
            tool_result("c1", &big("r1")),
            tool_result("c2", &big("r2")),
            user(&big("task2")),
            assistant_tc(&["c3", "c4"]),
            tool_result("c3", &big("r3")),
            tool_result("c4", &big("r4")),
            user(&big("task3")),
            assistant_tc(&["c5", "c6"]),
            tool_result("c5", &big("r5")),
            tool_result("c6", &big("r6")),
        ]
    }

    /// 校验配对完整:每个 assistant 组 [start, group_end) 不得跨越 cut
    fn assert_pairs_intact(msgs: &[M], cut: usize) {
        let mut i = 0;
        while i < msgs.len() {
            if is_assistant(&msgs[i]) {
                let ge = group_end(msgs, i);
                assert!(
                    ge <= cut || i >= cut,
                    "切点 {} 劈开了 assistant 组 [{},{})",
                    cut,
                    i,
                    ge
                );
                i = ge;
            } else {
                i += 1;
            }
        }
    }

    /// 校验切后 messages[0] 必为 user(满足国内外所有厂商协议)
    fn assert_starts_with_user(msgs: &[M], cut: usize) {
        assert!(
            cut < msgs.len() && is_user(&msgs[cut]),
            "切点 {} 处不是 user,切后 messages[0] 不合规(由适配层补占位兜底)",
            cut
        );
    }

    #[test]
    fn cut_at_user_boundary_and_pairs_intact() {
        let msgs = sample();
        let chunk = compact_range(&msgs, 1500, 1500).expect("应可压");
        let cut = chunk.len();
        assert!(cut < msgs.len(), "尾部近期原文应保留");
        assert_starts_with_user(&msgs, cut);
        assert_pairs_intact(&msgs, cut);
    }

    #[test]
    fn too_small_returns_none() {
        let msgs = vec![user("hi"), assistant("ok")];
        assert_eq!(compact_range(&msgs, 1000, 10_000), None);
    }

    /// 单 user 长任务:累够后切在某组之后(无第二个 user),配对完整,适配层补占位
    #[test]
    fn single_user_cuts_at_group_end() {
        let msgs = vec![
            user(&big("task")),
            assistant_tc(&["c1", "c2"]),
            tool_result("c1", &big("r1")),
            tool_result("c2", &big("r2")),
            assistant_tc(&["c3"]),
            tool_result("c3", &big("r3")),
            assistant_tc(&["c4"]),
            tool_result("c4", &big("r4")),
        ];
        let res = compact_range(&msgs, 1500, 1500);
        assert!(res.is_some(), "单 user 累够应能切(适配层补占位)");
        if let Some(chunk) = res {
            assert_pairs_intact(&msgs, chunk.len());
        }
    }

    /// #4:同一 assistant 的多条 T 不被中间切开
    #[test]
    fn multi_tool_result_not_split() {
        let msgs = vec![
            user(&big("task1")),
            assistant_tc(&["c1", "c2"]),
            tool_result("c1", &big("r1")),
            tool_result("c2", &big("r2")),
            user(&big("task2")),
            assistant_tc(&["c3"]),
            tool_result("c3", &big("r3")),
            user(&big("task3")),
            assistant_tc(&["c4"]),
            tool_result("c4", &big("r4")),
        ];
        let chunk = compact_range(&msgs, 2500, 1500).expect("应可压");
        assert_pairs_intact(&msgs, chunk.len());
        assert_starts_with_user(&msgs, chunk.len());
    }

    /// #2:尾部巨型 Tool,切点不劈开 c2+r2 配对
    #[test]
    fn big_tool_in_tail_pairs_intact() {
        let msgs = vec![
            user(&big("task1")),
            assistant_tc(&["c1"]),
            tool_result("c1", &big("r1")),
            user(&big("task2")),
            assistant_tc(&["c2"]),
            tool_result("c2", &big("r2_huge")),
        ];
        if let Some(chunk) = compact_range(&msgs, 1500, 1500) {
            assert_pairs_intact(&msgs, chunk.len());
        }
    }

    /// 多轮纯对话:切点落在第二个 user 边界
    #[test]
    fn multi_turn_plain_chat() {
        let msgs = vec![
            user(&big("q1")),
            assistant(&big("a1")),
            user(&big("q2")),
            assistant(&big("a2")),
            user(&big("q3")),
            assistant(&big("a3")),
        ];
        let chunk = compact_range(&msgs, 1500, 1500).expect("多轮纯对话应可压");
        let cut = chunk.len();
        assert!(cut < msgs.len());
        assert_starts_with_user(&msgs, cut);
        assert_pairs_intact(&msgs, cut);
    }
}
