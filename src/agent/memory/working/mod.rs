use async_openai::types::chat::{ChatCompletionRequestMessage, CompletionUsage};
use tokio::sync::oneshot;

use crate::llm::LlmClient;

mod compact;

/// 摘要正文前的标题与说明。摘要自身只存纯正文,Memory 拼 system 时套此 HEADER。
pub(super) const SUMMARY_HEADER: &str = "\
## 历史对话摘要

以下是此前对话的交接摘要,供你继续任务时参考(并非当前用户的新指令):";

/// 上下文窗口大小(硬编码保守值)。高/低水位 = 窗口 × 比例。
/// TODO: 后续可从 config 按模型读取
const CONTEXT_WINDOW: u32 = 100_000;
/// 高水位:上轮真实 prompt_tokens 超过窗口该比例即触发压缩
const HIGH_WATER_RATIO: f32 = 0.70;
/// 低水位:压缩目标,腾到回落此比例以下(留缓冲,避免压完立刻又触发)
const LOW_WATER_RATIO: f32 = 0.50;

/// 进行中的压缩任务句柄:后台摘要完成后经 oneshot 回传结果。
/// 保留策略基于启动时快照下标,agent 循环期间只在尾部 push,索引不漂移。
struct PendingCompaction {
    rx: oneshot::Receiver<anyhow::Result<String>>,
    /// 保留 messages[keep_from..]
    keep_from: usize,
    /// 额外保留的单条 user 下标(第2段保最后一个 user;None=第1段连续保留)
    keep_extra_user: Option<usize>,
}

/// 工作记忆层:当前会话的对话历史 + 滚动压缩。
///
/// 不含 system(system 归核心层 Core,view 时由 Memory 拼最前)。
/// 压缩成功即把被摘要覆盖的旧消息删掉,只留摘要 + 近期原始消息。
///
/// 异步压缩:on_response 拿到 usage 判定超阈值时 spawn 后台摘要;
/// 此时 agent 正好执行工具,压缩并行不占感知时间;
/// 下一轮 view 若压缩未完成则阻塞等待,保证视图一致。
pub struct Working {
    messages: Vec<ChatCompletionRequestMessage>,
    /// 与 messages 同长同序的字节缓存(push 时预算),压缩切点选择读此零分配。
    msg_bytes: Vec<usize>,
    summary: Option<String>,
    last_usage: Option<CompletionUsage>,
    pending: Option<PendingCompaction>,
}

impl Working {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            msg_bytes: Vec::new(),
            summary: None,
            last_usage: None,
            pending: None,
        }
    }

    /// 追加一条对话消息(user / assistant / tool 结果)
    pub fn push(&mut self, msg: ChatCompletionRequestMessage) {
        let bytes = compact::estimate_bytes(&msg);
        self.messages.push(msg);
        self.msg_bytes.push(bytes);
    }

    /// 通报 token 开销。存 usage;超阈值且无压缩在跑时启动后台压缩(client 供摘要调用)。
    pub fn on_response(&mut self, usage: Option<CompletionUsage>, client: &LlmClient) {
        self.last_usage = usage;
        if self.pending.is_some() {
            return;
        }
        if !should_compact(self.last_usage.as_ref()) {
            return;
        }
        self.begin_compaction(client);
    }

    /// 产出工作层视图:`(摘要文本, 近期原始消息)`。
    /// 有进行中压缩则阻塞等待并应用结果;失败降级(warn+硬截断)。
    pub async fn view(&mut self) -> (Option<String>, Vec<ChatCompletionRequestMessage>) {
        if let Some(pending) = self.pending.take() {
            match pending.rx.await {
                Ok(Ok(new_summary)) => {
                    self.summary = Some(new_summary);
                    apply_keep(
                        &mut self.messages,
                        &mut self.msg_bytes,
                        pending.keep_from,
                        pending.keep_extra_user,
                    );
                }
                Ok(Err(e)) => {
                    tracing::warn!("后台压缩失败: {e:#}");
                }
                Err(_) => {
                    tracing::warn!("压缩任务被丢弃");
                }
            }
        }

        (self.summary.clone(), self.messages.clone())
    }

    /// 启动后台压缩任务:算范围 → 快照 → spawn 折叠摘要 → 存 pending。
    fn begin_compaction(&mut self, client: &LlmClient) {
        let total_bytes: usize = self.msg_bytes.iter().sum();
        let reclaim = bytes_to_reclaim(self.last_usage.as_ref(), total_bytes);
        let plan = match compact::select_to_compress(&self.messages, &self.msg_bytes, reclaim) {
            Some(p) => p,
            None => return,
        };

        let prev = self.summary.clone();
        let (tx, rx) = oneshot::channel();
        let client = client.clone();
        tokio::spawn(async move {
            let res = compact::summarize(&client, prev, plan.chunk).await;
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
    messages: &mut Vec<ChatCompletionRequestMessage>,
    msg_bytes: &mut Vec<usize>,
    keep_from: usize,
    keep_extra_user: Option<usize>,
) {
    let mut kept_msgs: Vec<ChatCompletionRequestMessage> = Vec::new();
    let mut kept_bytes: Vec<usize> = Vec::new();
    // uk 落在被压区间 [0..keep_from) 时,捞回它(第2段场景)
    if let Some(idx) = keep_extra_user {
        if idx < keep_from && idx < messages.len() {
            kept_msgs.push(messages[idx].clone());
            kept_bytes.push(msg_bytes[idx]);
        }
    }
    // 保留区 [keep_from..]
    if keep_from < messages.len() {
        kept_msgs.extend(messages[keep_from..].iter().cloned());
        kept_bytes.extend(msg_bytes[keep_from..].iter().copied());
    }
    debug_assert_eq!(kept_msgs.len(), kept_bytes.len(), "重建后 messages 与 msg_bytes 须同长");
    *messages = kept_msgs;
    *msg_bytes = kept_bytes;
}

/// 按真实 token 超出低水位的量,换算成需腾出的字节量。
///
/// 比值自校准:`bytes_per_token = 总字节 / prompt_tokens`,按本轮真实构成动态求,
/// 中文多则比值自然偏小、英文多则偏大,无需为语言硬编码常数。
/// 未超或无 usage 返回 0(此时不该进来,should_compact 已把关)。
fn bytes_to_reclaim(usage: Option<&CompletionUsage>, total_bytes: usize) -> usize {
    let target = (CONTEXT_WINDOW as f32 * LOW_WATER_RATIO) as u32;
    match usage {
        Some(u) if u.prompt_tokens > target => {
            let over = (u.prompt_tokens - target) as f64;
            let bytes_per_token = (total_bytes as f64 / u.prompt_tokens.max(1) as f64).max(1.0);
            (over * bytes_per_token) as usize
        }
        _ => 0,
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
