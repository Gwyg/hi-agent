//! 上下文压缩内核(内部模块,仅供 context 使用):
//! 真实 token 记账 + 历史渲染 + 递归折叠摘要,以及滚动缓冲的压缩判定与后台落地。
//!
//! 对外不暴露:`Context` 只通过 `add` / `record_usage` / `messages` 使用本模块。
//! 每条消息的 token 数在 push 时先用估算占位,下一次主对话请求的 `input_tokens`
//! 差值回填成真值;压缩水位直接看最近一次请求的真实输入,不再本地估算窗口。

use tokio::sync::oneshot;
use tracing::warn;

use crate::ai::llm::chat;
use crate::ai::llm::types::{Content, Msg, Usage};
use crate::ai::llm::MODEL_CONTEXT_WINDOW;

/// 高/低水位 = 官方真实窗口 × 比例。`input_tokens` 已含 system/tools/摘要,
/// 与水位同口径,不需要再单独标定基线。
const HIGH_WATER_RATIO: f32 = 0.70;
const LOW_WATER_RATIO: f32 = 0.50;
const HIGH_WATER_TOKENS: u32 = (MODEL_CONTEXT_WINDOW as f32 * HIGH_WATER_RATIO) as u32;
const LOW_WATER_TOKENS: u32 = (MODEL_CONTEXT_WINDOW as f32 * LOW_WATER_RATIO) as u32;

/// 历史渲染文本的初始容量。水位现在是 token、渲染出来是字节,不再由常量推导;
/// 取一个保守值即可,超界由 String 几何扩容兜底,不影响正确性。
const RENDER_CAP_EST: usize = 64 * 1024;

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

/// 进行中的压缩任务句柄:后台摘要完成后经 oneshot 回传结果。
/// 保留策略基于启动时快照下标,buffer 期间只在尾部 push,索引不漂移。
struct PendingCompaction {
    rx: oneshot::Receiver<anyhow::Result<String>>,
    /// 保留 messages[keep_from..]
    keep_from: usize,
    /// 额外保留的单条 user 下标(第2段保最后一个 user;None=第1段连续保留)
    keep_extra_user: Option<usize>,
}

/// 压缩式会话历史(内部):当前会话的对话历史 + 滚动压缩,是"上下文管理"的一种具体实现。
///
/// 不含 system(system 归 Context 的 system_prompt 字段,view 时由 Context 拼最前)。
/// 压缩成功即把被摘要覆盖的旧消息删掉,只留摘要 + 近期原始消息。
///
/// 异步压缩:`push` 按最近一次请求的真实输入判超阈值时 spawn 后台摘要;
/// 此时 agent 正好执行工具,压缩并行不占感知时间;
/// 取视图(`view`)时内部 `settle` 落地,保证视图一致。
///
/// 摘要直接调 `llm::chat`,不持有任何客户端对象。
pub(super) struct CompactingHistory {
    messages: Vec<Msg>,
    /// 与 messages 同长同序的 token 缓存。push 先放估算占位,
    /// 下一次主对话请求的 input 差值回填后即为真值(切点只读这个数组)。
    msg_tokens: Vec<u32>,
    summary: Option<String>,
    pending: Option<PendingCompaction>,
    /// 最近一次主对话请求的用量锚点。压缩落地 / reset 后置 None:
    /// 前缀变了,跨过去做 input 差值没有意义。
    anchor: Option<UsageAnchor>,
    /// 自上次被取走以来,累计压掉了多少条消息。
    ///
    /// 只记「条数」不记「第几条」:绝对位置由持久化层从当前水位往后数,内存里的
    /// 条数是相对量——恢复过的会话内存条数不等于库内总行数,拿它当绝对位置会把水位算低。
    compacted: usize,
}

/// 一次请求的差分锚点:`input` 用于判水位和算 Δ,`output` 用于给新增区间首条
/// assistant 精确赋值,`from` 是那次请求视图的历史条数(此后 add 的消息待校准)。
struct UsageAnchor {
    input: u32,
    output: u32,
    from: usize,
}

impl CompactingHistory {
    pub(super) fn new() -> Self {
        Self {
            messages: Vec::new(),
            msg_tokens: Vec::new(),
            summary: None,
            pending: None,
            anchor: None,
            compacted: 0,
        }
    }

