use async_openai::types::chat::{ChatCompletionRequestMessage, CompletionUsage};
use tokio::sync::oneshot;

use crate::llm::LlmClient;

mod compact;

/// 上下文窗口大小(硬编码保守值)。高/低水位 = 窗口 × 比例。
/// TODO: 后续可从 config 按模型读取
const CONTEXT_WINDOW: u32 = 100_000;
/// 高水位:上轮真实 prompt_tokens 超过窗口该比例即触发压缩
const HIGH_WATER_RATIO: f32 = 0.70;
/// 低水位:压缩目标,腾到回落此比例以下(留缓冲,避免压完立刻又触发)
const LOW_WATER_RATIO: f32 = 0.50;
/// 近期原文至少保留的字节量(不被压;含刚回灌的 tool_calls,防跨执行孤儿配对)
const MIN_KEEP_TAIL_BYTES: usize = 20_000;

/// 进行中的压缩任务句柄:后台摘要完成后经 oneshot 回传结果。
/// drain_len = 被压缩的头部消息条数(apply 时 drain 0..drain_len,保留剩余)。
struct PendingCompaction {
    rx: oneshot::Receiver<anyhow::Result<String>>,
    drain_len: usize,
}

/// 工作记忆层:当前会话的对话历史 + 滚动压缩。
///
/// 不含 system(system 归核心层 Core,view 时由 Memory 拼最前)。
/// 本期不接持久化:压缩成功即把被摘要覆盖的旧消息从 Vec 前端 drain 掉,
/// 只留摘要 + 近期原始消息(全量历史留待接入持久化后再说)。
///
/// 异步压缩:on_response 拿到 usage 判定超阈值时 spawn 后台摘要;
/// 此时 agent 正好执行工具,压缩并行不占感知时间;
/// 下一轮 view 若压缩未完成则阻塞等待,保证视图一致。
///
/// view 输出 = summary(若有) + messages(剩余原始)
/// (system 由 Memory 在外层拼接,不在此出现)
pub struct Working {
    /// 会话对话历史(不含 system;user/assistant/tool)。压缩成功后前端旧消息被 drain
    messages: Vec<ChatCompletionRequestMessage>,
    /// 压缩产物:被覆盖的旧消息合成的摘要文本
    summary: Option<String>,
    /// 上轮 LLM 调用返回的真实 token 用量,压缩判定依据
    last_usage: Option<CompletionUsage>,
    /// 进行中的后台压缩任务(None=无);防重入
    pending: Option<PendingCompaction>,
}

impl Working {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            summary: None,
            last_usage: None,
            pending: None,
        }
    }

    /// 追加一条对话消息(user / assistant / tool 结果)
    pub fn push(&mut self, msg: ChatCompletionRequestMessage) {
        self.messages.push(msg);
    }

    /// 通报 token 开销。存 usage;超阈值且无压缩在跑时启动后台压缩(client 供摘要调用)。
    pub fn on_response(&mut self, usage: Option<CompletionUsage>, client: &LlmClient) {
        self.last_usage = usage;
        // 防重入:已有压缩在跑
        if self.pending.is_some() {
            return;
        }
        if !should_compact(self.last_usage.as_ref()) {
            return;
        }
        // 超阈值,启动异步压缩(第二步实现)
        self.start_compaction(client);
    }

    /// 产出工作层视图:`(摘要文本, 近期原始消息)`。
    /// summary 交由 Memory 递给 Core 合并进单条 system(不在此预包成 Message)。
    /// 有进行中压缩则阻塞等待并应用结果;失败降级(warn+硬截断)。
    pub async fn view(&mut self) -> (Option<String>, Vec<ChatCompletionRequestMessage>) {
        // 若有进行中的压缩,阻塞等待其完成(保证视图一致)
        let mut compacted = false;
        if let Some(pending) = self.pending.take() {
            match pending.rx.await {
                Ok(Ok(new_summary)) => {
                    self.summary = Some(new_summary);
                    // 压缩成功:drain 掉被摘要覆盖的头部区间。
                    // 期间只在尾部追加过消息,前端索引不漂移,drain 0..drain_len 仍指向那批旧消息。
                    let len = self.messages.len();
                    let end = pending.drain_len.min(len);
                    if end > 0 {
                        self.messages.drain(0..end);
                    }
                    compacted = true;
                }
                Ok(Err(e)) => {
                    tracing::warn!("后台压缩失败,降级硬截断: {e:#}");
                }
                Err(_) => {
                    tracing::warn!("压缩任务被丢弃,降级硬截断");
                }
            }
        }

        // 剩余原始消息(已 drain 掉被压缩部分)
        let mut messages = self.messages.clone();
        // 硬截断兜底:仅在压缩未成功时启用。
        // 压缩目标已是低水位,再截会误删保留区前段(两套机制互相拆台)。
        if !compacted {
            hard_truncate(&mut messages);
        }

        (self.summary.clone(), messages)
    }

    /// 启动后台压缩任务:算范围 → 快照 → spawn 折叠摘要 → 存 pending。
    /// 快照全为 owned,spawn 的 future 满足 'static,不共享可变状态。
    fn start_compaction(&mut self, client: &LlmClient) {
        // 按真实 token 超量 × 自校准比值,换算需腾出的字节量(腾到低水位以下)
        let total_bytes: usize = self.messages.iter().map(compact::est_bytes).sum();
        let reclaim = reclaim_bytes(self.last_usage.as_ref(), total_bytes);
        // 直接拿到被压缩的 chunk 数组,messages 此刻不动,等 apply 时再 drain
        let chunk = match compact::compact_range(&self.messages, reclaim, MIN_KEEP_TAIL_BYTES) {
            Some(c) => c,
            None => return,
        };
        let drain_len = chunk.len();

        // 快照:旧摘要 + 待压 chunk(均 owned,喂给后台任务)
        let prev = self.summary.clone();
        let (tx, rx) = oneshot::channel();
        let client = client.clone();
        tokio::spawn(async move {
            let res = compact::summarize(&client, prev, chunk).await;
            let _ = tx.send(res);
        });
        self.pending = Some(PendingCompaction { rx, drain_len });
    }
}

