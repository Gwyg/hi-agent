use async_openai::types::chat::{ChatCompletionRequestMessage, CompletionUsage};
use tokio::sync::oneshot;

use crate::llm::LlmClient;

mod compact;

/// 上下文窗口大小(硬编码保守值)。高水位 = 窗口 × 比例。
/// TODO: 后续可从 config 按模型读取
const CONTEXT_WINDOW: u32 = 100_000;
/// 高水位:上轮真实 prompt_tokens 超过窗口该比例即触发压缩
const HIGH_WATER_RATIO: f32 = 0.70;
/// 每次触发压掉最老的用户轮次数(精确计数,不估 token;不够下轮再压)
const COMPACT_TURNS: usize = 6;

/// 进行中的压缩任务句柄:后台摘要完成后经 oneshot 回传结果。
/// upto_boundary = 本次压缩覆盖到的消息下标(完成后 drain 掉 messages[0..此])。
struct PendingCompaction {
    rx: oneshot::Receiver<anyhow::Result<String>>,
    upto_boundary: usize,
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
        if let Some(pending) = self.pending.take() {
            match pending.rx.await {
                Ok(Ok(new_summary)) => {
                    self.summary = Some(new_summary);
                    // 压缩成功:丢弃被摘要覆盖的前端旧消息(本期不持久化)。
                    // 边界从前端计,期间只在尾部追加过消息,[0..n] 仍指向那批旧消息。
                    let n = pending.upto_boundary.min(self.messages.len());
                    self.messages.drain(0..n);
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
        // 硬截断兜底:即便未压缩/压缩失败,也保证不超窗(按字符粗估,最后保险)
        hard_truncate(&mut messages);

        (self.summary.clone(), messages)
    }

    /// 启动后台压缩任务:算边界 → 快照 → spawn 折叠摘要 → 存 pending。
    /// 快照全为 owned,spawn 的 future 满足 'static,不共享可变状态。
    fn start_compaction(&mut self, client: &LlmClient) {
        let boundary = match compact::compact_boundary(&self.messages, COMPACT_TURNS) {
            Some(b) => b,
            None => return,
        };

        // 快照:旧摘要 + 待压 chunk(均 owned,喂给后台任务)
        let prev = self.summary.clone();
        let chunk: Vec<ChatCompletionRequestMessage> = self.messages[..boundary].to_vec();

        let (tx, rx) = oneshot::channel();
        let client = client.clone();
        tokio::spawn(async move {
            let res = compact::summarize(&client, prev, chunk).await;
            let _ = tx.send(res);
        });
        self.pending = Some(PendingCompaction {
            rx,
            upto_boundary: boundary,
        });
    }
}

/// 硬截断兜底:总字符超上限时,从尾部保留对齐 user 边界的近期消息。
/// 压缩未触发/失败时的最后保险。summary 由 Memory 单独处理,不在此列。
fn hard_truncate(msgs: &mut Vec<ChatCompletionRequestMessage>) {
    // 字符上限(用 Debug 串估算,偏保守)。窗口 100k token,字符取 120k 留余量
    const MAX_CHARS: usize = 120_000;
    let total: usize = msgs.iter().map(|m| format!("{m:?}").len()).sum();
    if total <= MAX_CHARS {
        return;
    }
    // 从尾部往前累计,定出可保留的起点
    let mut acc = 0;
    let mut cut = msgs.len();
    for (i, m) in msgs.iter().enumerate().rev() {
        acc += format!("{m:?}").len();
        if acc > MAX_CHARS {
            cut = i + 1;
            break;
        }
    }
    // 对齐 user 边界:从 cut 往后找第一个 User,跳过落单的 tool/assistant
    // (防 tool_calls/tool_result 配对被切的残骸)
    let mut new_start = cut;
    while new_start < msgs.len()
        && !matches!(msgs[new_start], ChatCompletionRequestMessage::User(_))
    {
        new_start += 1;
    }
    if new_start > 0 {
        msgs.drain(0..new_start);
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