    /// 追加一条对话消息(user / assistant / tool 结果)。token 先放估算占位,
    /// 等下一次请求的 usage 差值回填;内部顺带按最近一次真实输入判压缩。
    pub(super) fn push(&mut self, msg: Msg) {
        let tokens = estimate_tokens(&msg);
        self.messages.push(msg);
        self.msg_tokens.push(tokens);
        self.maybe_compact();
    }

    /// 主对话请求返回:用 `input` 差值把上次请求之后新增的那批消息校准成真实 token,
    /// 再以本次请求为新锚点。摘要请求的 usage 不要传进来(另一个请求前缀)。
    ///
    /// 调用时机是 `chat_stream` 返回后、assistant 落库前,此时 `messages.len()`
    /// 恰好是本次请求视图的历史条数。
    pub(super) fn record_usage(&mut self, usage: Usage) {
        let now_len = self.messages.len();
        if let Some(anchor) = self.anchor.take() {
            if anchor.from < now_len && now_len <= self.msg_tokens.len() {
                let delta = usage.input_tokens.saturating_sub(anchor.input);
                // 输入没涨(前缀变化 / 端点异常)时别把估算占位抹成 0,保留等下一轮真值
                if delta > 0 {
                    self.calibrate(anchor.from, now_len, delta, anchor.output);
                }
            }
        }
        self.anchor = Some(UsageAnchor {
            input: usage.input_tokens,
            output: usage.output_tokens,
            from: now_len,
        });
    }

    /// 把真实新增量 delta 铺到 `[start..end)`。首条通常是上次请求的 assistant,
    /// 用 output 精确赋值;其余按现有估算比例分摊,余数补最后一条保证总量精确。
    fn calibrate(&mut self, start: usize, end: usize, delta: u32, output: u32) {
        let (head, tail_from) = if output > 0 && output < delta {
            self.msg_tokens[start] = output;
            (output, start + 1)
        } else {
            (0, start)
        };
        let rest = delta - head;
        if tail_from >= end {
            return;
        }
        let est: u64 = self.msg_tokens[tail_from..end]
            .iter()
            .map(|&v| v as u64)
            .sum();
        if est == 0 {
            self.msg_tokens[end - 1] = rest;
            return;
        }
        let mut assigned = 0u32;
        for i in tail_from..end - 1 {
            let v = ((self.msg_tokens[i] as u64 * rest as u64) / est) as u32;
            self.msg_tokens[i] = v;
            assigned += v;
        }
        self.msg_tokens[end - 1] = rest.saturating_sub(assigned);
    }

    /// 落地进行中的压缩任务:阻塞等待摘要结果并应用(删旧消息、存摘要)。
    /// 失败降级(warn + 不压缩),留待下轮再触发。是缓冲唯一需 await 的写操作。
    /// 由 `view` 在取视图前调用,对调用方透明。
    pub(super) async fn settle(&mut self) {
        if let Some(pending) = self.pending.take() {
            match pending.rx.await {
                Ok(Ok(new_summary)) => {
                    self.summary = Some(new_summary);
                    let before = self.messages.len();
                    apply_keep(
                        &mut self.messages,
                        &mut self.msg_tokens,
                        pending.keep_from,
                        pending.keep_extra_user,
                    );
                    self.compacted += before - self.messages.len();
                    // system 加了摘要、历史删了一段,前缀已变:
                    // 差分基线作废,压缩后第一次请求直接重建锚点。
                    self.anchor = None;
                }
                Ok(Err(e)) => {
                    warn!("后台压缩失败: {e:#}");
                }
                Err(_) => {
                    warn!("压缩任务被丢弃");
                }
            }
        }
    }

    /// 当前历史视图:`(摘要文本, 近期原始消息)`,供上层 `Context::messages` 拼装。
    /// 先 `settle` 落地进行中的压缩(若有),保证返回一致的最新状态。
    pub(super) async fn view(&mut self) -> (Option<String>, Vec<Msg>) {
        self.settle().await;
        (self.summary.clone(), self.messages.clone())
    }

    /// 最新摘要。空串与 `None` 同义(从未压缩),调用方自己要写库时才需要区分。
    pub(super) fn summary(&self) -> Option<&str> {
        self.summary.as_deref()
    }