/// 按真实 token 超出低水位的量,换算成需腾出的字节量。
///
/// 比值自校准:`bytes_per_token = 总字节 / prompt_tokens`,按本轮真实构成动态求,
/// 中文多则比值自然偏小、英文多则偏大,无需为语言硬编码常数。
/// 未超或无 usage 返回 0(此时不该进来,should_compact 已把关)。
fn reclaim_bytes(usage: Option<&CompletionUsage>, total_bytes: usize) -> usize {
    let target = (CONTEXT_WINDOW as f32 * LOW_WATER_RATIO) as u32;
    match usage {
        Some(u) if u.prompt_tokens > target => {
            let over = (u.prompt_tokens - target) as f64;
            // 本轮真实字节/token 比(至少 1,防除零/极端小值)
            let bytes_per_token = (total_bytes as f64 / u.prompt_tokens.max(1) as f64).max(1.0);
            (over * bytes_per_token) as usize
        }
        _ => 0,
    }
}

/// 硬截断兜底:总字符超上限时,保锚点(首条 user)+ 留近期原文,删中间最老的噪音。
/// 压缩未触发/失败时的最后同步保险,LLM-free、确定性。summary 由 Memory 单独处理。
fn hard_truncate(msgs: &mut Vec<ChatCompletionRequestMessage>) {
    // 字节上限:按窗口 × 4 字节/token 估算(英文 ~4 bytes/token,中文更少;Debug 串略膨胀)。
    // 此为最后兜底,仅在压缩未成功时启用,取宽裕上界避免误伤 50-70k token 的正常区间。
    const MAX_BYTES: usize = (CONTEXT_WINDOW as usize) * 4;
    let total: usize = msgs.iter().map(compact::est_bytes).sum();
    if total <= MAX_BYTES {
        return;
    }
    // 锚点:首条 user(任务指令),永不丢
    let anchor = msgs
        .iter()
        .position(|m| matches!(m, ChatCompletionRequestMessage::User(_)));
    let anchor_cost = anchor.map(|i| compact::est_bytes(&msgs[i])).unwrap_or(0);
    let budget = MAX_BYTES.saturating_sub(anchor_cost);

    // 从尾部往前累计,定出近期窗口起点
    let mut acc = 0;
    let mut cut = msgs.len();
    for (i, m) in msgs.iter().enumerate().rev() {
        acc += compact::est_bytes(m);
        if acc > budget {
            cut = i + 1;
            break;
        }
    }
    // 吸附安全切点:跳过落单的 tool 结果,避免保留窗口开头出现孤儿配对
    let mut start = cut;
    while start < msgs.len() && matches!(msgs[start], ChatCompletionRequestMessage::Tool(_)) {
        start += 1;
    }
    // 保锚点:锚点落在待删区间则先取出,删中间后回插到最前
    match anchor {
        Some(a) if a < start => {
            let anchor_msg = msgs[a].clone();
            msgs.drain(0..start);
            msgs.insert(0, anchor_msg);
        }
        _ => {
            if start > 0 {
                msgs.drain(0..start);
            }
        }
    }
}

/// 判定是否需要压缩:上轮真实 prompt_tokens 超过高水位。无 usage 返回 false。
fn should_compact(usage: Option<&CompletionUsage>) -> bool {
    let threshold = (CONTEXT_WINDOW as f32 * HIGH_WATER_RATIO) as u32;
    match usage {
        Some(u) => u.prompt_tokens > threshold,
        None => false,
    }
}
