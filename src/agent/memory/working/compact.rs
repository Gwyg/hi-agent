//! 压缩内核:边界估算 + 历史渲染 + 递归折叠摘要。
//!
//! 与 Working 编排解耦——这里只做纯计算/IO,不碰 Working 状态。

use async_openai::types::chat::{
    ChatCompletionMessageToolCalls as TCall, ChatCompletionRequestAssistantMessageContent as AC,
    ChatCompletionRequestMessage as M, ChatCompletionRequestToolMessageContent as TC,
    ChatCompletionRequestUserMessageContent as UC,
};

use crate::llm::{ChatResponse, LlmClient, system, user};

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

/// 估算一条消息的字节数(Debug 串长度,粗略代表大小)。仅用于 push 时缓存,不计费。
/// 注意是字节非字符:中文 1 字 = 3 字节,让中英差异下"字节/token"比更接近,利于统一估算。
pub(super) fn estimate_bytes(m: &M) -> usize {
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

/// 压缩计划:被压消息 chunk + 保留策略。
///
/// 保留区 = (可选)messages[keep_extra_user] + messages[keep_from..]
/// - 第1段:keep_extra_user=None,保留区连续 [keep_from..]
/// - 第2段:keep_extra_user=Some(uk),保留 messages[uk] + messages[keep_from..]
pub(super) struct CompactionPlan {
    /// 进摘要的消息(owned,喂给摘要器)
    pub chunk: Vec<M>,
    /// 保留 messages[keep_from..]
    pub keep_from: usize,
    /// 额外保留的单条 user 消息下标(第2段保最后一个 user 用)
    pub keep_extra_user: Option<usize>,
}

/// 选出进摘要的消息 + 保留策略。
///
/// # 不变量
/// - 保留区恒以 user 开头(第1段切点落 user 边界;第2段保 uk=user)
/// - assistant 组整组同进同出,配对闭合
/// - 至少保留最后一个 user + 最后一组 assistant(防断片)
///
/// # 两段式切点
/// 1. 优先压历史完整轮次:从 u0 起逐轮纳入,够 reclaim 即切在某 user 前
/// 2. 仍不够 → 压最后一个 user 之后的 assistant 组中段(保 uk + 最后一组)
///
/// None = 无合法切点(消息过少 / 可压区不足),交 hard_truncate 兜底。
pub(super) fn select_to_compress(
    messages: &[M],
    msg_bytes: &[usize],
    reclaim_bytes: usize,
) -> Option<CompactionPlan> {
    debug_assert_eq!(
        messages.len(),
        msg_bytes.len(),
        "msg_bytes 必须与 messages 同长"
    );
    let n = messages.len();
    if n < 2 || reclaim_bytes == 0 {
        return None;
    }

    // 最后一个 user 下标(当前任务指令,必保留)。无 user → 无可保,不压。
    let last_user = messages.iter().rposition(is_user)?;

    // 第1段:压历史完整轮次 [0..last_user)
    if let Some(cut) = cut_history_rounds(messages, msg_bytes, reclaim_bytes, last_user) {
        return Some(CompactionPlan {
            chunk: messages[..cut].to_vec(),
            keep_from: cut,
            keep_extra_user: None,
        });
    }

    // 第2段:压 last_user 之后的 assistant 组中段
    cut_last_user_tail(messages, msg_bytes, reclaim_bytes, last_user)
}

/// 第1段:在 [0..last_user) 内逐轮累字节,够且尾部留足 → 返回切点(user 边界)。
/// 扫到 last_user 仍不够 → 返回 None(交第2段)。
fn cut_history_rounds(
    messages: &[M],
    msg_bytes: &[usize],
    reclaim_bytes: usize,
    last_user: usize,
) -> Option<usize> {
    let mut reclaimed = 0usize;
    let mut i = 0usize;
    while i < last_user {
        if is_user(&messages[i]) {
            // 累够 → 切在此 user 前(它保留,messages[0]=user 合规)
            if reclaimed >= reclaim_bytes {
                return Some(i);
            }
            reclaimed += msg_bytes[i];
            i += 1;
        } else if is_assistant(&messages[i]) {
            // 整组扫描:assistant + 紧随的所有 tool 结果
            let ge = assistant_group_end(messages, i);
            reclaimed += sum_range(msg_bytes, i, ge);
            i = ge;
        } else {
            i += 1;
        }
    }
    None
}

/// 第2段:压 last_user 之前的全部历史 + last_user 之后的 assistant 组中段。
/// 保 last_user(uk) + 最后一组 assistant(An)。
/// 逐组纳入中段,够即停;中段全压光仍不够 → 压光中段(滚动收敛,下轮再压)。
/// 只有一组(无中段)→ None。
fn cut_last_user_tail(
    messages: &[M],
    msg_bytes: &[usize],
    reclaim_bytes: usize,
    last_user: usize,
) -> Option<CompactionPlan> {
    // uk 之后最后一条 assistant = 最后一组起点(必保留)
    let last_gs = messages.iter().rposition(is_assistant)?;
    // 最后一组须在 uk 之后,且 uk 与最后一组之间至少有一组中段可压
    if last_gs <= last_user + 1 {
        return None;
    }

    // 基础字节:uk 之前的全部历史(第2段也压这部分)
    let base_bytes: usize = sum_range(msg_bytes, 0, last_user);

    // 逐组纳入中段,够即停
    let mut reclaimed = base_bytes;
    let mut i = last_user + 1;
    while i < last_gs {
        if is_assistant(&messages[i]) {
            let ge = assistant_group_end(messages, i);
            reclaimed += sum_range(msg_bytes, i, ge);
            if reclaimed >= reclaim_bytes {
                // chunk 含 uk,apply_keep 时按 keep_extra_user 标记捞回 uk
                return Some(CompactionPlan {
                    chunk: messages[..ge].to_vec(),
                    keep_from: ge,
                    keep_extra_user: Some(last_user),
                });
            }
            i = ge;
        } else {
            i += 1;
        }
    }
    // 中段全压光仍不够 → 压光中段,保留最后一组(滚动收敛)
    Some(CompactionPlan {
        chunk: messages[..last_gs].to_vec(),
        keep_from: last_gs,
        keep_extra_user: Some(last_user),
    })
}

/// 找 assistant 组的结束下标:assistant + 其紧随的所有 tool 结果
fn assistant_group_end(messages: &[M], start: usize) -> usize {
    let mut j = start + 1;
    while j < messages.len() && is_tool(&messages[j]) {
        j += 1;
    }
    j
}

/// 计算字节缓存区间 [start, end) 的和。纯数字累加,零分配。
fn sum_range(msg_bytes: &[usize], start: usize, end: usize) -> usize {
    let end = end.min(msg_bytes.len());
    let start = start.min(end);
    msg_bytes[start..end].iter().sum()
}

/// 递归折叠摘要:新摘要 = LLM(旧摘要 + 渲染(chunk))。摘要不带 tools。
/// 返回纯正文(不含 HEADER),HEADER 由 Memory 拼 system 时加。
pub(super) async fn summarize(
    client: &LlmClient,
    prev: Option<String>,
    chunk: Vec<M>,
) -> anyhow::Result<String> {
    let rendered = render_messages(&chunk);
    let user_content = match prev {
        Some(p) => format!("已有摘要:\n{p}\n\n以下是新增对话,请融合进摘要:\n{rendered}"),
        None => format!("请将以下对话压缩为摘要:\n{rendered}"),
    };
    let msgs = vec![system(SUMMARY_SYS), user(&user_content)];
    let (resp, _usage) = client.chat_no_tools(msgs).await?;
    response_text(resp).ok_or_else(|| anyhow::anyhow!("摘要返回为空"))
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

/// 把历史渲染成可读文本,供摘要器阅读。
/// 初始容量按 chunk 的 estimate_bytes 总和的一半预估(render 文本约为 Debug 串的 40-60%),
/// 避免反复扩容。仅在压缩触发时调一次,非热路径。
fn render_messages(msgs: &[M]) -> String {
    let cap: usize = msgs.iter().map(estimate_bytes).sum::<usize>() / 2;
    let mut out = String::with_capacity(cap);
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

    /// 给 messages 同步建字节缓存(模拟 Working::push 的行为)
    fn with_bytes(msgs: Vec<M>) -> (Vec<M>, Vec<usize>) {
        let bytes: Vec<usize> = msgs.iter().map(estimate_bytes).collect();
        (msgs, bytes)
    }

    // 多 user 样本:[U1, A+T*, U2, A+T*, U3, A+T*]
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

    /// 校验保留区以 user 开头(满足国内外协议)
    fn assert_kept_starts_with_user(msgs: &[M], plan: &CompactionPlan) {
        let kept_first = plan
            .keep_extra_user
            .map(|idx| &msgs[idx])
            .unwrap_or(&msgs[plan.keep_from]);
        assert!(
            is_user(kept_first),
            "保留区首条非 user(keep_from={}, extra={:?})",
            plan.keep_from,
            plan.keep_extra_user
        );
    }

    /// 校验配对完整:被压区间不得劈开任何 assistant 组
    fn assert_pairs_intact(msgs: &[M], plan: &CompactionPlan) {
        // 被压下标集合 = chunk 对应的原始位置
        // 第1段:[0..keep_from]
        // 第2段:[0..uk] + [uk+1..keep_from]
        let mut compressed: Vec<bool> = vec![false; msgs.len()];
        let uk = plan.keep_extra_user;
        let end = plan.keep_from;
        for i in 0..end {
            if uk == Some(i) {
                continue; // uk 保留
            }
            compressed[i] = true;
        }
        // 每个 assistant 组要么全压要么全留
        let mut i = 0;
        while i < msgs.len() {
            if is_assistant(&msgs[i]) {
                let ge = assistant_group_end(msgs, i);
                let group_compressed = compressed[i];
                for j in i..ge {
                    assert_eq!(
                        compressed[j], group_compressed,
                        "切点劈开了 assistant 组 [{},{})",
                        i, ge
                    );
                }
                i = ge;
            } else {
                i += 1;
            }
        }
    }

    #[test]
    fn cut_at_user_boundary_and_pairs_intact() {
        let (msgs, bytes) = with_bytes(sample());
        let plan = select_to_compress(&msgs, &bytes, 1500).expect("应可压");
        assert!(plan.keep_from < msgs.len(), "尾部近期原文应保留");
        assert_kept_starts_with_user(&msgs, &plan);
        assert_pairs_intact(&msgs, &plan);
    }

    #[test]
    fn too_small_returns_none() {
        let (msgs, bytes) = with_bytes(vec![user("hi"), assistant("ok")]);
        assert!(select_to_compress(&msgs, &bytes, 1000).is_none());
    }

    /// 单 user 长任务:压 last_user 之后的 assistant 组中段,保 last_user + 最后一组
    #[test]
    fn single_user_compress_tail_mid() {
        let (msgs, bytes) = with_bytes(vec![
            user(&big("task")),
            assistant_tc(&["c1", "c2"]),
            tool_result("c1", &big("r1")),
            tool_result("c2", &big("r2")),
            assistant_tc(&["c3"]),
            tool_result("c3", &big("r3")),
            assistant_tc(&["c4"]),
            tool_result("c4", &big("r4")),
        ]);
        let plan =
            select_to_compress(&msgs, &bytes, 1500).expect("单 user 多组应能压中段");
        // last_user=0,保 messages[0] + 最后一组(c4)
        assert_eq!(plan.keep_extra_user, Some(0));
        assert_kept_starts_with_user(&msgs, &plan);
        assert_pairs_intact(&msgs, &plan);
        // chunk 非空:含 uk 之前(空)+ uk 之后中段
        assert!(!plan.chunk.is_empty(), "chunk 不应为空");
    }

    /// 单 user 仅一组:无中段可压 → None
    #[test]
    fn single_user_one_group_returns_none() {
        let (msgs, bytes) = with_bytes(vec![
            user(&big("task")),
            assistant_tc(&["c1"]),
            tool_result("c1", &big("r1")),
        ]);
        assert!(select_to_compress(&msgs, &bytes, 1500).is_none());
    }

    /// 同一 assistant 的多条 T 不被中间切开
    #[test]
    fn multi_tool_result_not_split() {
        let (msgs, bytes) = with_bytes(vec![
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
        ]);
        let plan = select_to_compress(&msgs, &bytes, 2500).expect("应可压");
        assert_pairs_intact(&msgs, &plan);
        assert_kept_starts_with_user(&msgs, &plan);
    }

    /// 尾部巨型 Tool,切点不劈开配对
    #[test]
    fn big_tool_in_tail_pairs_intact() {
        let (msgs, bytes) = with_bytes(vec![
            user(&big("task1")),
            assistant_tc(&["c1"]),
            tool_result("c1", &big("r1")),
            user(&big("task2")),
            assistant_tc(&["c2"]),
            tool_result("c2", &big("r2_huge")),
        ]);
        if let Some(plan) = select_to_compress(&msgs, &bytes, 1500) {
            assert_pairs_intact(&msgs, &plan);
            assert_kept_starts_with_user(&msgs, &plan);
        }
    }

    /// 多轮纯对话:切点落在某 user 边界
    #[test]
    fn multi_turn_plain_chat() {
        let (msgs, bytes) = with_bytes(vec![
            user(&big("q1")),
            assistant(&big("a1")),
            user(&big("q2")),
            assistant(&big("a2")),
            user(&big("q3")),
            assistant(&big("a3")),
        ]);
        let plan = select_to_compress(&msgs, &bytes, 1500).expect("多轮纯对话应可压");
        assert!(plan.keep_from < msgs.len());
        assert_kept_starts_with_user(&msgs, &plan);
        assert_pairs_intact(&msgs, &plan);
    }

    /// 第2段保底:中段全压光仍不够,保留最后一组
    #[test]
    fn tail_compress_all_mid_keeps_last_group() {
        let (msgs, bytes) = with_bytes(vec![
            user(&big("task")),
            assistant_tc(&["c1"]),
            tool_result("c1", "r1"),
            assistant_tc(&["c2"]),
            tool_result("c2", "r2"),
            assistant_tc(&["c3"]),
            tool_result("c3", "r3"),
        ]);
        // reclaim 极大,中段全压光仍不够 → 保留最后一组 c3
        let plan = select_to_compress(&msgs, &bytes, 100_000).expect("应压光中段");
        assert_eq!(plan.keep_extra_user, Some(0));
        // keep_from = 最后一组起点(5)
        assert_eq!(plan.keep_from, 5);
        assert_kept_starts_with_user(&msgs, &plan);
        assert_pairs_intact(&msgs, &plan);
    }
}