    /// 取走「累计压掉几条」并清零。持久化层据此把水位往后推同样多条。
    pub(super) fn take_compacted(&mut self) -> usize {
        std::mem::take(&mut self.compacted)
    }

    /// 清空历史(对应"新对话"):消息、token 缓存、摘要、压缩任务、用量锚点全部归零。
    pub(super) fn reset(&mut self) {
        self.messages.clear();
        self.msg_tokens.clear();
        self.summary = None;
        self.pending = None;
        self.anchor = None;
        self.compacted = 0;
    }

    /// 换上一段已有历史(打开已有会话)。
    ///
    /// `msgs` 必须是**水位之后**的那批:水位之前的内容已经进了 `summary`,
    /// 再装回来就是同一段对话被算两遍。
    ///
    /// 三处必须归零:
    /// - `compacted`:这是「自上次被取走以来压掉几条」的增量,恢复不该继承它,
    ///   否则打开会话后的第一轮收尾会把水位再往前推一次;
    /// - `anchor`:上一个会话的 input 基线,跨过去做差值没有意义;
    /// - `pending`:上一个会话还有压缩在跑时,它的结果属于那个会话。
    ///
    /// token 先全部放估算占位:一次请求之后就由 `record_usage` 校准成真值。
    pub(super) fn restore(&mut self, summary: Option<String>, msgs: Vec<Msg>) {
        self.msg_tokens = msgs.iter().map(estimate_tokens).collect();
        self.messages = msgs;
        self.summary = summary.filter(|s| !s.is_empty());
        self.pending = None;
        self.anchor = None;
        self.compacted = 0;
    }

    /// 内部:最近一次请求的真实输入超高水位且无压缩在跑时,启动后台压缩。
    fn maybe_compact(&mut self) {
        if self.pending.is_some() {
            return;
        }
        let input = match self.anchor.as_ref().map(|a| a.input) {
            Some(v) if v > HIGH_WATER_TOKENS => v,
            _ => return,
        };
        self.begin_compaction(input.saturating_sub(LOW_WATER_TOKENS));
    }

    /// 启动后台压缩任务:算范围 → 快照 → spawn 折叠摘要 → 存 pending。
    fn begin_compaction(&mut self, reclaim_tokens: u32) {
        if reclaim_tokens == 0 {
            return;
        }
        let plan = match select_to_compress(&self.messages, &self.msg_tokens, reclaim_tokens) {
            Some(p) => p,
            None => return,
        };

        let prev = self.summary.clone();
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let res = summarize(prev, plan.chunk).await;
            let _ = tx.send(res);
        });
        self.pending = Some(PendingCompaction {
            rx,
            keep_from: plan.keep_from,
            keep_extra_user: plan.keep_extra_user,
        });
    }
}

/// 应用保留策略:删 messages[0..keep_from),但 keep_extra_user 标记的那条捞回。
/// keep_from / keep_extra_user 基于启动压缩时的快照下标,push 期间只在尾部追加,索引不漂移。
fn apply_keep(
    messages: &mut Vec<Msg>,
    msg_tokens: &mut Vec<u32>,
    keep_from: usize,
    keep_extra_user: Option<usize>,
) {
    let mut kept_msgs: Vec<Msg> = Vec::new();
    let mut kept_tokens: Vec<u32> = Vec::new();
    // uk 落在被压区间 [0..keep_from) 时,捞回它(第2段场景)
    if let Some(idx) = keep_extra_user {
        if idx < keep_from && idx < messages.len() {
            kept_msgs.push(messages[idx].clone());
            kept_tokens.push(msg_tokens[idx]);
        }
    }
    // 保留区 [keep_from..]
    if keep_from < messages.len() {
        kept_msgs.extend(messages[keep_from..].iter().cloned());
        kept_tokens.extend(msg_tokens[keep_from..].iter().copied());
    }
    debug_assert_eq!(
        kept_msgs.len(),
        kept_tokens.len(),
        "重建后 messages 与 msg_tokens 须同长"
    );
    *messages = kept_msgs;
    *msg_tokens = kept_tokens;
}

// ———————————————————————— 以下为压缩纯函数(与缓冲状态解耦) ————————————————————————

/// 估算一条消息的字节数(粗略代表大小)。仅用于 push 时缓存,不计费。
/// 轻量实现:遍历字段长度累加,零分配。
/// 仍是字节非字符:中文 1 字 = 3 字节,维持"字节/token"比语义。
/// 每条 +80 固定开销覆盖结构噪音;下游(切点判断/自校准)只用相对值与求和,
/// 估算的比例偏差在运算中互相抵消,不影响行为。
pub(super) fn estimate_bytes(m: &Msg) -> usize {
    const STRUCT_OVERHEAD: usize = 80;
    match m {
        Msg::System(s) => STRUCT_OVERHEAD + s.content.len(),
        Msg::User(u) => {
            STRUCT_OVERHEAD
                + u.content
                    .iter()
                    .map(|c| match c {
                        Content::Text { text } => text.len(),
                    })
                    .sum::<usize>()
        }
        Msg::Assistant(a) => {
            let mut n = STRUCT_OVERHEAD + a.content.len();
            // 思考文本要随历史原样回传,同样占上下文,别漏算。
            n += a.thinking.as_ref().map(|t| t.text.len()).unwrap_or(0);
            for call in &a.tool_calls {
                // tool_call 自身结构噪音:id、花括号、字段名
                n += call.name.len() + call.arguments.len() + 40;
            }
            n
        }
        Msg::Tool(t) => STRUCT_OVERHEAD + t.content.len(),
    }
}

/// 占位/兜底用的 token 估算:字节估算按 4 字节/token 折算。
/// 新消息在等到下一次请求的 input 差值之前只有这个值,回填后即为真值。
fn estimate_tokens(m: &Msg) -> u32 {
    ((estimate_bytes(m) / 4) as u32).max(1)
}

fn is_user(m: &Msg) -> bool {
    matches!(m, Msg::User(_))
}

fn is_tool(m: &Msg) -> bool {
    matches!(m, Msg::Tool(_))
}

fn is_assistant(m: &Msg) -> bool {
    matches!(m, Msg::Assistant(_))
}

/// 压缩计划:被压消息 chunk + 保留策略。
///
/// 保留区 = (可选)messages[keep_extra_user] + messages[keep_from..]
/// - 第1段:keep_extra_user=None,保留区连续 [keep_from..]
/// - 第2段:keep_extra_user=Some(uk),保留 messages[uk] + messages[keep_from..]
pub(super) struct CompactionPlan {
    /// 进摘要的消息(owned,喂给摘要器)
    pub chunk: Vec<Msg>,
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
    messages: &[Msg],
    msg_tokens: &[u32],
    reclaim_tokens: u32,
) -> Option<CompactionPlan> {
    debug_assert_eq!(
        messages.len(),
        msg_tokens.len(),
        "msg_tokens 必须与 messages 同长"
    );
    let n = messages.len();
    if n < 2 || reclaim_tokens == 0 {
        return None;
    }

    // 最后一个 user 下标(当前任务指令,必保留)。无 user → 无可保,不压。
    let last_user = messages.iter().rposition(is_user)?;

    // 第1段:压历史完整轮次 [0..last_user)
    if let Some(cut) = cut_history_rounds(messages, msg_tokens, reclaim_tokens, last_user) {
        return Some(CompactionPlan {
            chunk: messages[..cut].to_vec(),
            keep_from: cut,
            keep_extra_user: None,
        });
    }

    // 第2段:压 last_user 之后的 assistant 组中段
    cut_last_user_tail(messages, msg_tokens, reclaim_tokens, last_user)
}

/// 第1段:在 [0..last_user) 内逐轮累 token,够且尾部留足 → 返回切点(user 边界)。
/// 扫到 last_user 仍不够 → 返回 None(交第2段)。
fn cut_history_rounds(
    messages: &[Msg],
    msg_tokens: &[u32],
    reclaim_tokens: u32,
    last_user: usize,
) -> Option<usize> {
    let mut reclaimed = 0u32;
    let mut i = 0usize;
    while i < last_user {
        if is_user(&messages[i]) {
            // 累够 → 切在此 user 前(它保留,messages[0]=user 合规)
            if reclaimed >= reclaim_tokens {
                return Some(i);
            }
            reclaimed += msg_tokens[i];
            i += 1;
        } else if is_assistant(&messages[i]) {
            // 整组扫描:assistant + 紧随的所有 tool 结果
            let ge = assistant_group_end(messages, i);
            reclaimed += sum_range(msg_tokens, i, ge);
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
    messages: &[Msg],
    msg_tokens: &[u32],
    reclaim_tokens: u32,
    last_user: usize,
) -> Option<CompactionPlan> {
    // uk 之后最后一条 assistant = 最后一组起点(必保留)
    let last_gs = messages.iter().rposition(is_assistant)?;
    // 最后一组须在 uk 之后,且 uk 与最后一组之间至少有一组中段可压
    if last_gs <= last_user + 1 {
        return None;
    }

    // 基础 token:uk 之前的全部历史(第2段也压这部分)
    let base_tokens: u32 = sum_range(msg_tokens, 0, last_user);

    // 逐组纳入中段,够即停
    let mut reclaimed = base_tokens;
    let mut i = last_user + 1;
    while i < last_gs {
        if is_assistant(&messages[i]) {
            let ge = assistant_group_end(messages, i);
            reclaimed += sum_range(msg_tokens, i, ge);
            if reclaimed >= reclaim_tokens {
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
fn assistant_group_end(messages: &[Msg], start: usize) -> usize {
    let mut j = start + 1;
    while j < messages.len() && is_tool(&messages[j]) {
        j += 1;
    }
    j
}

/// 计算 token 缓存区间 [start, end) 的和。纯数字累加,零分配。
fn sum_range(msg_tokens: &[u32], start: usize, end: usize) -> u32 {
    let end = end.min(msg_tokens.len());
    let start = start.min(end);
    msg_tokens[start..end].iter().sum()
}

/// 递归折叠摘要:新摘要 = LLM(旧摘要 + 渲染(chunk))。摘要不带 tools。
/// 返回纯正文;章节标题与说明已写在 system_prompt.md,由 Context 直接追加到末尾。
pub(super) async fn summarize(prev: Option<String>, chunk: Vec<Msg>) -> anyhow::Result<String> {
    let rendered = render_messages(&chunk);
    let user_content = match prev {
        Some(p) => format!("已有摘要:\n{p}\n\n以下是新增对话,请融合进摘要:\n{rendered}"),
        None => format!("请将以下对话压缩为摘要:\n{rendered}"),
    };
    let resp = chat(vec![Msg::system(SUMMARY_SYS), Msg::user(user_content)]).await?;
    // 压缩只要正文,不看 finish_reason。空正文照旧报错,让 settle 降级不压。
    match resp.message {
        Msg::Assistant(a) if !a.content.trim().is_empty() => Ok(a.content),
        _ => Err(anyhow::anyhow!("摘要返回为空")),
    }
}

/// 把历史渲染成可读文本,供摘要器阅读。
/// 初始容量取 RENDER_CAP_EST(窗口常量推导的安全上界),免去逐条估算的开销。
/// 仅在压缩触发时调一次,非热路径。
fn render_messages(msgs: &[Msg]) -> String {
    let mut out = String::with_capacity(RENDER_CAP_EST);
    for m in msgs {
        match m {
            Msg::System(_) => {}
            Msg::User(u) => {
                out.push_str("用户: ");
                for c in &u.content {
                    let Content::Text { text } = c;
                    out.push_str(text);
                }
                out.push('\n');
            }
            Msg::Assistant(a) => {
                if !a.content.is_empty() {
                    out.push_str("助手: ");
                    out.push_str(&a.content);
                    out.push('\n');
                }
                for call in &a.tool_calls {
                    out.push_str("助手调用工具 ");
                    out.push_str(&call.name);
                    out.push_str(": ");
                    out.push_str(&call.arguments);
                    out.push('\n');
                }
            }
            Msg::Tool(t) => {
                out.push_str("工具结果: ");
                out.push_str(&t.content);
                out.push('\n');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::llm::types::ToolCall;

    fn big(prefix: &str) -> String {
        format!("{prefix}-{}", "x".repeat(1000))
    }

    /// 构造带 N 条 tool_call 的 assistant(一次调用多个工具的规范形态)
    fn assistant_tc(ids: &[&str]) -> Msg {
        let calls = ids
            .iter()
            .map(|id| ToolCall::new(*id, "dummy", "{}"))
            .collect();
        Msg::assistant_with("", calls, None, None)
    }

    /// 给 messages 同步建 token 缓存(模拟 push 的占位估算)
    fn with_tokens(msgs: Vec<Msg>) -> (Vec<Msg>, Vec<u32>) {
        let tokens: Vec<u32> = msgs.iter().map(estimate_tokens).collect();
        (msgs, tokens)
    }

    // 多 user 样本:[U1, A+T*, U2, A+T*, U3, A+T*]
    fn sample() -> Vec<Msg> {
        vec![
            Msg::user(big("task1")),
            assistant_tc(&["c1", "c2"]),
            Msg::tool("c1", big("r1")),
            Msg::tool("c2", big("r2")),
            Msg::user(big("task2")),
            assistant_tc(&["c3", "c4"]),
            Msg::tool("c3", big("r3")),
            Msg::tool("c4", big("r4")),
            Msg::user(big("task3")),
            assistant_tc(&["c5", "c6"]),
            Msg::tool("c5", big("r5")),
            Msg::tool("c6", big("r6")),
        ]
    }

    /// 校验保留区以 user 开头(满足国内外协议)
    fn assert_kept_starts_with_user(msgs: &[Msg], plan: &CompactionPlan) {
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
    fn assert_pairs_intact(msgs: &[Msg], plan: &CompactionPlan) {
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
                        "切点劈开了 assistant 组 [{}, {})",
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
        let (msgs, tokens) = with_tokens(sample());
        let plan = select_to_compress(&msgs, &tokens, 375).expect("应可压");
        assert!(plan.keep_from < msgs.len(), "尾部近期原文应保留");
        assert_kept_starts_with_user(&msgs, &plan);
        assert_pairs_intact(&msgs, &plan);
    }

    #[test]
    fn too_small_returns_none() {
        let (msgs, tokens) = with_tokens(vec![Msg::user("hi"), Msg::assistant("ok")]);
        assert!(select_to_compress(&msgs, &tokens, 1000).is_none());
    }

    /// 单 user 长任务:压 last_user 之后的 assistant 组中段,保 last_user + 最后一组
    #[test]
    fn single_user_compress_tail_mid() {
        let (msgs, tokens) = with_tokens(vec![
            Msg::user(big("task")),
            assistant_tc(&["c1", "c2"]),
            Msg::tool("c1", big("r1")),
            Msg::tool("c2", big("r2")),
            assistant_tc(&["c3"]),
            Msg::tool("c3", big("r3")),
            assistant_tc(&["c4"]),
            Msg::tool("c4", big("r4")),
        ]);
        let plan = select_to_compress(&msgs, &tokens, 375).expect("单 user 多组应能压中段");
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
        let (msgs, tokens) = with_tokens(vec![
            Msg::user(big("task")),
            assistant_tc(&["c1"]),
            Msg::tool("c1", big("r1")),
        ]);
        assert!(select_to_compress(&msgs, &tokens, 375).is_none());
    }

    /// 同一 assistant 的多条 T 不被中间切开
    #[test]
    fn multi_tool_result_not_split() {
        let (msgs, tokens) = with_tokens(vec![
            Msg::user(big("task1")),
            assistant_tc(&["c1", "c2"]),
            Msg::tool("c1", big("r1")),
            Msg::tool("c2", big("r2")),
            Msg::user(big("task2")),
            assistant_tc(&["c3"]),
            Msg::tool("c3", big("r3")),
            Msg::user(big("task3")),
            assistant_tc(&["c4"]),
            Msg::tool("c4", big("r4")),
        ]);
        let plan = select_to_compress(&msgs, &tokens, 625).expect("应可压");
        assert_pairs_intact(&msgs, &plan);
        assert_kept_starts_with_user(&msgs, &plan);
    }

    /// 尾部巨型 Tool,切点不劈开配对
    #[test]
    fn big_tool_in_tail_pairs_intact() {
        let (msgs, tokens) = with_tokens(vec![
            Msg::user(big("task1")),
            assistant_tc(&["c1"]),
            Msg::tool("c1", big("r1")),
            Msg::user(big("task2")),
            assistant_tc(&["c2"]),
            Msg::tool("c2", big("r2_huge")),
        ]);
        if let Some(plan) = select_to_compress(&msgs, &tokens, 375) {
            assert_pairs_intact(&msgs, &plan);
            assert_kept_starts_with_user(&msgs, &plan);
        }
    }

    /// 多轮纯对话:切点落在某 user 边界
    #[test]
    fn multi_turn_plain_chat() {
        let (msgs, tokens) = with_tokens(vec![
            Msg::user(big("q1")),
            Msg::assistant(big("a1")),
            Msg::user(big("q2")),
            Msg::assistant(big("a2")),
            Msg::user(big("q3")),
            Msg::assistant(big("a3")),
        ]);
        let plan = select_to_compress(&msgs, &tokens, 375).expect("多轮纯对话应可压");
        assert!(plan.keep_from < msgs.len());
        assert_kept_starts_with_user(&msgs, &plan);
        assert_pairs_intact(&msgs, &plan);
    }

    /// 第2段保底:中段全压光仍不够,保留最后一组
    #[test]
    fn tail_compress_all_mid_keeps_last_group() {
        let (msgs, tokens) = with_tokens(vec![
            Msg::user(big("task")),
            assistant_tc(&["c1"]),
            Msg::tool("c1", "r1"),
            assistant_tc(&["c2"]),
            Msg::tool("c2", "r2"),
            assistant_tc(&["c3"]),
            Msg::tool("c3", "r3"),
        ]);
        // reclaim 极大,中段全压光仍不够 → 保留最后一组 c3
        let plan = select_to_compress(&msgs, &tokens, 100_000).expect("应压光中段");
        assert_eq!(plan.keep_extra_user, Some(0));
        // keep_from = 最后一组起点(5)
        assert_eq!(plan.keep_from, 5);
        assert_kept_starts_with_user(&msgs, &plan);
        assert_pairs_intact(&msgs, &plan);
    }

    /// 真值回填:两次请求的 input 差值 = 上次请求后新增消息的真实 token 合计
    #[test]
    fn usage_delta_calibrates_added_messages() {
        let mut h = CompactingHistory::new();
        h.push(Msg::user("hello"));
        h.record_usage(Usage {
            input_tokens: 130,
            output_tokens: 20,
            ..Default::default()
        });
        h.push(Msg::assistant("call"));
        h.push(Msg::tool("c1", "result one"));
        h.push(Msg::tool("c2", "result two"));
        h.record_usage(Usage {
            input_tokens: 230,
            output_tokens: 15,
            ..Default::default()
        });
        assert_eq!(h.msg_tokens[1], 20, "assistant 用上次 output 精确赋值");
        assert_eq!(
            h.msg_tokens[1..4].iter().sum::<u32>(),
            100,
            "区间总量应等于 input 差值"
        );
        assert_eq!(
            h.msg_tokens[2..4].iter().sum::<u32>(),
            80,
            "其余按估算比例分摊"
        );
    }

    /// 压缩落地后 system 与历史都变了,差分锚点必须作废
    #[test]
    fn settled_compaction_invalidates_anchor() {
        use futures::executor::block_on;
        let mut h = CompactingHistory::new();
        h.push(Msg::user("task"));
        h.push(Msg::assistant("a1"));
        h.record_usage(Usage {
            input_tokens: 130,
            ..Default::default()
        });
        let (tx, rx) = oneshot::channel();
        tx.send(Ok("摘要".to_string())).unwrap();
        h.pending = Some(PendingCompaction {
            rx,
            keep_from: 1,
            keep_extra_user: None,
        });
        block_on(h.settle());
        assert!(h.summary.is_some());
        assert!(h.anchor.is_none(), "压缩后不能拿旧 input 做差");
        assert_eq!(h.messages.len(), 1);
        assert_eq!(h.msg_tokens.len(), 1);
    }

    /// 新对话清空用量锚点
    #[test]
    fn reset_clears_usage_anchor() {
        let mut h = CompactingHistory::new();
        h.push(Msg::user("hello"));
        h.record_usage(Usage {
            input_tokens: 130,
            ..Default::default()
        });
        assert!(h.anchor.is_some());
        h.reset();
        assert!(h.anchor.is_none());
        assert!(h.msg_tokens.is_empty());
    }
}
