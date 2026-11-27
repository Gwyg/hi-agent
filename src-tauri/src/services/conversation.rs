//! 对话用例:把"跑一轮对话"编排成完整流程。
//!
//! 这是命令层通往 agent 域的唯一入口:装配 `Toolbox`、初始化会话上下文
//! 单例、持锁、把用户输入落入上下文、跑 agent 循环、收尾写库。
//! 命令层(`commands/chat.rs`)只负责把 invoke 参数喂进来、把 `AgentEvent`
//! 翻成 IPC 事件,不碰任何装配细节。
//!
//! # 会话是懒创建的
//!
//! 点"新对话"不会建行。会话行在**首条消息落库的同一个事务里**建 —— 空会话因此
//! 不存在,也不会占「每工作区保留几条」的名额。标题取首条输入的截断(见 [`title_of`])。
//!
//! # 消息先落库、再进内存
//!
//! 顺序由 `Context::add` 保证(它先调 [`PersistFn`] 再入缓冲)。恢复上下文靠
//! 「库内条数」把压缩水位往后数,内存比库多一条水位就会算偏,所以落库失败一律
//! 中止本轮,不接受"内存有、库里没有"。
//!
//! LLM 调用不需要装配客户端对象:`llm::chat` / `llm::chat_stream` 是无状态函数,
//! 每次调用自己读全局配置。

use crate::ai::agent::context::{context_instance, init_context, Context};
use crate::ai::agent::{run_agent_turn, AgentEvent, AskRequest, Decision};
use crate::ai::llm::tools::Toolbox;
use crate::ai::llm::types::{Content, Msg};
use crate::services::workspace;
use crate::store::{message, session, Store};
use anyhow::Context as _;
use futures::future::BoxFuture;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// 首条输入只能给个短标题,太长在侧边栏里显示不下(超出的部分由 CSS 省略号处理)。
const TITLE_MAX_CHARS: usize = 10;

/// 用户只发了空白时用的兜底标题。
const FALLBACK_TITLE: &str = "新对话";

/// 每个会话在水位之前保留多少条可回看消息（水位之上的部分不受此限制）。
///
/// 为什么是 2000 而不是按「总条数」定:水位之上那段是模型当前还要用的上下文，
/// 随压缩浮动，用固定总数表达不出「留多少给用户回看」。2000 条约 2~4MB，
/// 够往前翻上千轮；再久远的细节收益很低。
const KEEP_MESSAGES: i64 = 2_000;

/// 每个工作区保留多少个会话（工作区本身不设上限）。
const KEEP_SESSIONS: i64 = 20;

/// 一次清理的结果。
///
/// `messages` 与 `sessions` 都为 0 时不会通知前端——「什么都没删」不是用户需要知道的事。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrimNotice {
    /// 消息被清理的那个会话（前端据此决定要不要在当前视图里提示）
    pub session_id: String,
    /// 删掉的消息条数
    pub messages: usize,
    /// 删掉的会话个数（这些会话里的消息不计入 `messages`）
    pub sessions: usize,
}

/// 清理完成的通知回调。**在后台线程里调用**，实现方（命令层）自己负责把结果投给前端。
///
/// 之所以是独立参数而不是塞进 `AgentEvent`：清理是会话维护，不属于 agent 循环的进度，
/// 领域层不该认识它。
pub type TrimNotify = Arc<dyn Fn(TrimNotice) + Send + Sync + 'static>;

/// 当前会话。`None` = 还没有(本轮的首条消息会建行)。
///
/// 存 `workspace_id` 是自检:切换工作区时 [`reset`] 会清掉它,万一某条路径漏了清,
/// 也不会把消息写进属于别的目录的会话。
struct CurrentSession {
    id: String,
    workspace_id: String,
}

/// 当前会话。只在 [`run_turn`] 里读写,[`reset`] 清空。
static CURRENT_SESSION: Mutex<Option<CurrentSession>> = Mutex::new(None);

fn lock_session() -> MutexGuard<'static, Option<CurrentSession>> {
    CURRENT_SESSION
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// 会话 id 槽:落库闭包与收尾共用同一份,首条消息建行后由闭包填上。
fn lock_slot(slot: &Mutex<Option<String>>) -> MutexGuard<'_, Option<String>> {
    slot.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 当前进行中轮次的取消令牌。
///
/// 单独存放、**不挂 context 锁**:`run_turn` 整轮持有上下文锁,取消命令若去等那把锁
/// 就永远拿不到。同一时刻最多一轮在跑(上下文锁串行化),单个槽位足够。
static CURRENT_CANCEL: Mutex<Option<CancellationToken>> = Mutex::new(None);

fn lock_current() -> MutexGuard<'static, Option<CancellationToken>> {
    CURRENT_CANCEL
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// 等待确认的上限。到期按拒绝处理(fail-closed):没人看弹窗时,宁可这次不执行,
/// 也不能把高危操作放过去。
const ASK_TIMEOUT: Duration = Duration::from_secs(60);

/// 正在等的确认请求:tool_call_id → 回答通道。
///
/// 鉴权是串行的,同一时刻最多一条,但用表而不是单槽位:迟到的回答要能按 id
/// 精确丢弃(用户点了停止之后才点「允许」,不能误伤下一条)。
static PENDING: LazyLock<Mutex<HashMap<String, oneshot::Sender<Decision>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn lock_pending() -> MutexGuard<'static, HashMap<String, oneshot::Sender<Decision>>> {
    PENDING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 用户对确认弹窗的回答(对应前端 `answer_ask` 命令)。
/// 返回 false 表示这条请求已经不在等了(超时 / 已答 / 已取消)。
pub fn answer(id: &str, decision: Decision) -> bool {
    match lock_pending().remove(id) {
        Some(tx) => tx.send(decision).is_ok(),
        None => false,
    }
}

/// 待确认项的登记守卫:回答 / 超时 / 取消(含 future 被 drop)后把自己从表里摘掉,
/// 免得留下永远发不出去的通道。
struct AskGuard(String);

impl Drop for AskGuard {
    fn drop(&mut self) {
        lock_pending().remove(&self.0);
    }
}

/// 取消当前轮次。返回 false 表示当前没有在跑的轮次(无事可取消)。
pub fn cancel() -> bool {
    match lock_current().as_ref() {
        Some(token) => {
            token.cancel();
            true
        }
        None => false,
    }
}

/// 轮次注册守卫:轮次结束(正常/出错/取消)时清空槽位,避免残留令牌影响下一轮。
struct TurnGuard;

impl Drop for TurnGuard {
    fn drop(&mut self) {
        *lock_current() = None;
    }
}

/// 跑一轮对话:调用方传入本轮用户输入,历史从会话上下文单例(进程级)取。
///
/// `on_event` 由调用方提供,把领域级进度事件(流式 token / 工具事件 / 结束 / 出错)
/// 翻译成它想要的形态——命令层翻成 Tauri IPC 事件,测试可直接断言领域事件。
/// 本函数不感知传输层:无 Tauri、无序列化。
///
/// 错误(网络/序列化/循环未收敛/落库失败)经 `anyhow` 原样上抛,由调用方决定形态:
/// 命令层翻成 invoke 返回值 + 补发 `llm-error` 事件。
///
/// 取消:每轮新建独立 `CancellationToken` 并登记到 `CURRENT_CANCEL`,
/// 命令层 `cancel_chat` 通过 [`cancel`] 触发;轮次结束(含提前返回)由守卫清空登记。
///
/// `on_trim` 在**收尾之后**于后台线程被调用（只在真的删了东西时）。传 `Arc<Store>`
/// 而不是借用：清理任务比这一轮活得更久。
///
/// 返回本轮消息落进了哪个会话，前端据此刷新侧边栏列表并高亮当前项。
/// 本轮一条都没落成时返回空串（调用方不必区分，空串只表示「没有会话」）。
pub async fn run_turn(
    store: Arc<Store>,
    user_input: &str,
    on_event: impl Fn(AgentEvent) + Send + Sync,
    on_trim: TrimNotify,
) -> anyhow::Result<String> {
    let toolbox = Toolbox::new();
    // 初始化会话上下文单例(幂等,重复调用只初始化一次)。
    init_context();

    // 锁在本层(用例编排)持有:上下文单例在整轮对话期间加锁,
    // 领域循环只拿到 `&mut Context`,完全不碰锁 / Tauri / 序列化。
    // 异步 Mutex 的 guard 允许跨 await 持有,此处持有整轮是预期用法。
    let mut context = context_instance().lock().await;

    // 拿锁之后再登记:锁保证同一时刻只有一轮,登记不会被并发轮次覆盖
    let cancel = CancellationToken::new();
    *lock_current() = Some(cancel.clone());
    let _turn_guard = TurnGuard;

    // 会话必须属于当前工作区;不属于就当没有,这一轮重新建 —— 换了目录还接着
    // 往旧目录的会话里写,恢复时会出现「在 A 的对话里读到了 B 的消息」。
    let ws = workspace::current().context("工作区未初始化,无法确定会话归属")?;
    let existing = lock_session()
        .as_ref()
        .filter(|s| s.workspace_id == ws.id)
        .map(|s| s.id.clone());
    // 本轮开始前就有 id = 续用旧会话；没有 = 首条消息会建一行（会话数可能要清理）
    let resumed = existing.is_some();
    let slot: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(existing));

    let persist = persist_into(&store, ws.id.clone(), slot.clone());

    // 用户输入也走同一条落库路径:槽为空时闭包会在一个事务里建会话 + 落这条消息。
    context.add(Msg::user(user_input), &persist)?;

    // 工具确认的等待实现:登记(同步段)→ 交回循环发 Ask 事件 → 三路等。
    // 三路里取消那一路不能省:用户点停止时若继续等,整轮持锁要空转到超时才退出。
    // 自己持一份令牌(move),闭包因此是 'static,能作为 ConfirmFn 交出去。
    let ask_cancel = cancel.clone();
    let confirm = move |req: &AskRequest| -> BoxFuture<'static, Decision> {
        let (tx, rx) = oneshot::channel();
        lock_pending().insert(req.id.clone(), tx);
        let guard = AskGuard(req.id.clone());
        let cancel = ask_cancel.clone();
        Box::pin(async move {
            let decision = tokio::select! {
                biased;
                _ = cancel.cancelled() => Decision::Deny,
                // 发送端被丢弃(理论上不会,防御性兜底)同样按拒绝
                d = rx => d.unwrap_or(Decision::Deny),
                _ = tokio::time::sleep(ASK_TIMEOUT) => Decision::Deny,
            };
            drop(guard);
            decision
        })
    };

    let result = run_agent_turn(
        &toolbox,
        &mut context,
        &cancel,
        &persist,
        on_event,
        &confirm,
    )
    .await;

    // 收尾写库:成功失败都做 —— 已落库的消息是事实,时间与摘要水位该跟上。
    finish(&store, &mut context, &slot, &ws.id);

    // 清理扔到后台：它要占连接锁，不该压在「回复已产出」到「界面解除忙碌」之间。
    // 懒创建失败时槽仍然是空的 —— 那一行并不存在，也就不该触发会话数清理。
    let session_id = lock_slot(&slot).clone();
    if let Some(id) = session_id.clone() {
        // 会话已落定，更新断点：下次启动据此回到这个对话，而不是从空白开始。
        // 写失败只告警——它影响的是下次启动，不是这一轮。
        if let Err(e) = crate::config::resume::set_session(&id) {
            tracing::warn!(error = %e, session = %id, "断点写回失败(本次已生效,影响下次启动)");
        }
        spawn_trim(store.clone(), id, ws.id.clone(), !resumed, on_trim);
    }

    // 失败也把 id 带出去：消息已经落库了，界面该能看到这一条会话
    result.map(|()| session_id.unwrap_or_default())
}

/// 新对话:结束当前会话、清空内存上下文(对应前端"新对话"按钮)。
///
/// 只清内存,不删任何已落库的行 —— 那个对话还在侧边栏里,随时能点回去。
/// 用户未发过任何消息即点"新对话"时底层自动跳过(无内容可清)。
pub async fn reset() {
    // 顺序无关,但先清会话:万一 reset_context 内部 panic,至少不会把下一条消息
    // 继续写进上一个会话。
    *lock_session() = None;
    crate::ai::agent::context::reset_context().await;
}

/// 模型此刻挂在哪个会话上。`None` = 处于「新对话」且还没落库。
///
/// 只给「删会话」那条路径用：删掉的正好是当前会话时，内存上下文必须一起清，
/// 否则下一条消息会往一个已经不存在的 session 行里写。
pub fn current_id() -> Option<String> {
    lock_session().as_ref().map(|s| s.id.clone())
}

/// 把「当前会话」切到一个已存在的会话(用户点开侧边栏里的历史会话)。
///
/// 三件事必须一起做,缺一件都会让界面和后端对不上:
/// - 换掉模型上下文 → 否则界面上是 A 的记录,模型拿着 B 的记忆答非所问;
/// - 改会话指针 → 否则下一轮消息写回旧会话,新会话永远是空的;
/// - 更新断点 → 否则关掉再打开会落回上一个会话。
///
/// `summary` 与 `msgs` 由调用方从库里读好(两者配套,水位之前的已进摘要)。
/// 断点写失败只 warn:本次切换已经生效,影响的是下次启动。
pub async fn adopt(id: &str, workspace_id: &str, summary: String, msgs: Vec<Msg>) {
    init_context();
    context_instance().lock().await.restore(summary, msgs);
    *lock_session() = Some(CurrentSession {
        id: id.to_string(),
        workspace_id: workspace_id.to_string(),
    });
    if let Err(e) = crate::config::resume::set_session(id) {
        tracing::warn!(error = %e, session = %id, "断点写回失败(本次已生效,影响下次启动)");
    }
}

// ---- 内部 ----

// ---- 清理 ----

/// 正在清理的会话。避免同一会话连续两轮各踢一脚时，两个后台任务同时判断再各删一半
/// （删除本身幂等，但通知会重复）。
static TRIMMING: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

fn lock_trimming() -> MutexGuard<'static, HashSet<String>> {
    TRIMMING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 在跑标记的守卫：任务结束（含 panic 展开）时摘掉自己，否则一次失败会让这个会话
/// 永远不再被清理。
struct TrimInFlight(String);

impl Drop for TrimInFlight {
    fn drop(&mut self) {
        lock_trimming().remove(&self.0);
    }
}

/// 踢一次后台清理，**不等结果**。
///
/// 删行要占连接锁，放在收尾路径上会让「回复已经出来了、界面还停着」。清理是幂等且
/// 可重入的：进程退出时这个线程被丢掉也没关系，下次写入会再踢一脚 —— 这也是不需要
/// 清理队列表、不需要持久化任务的原因。
///
/// `trim_sessions` 只在**本轮新建了会话**时为真：会话数只会在新建时增长，
/// 别的时候去查一遍是白跑（虽然只要几微秒）。
fn spawn_trim(
    store: Arc<Store>,
    session_id: String,
    workspace_id: String,
    trim_sessions: bool,
    notify: TrimNotify,
) {
    if !lock_trimming().insert(session_id.clone()) {
        return; // 上一趟还在跑，它读到的已经是最新数据
    }
    std::thread::spawn(move || {
        let _guard = TrimInFlight(session_id.clone());
        match trim_once(&store, &session_id, &workspace_id, trim_sessions) {
            Ok(notice) if notice.messages > 0 || notice.sessions > 0 => notify(notice),
            Ok(_) => {}
            Err(e) => tracing::warn!(
                error = format!("{e:#}"),
                session = %session_id,
                "后台清理失败（下次写入会重试）"
            ),
        }
    });
}

/// 同步执行一次清理。消息与会话各一个事务：前者失败不该拖累后者。
///
/// 消息只删水位之前的：水位之上是模型当前还要用的上下文，删了恢复时切点就错位。
/// 所以没压缩过的会话（水位 0）什么都不会删，历史完整保留。
fn trim_once(
    store: &Store,
    session_id: &str,
    workspace_id: &str,
    trim_sessions: bool,
) -> anyhow::Result<TrimNotice> {
    let messages = store.tx(|tx| {
        let row = session::load(tx, session_id)?.context("会话行在清理时消失")?;
        message::trim(tx, session_id, row.summary_upto_seq, KEEP_MESSAGES)
    })?;

    let sessions = if trim_sessions {
        store.tx(|tx| {
            session::trim_by_workspace(tx, workspace_id, KEEP_SESSIONS, Some(session_id))
        })?
    } else {
        0
    };

    Ok(TrimNotice {
        session_id: session_id.to_string(),
        messages,
        sessions,
    })
}

/// 一条消息的落库闭包。
///
/// 槽为空时顺手把会话建出来(懒创建),**与首条消息同一个事务**:崩在中间不会
/// 留下一条没有任何消息的空会话。建完之后槽里就有 id 了,后续都是纯插入。
///
/// 借用 `store` 而不是克隆它:存储层是连接 + 锁的单例,克隆没有意义。
fn persist_into<'a>(
    store: &'a Store,
    workspace_id: String,
    slot: Arc<Mutex<Option<String>>>,
) -> impl Fn(&Msg) -> anyhow::Result<()> + Send + Sync + 'a {
    move |msg: &Msg| {
        if let Some(id) = lock_slot(&slot).clone() {
            store.tx(|tx| message::insert(tx, &id, msg))?;
            return Ok(());
        }

        let title = title_of(msg);
        let id = store.tx(|tx| {
            let created = session::insert(tx, &workspace_id, &title)?;
            message::insert(tx, &created.id, msg)?;
            Ok(created.id)
        })?;
        *lock_slot(&slot) = Some(id);
        Ok(())
    }
}

/// 一轮结束后的收尾:刷新会话时间,压缩过就回写摘要与水位。
///
/// 两件事都**只 warn 不报错**:消息已经落库,时间和摘要是附加信息;
/// 为它们把一轮已经跑完的对话判成失败,用户会看到「回复出来了却报错」。
///
/// `take_compacted() > 0` 时才写摘要 —— 那个计数只在压缩落地成功后累加,
/// 所以此刻 `summary()` 一定是压缩后的新文本,不会出现「新水位配旧摘要」
/// (那会让被压掉的消息永久消失)。
fn finish(store: &Store, context: &mut Context, slot: &Mutex<Option<String>>, workspace_id: &str) {
    let Some(id) = lock_slot(slot).clone() else {
        // 本轮一条消息都没落成(首条就写失败):没有会话行可更新
        return;
    };

    // 这里才把"当前会话"定下来:下一轮据此续用同一行,而不是又建一个新会话。
    // 放在收尾而不是建行处,是因为写入失败时那一行并不存在,不该被记住。
    *lock_session() = Some(CurrentSession {
        id: id.clone(),
        workspace_id: workspace_id.to_string(),
    });

    let compacted = context.take_compacted();
    let summary = context.summary().map(str::to_string);

    let written = store.tx(|tx| {
        session::touch(tx, &id)?;
        if compacted > 0 {
            if let Some(summary) = summary.as_deref() {
                let row = session::load(tx, &id)?.context("会话行在收尾时消失")?;
                match message::nth_seq_after(tx, &id, row.summary_upto_seq, compacted as i64)? {
                    Some(seq) => session::set_summary(tx, &id, summary, seq)?,
                    // 库内条数少于内存压掉的条数:理论上不会发生(内存只装库里的子集)。
                    // 真发生就停在原水位,宁可多带几条已压缩的消息,也不把水位推过头。
                    None => tracing::warn!(
                        session = %id,
                        compacted,
                        "库内消息不足以覆盖压缩条数,水位未推进"
                    ),
                }
            }
        }
        Ok(())
    });
    if let Err(e) = written {
        tracing::warn!(error = format!("{e:#}"), "会话收尾写库失败(消息已落库)");
    }
}

/// 会话标题:首条输入 trim、连续空白压成一个空格、按**字符**截断。
///
/// 不追加省略号 —— 那是展示层的事(列表靠 CSS 截),库里存纯文本,
/// 用户改名时不会碰上「把省略号删掉」这种麻烦。
fn title_of(msg: &Msg) -> String {
    let Msg::User(u) = msg else {
        return FALLBACK_TITLE.to_string();
    };
    let text: String = u
        .content
        .iter()
        .map(|c| match c {
            Content::Text { text } => text.as_str(),
        })
        .collect();
    // split_whitespace 顺手完成 trim 与「连续空白归一」,再用单空格拼回去
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let title: String = normalized.chars().take(TITLE_MAX_CHARS).collect();
    if title.is_empty() {
        FALLBACK_TITLE.to_string()
    } else {
        title
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::workspace as workspace_repo;

    /// 回答投递:一次回答只兑现一次,重复/迟到一律 false(前端据此不提示)。
    /// 用独立 id,避免与并行的其他用例抢同一条登记。
    #[test]
    fn answer_delivers_once() {
        let id = "ask-answer-delivers-once";
        let (tx, mut rx) = oneshot::channel();
        lock_pending().insert(id.to_string(), tx);

        assert!(answer(id, Decision::Allow));
        assert_eq!(rx.try_recv(), Ok(Decision::Allow));
        // 已经取走:第二次是迟到点击,不能误伤下一条
        assert!(!answer(id, Decision::Deny));
    }

    /// 没有登记时回答不 panic,返回 false。
    #[test]
    fn answer_without_pending_is_false() {
        assert!(!answer("ask-not-registered", Decision::Allow));
    }

    /// 标题:去首尾空白、连续空白压成一个空格、按字符截 10 个、不加省略号
    #[test]
    fn title_is_trimmed_normalized_and_truncated() {
        assert_eq!(title_of(&Msg::user("  改一下这个 bug  ")), "改一下这个 bug");
        assert_eq!(
            title_of(&Msg::user("第一行\n\n  第二行   第三行")),
            "第一行 第二行 第三"
        );
        assert_eq!(
            title_of(&Msg::user("这是一个非常长的需求描述超过十个字符了")),
            "这是一个非常长的需求",
            "按字符截,不是字节"
        );
        assert_eq!(
            title_of(&Msg::user("   \n\t ")),
            FALLBACK_TITLE,
            "空白输入兜底"
        );
    }

    /// 懒创建:首条消息在同一个事务里建会话 + 落消息,标题取它的截断;
    /// 第二条消息复用同一个会话,不会建第二行。
    #[test]
    fn first_message_creates_session_then_reuses_it() {
        let db = std::env::temp_dir().join(format!("hi-agent-turn-{}.db", crate::util::new_id()));
        let store = Store::open_at(&db).expect("打开临时库");
        let ws = store
            .tx(|tx| workspace_repo::insert(tx, "/proj/turn", "turn"))
            .expect("工作区");

        let slot: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let persist = persist_into(&store, ws.id.clone(), slot.clone());

        persist(&Msg::user("帮我看看这个函数")).expect("首条");
        let sid = lock_slot(&slot).clone().expect("已建会话");

        persist(&Msg::assistant("看一下")).expect("第二条");
        assert_eq!(
            lock_slot(&slot).clone().as_deref(),
            Some(sid.as_str()),
            "第二条消息不该再建会话"
        );

        let row = store
            .tx(|tx| session::load(tx, &sid))
            .expect("查询")
            .expect("会话存在");
        assert_eq!(row.title, "帮我看看这个函数");
        assert_eq!(row.workspace_id, ws.id);

        let msgs = store.tx(|tx| message::load(tx, &sid, 0)).expect("读取");
        assert_eq!(msgs.len(), 2, "两条消息都落了库");
        assert_eq!(msgs[0], Msg::user("帮我看看这个函数"));

        // 闭包借着 `store`,先放掉它再关库 —— Windows 上连接没关时文件删不掉
        drop(persist);
        drop(store);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", db.display()));
        }
    }

    /// 收尾后「当前会话」必须指向刚落定的那一行:否则下一轮会再建一个新会话,
    /// 同一次对话被切成两段,列表里出现两条。
    #[test]
    fn finish_remembers_the_session() {
        let db =
            std::env::temp_dir().join(format!("hi-agent-remember-{}.db", crate::util::new_id()));
        let store = Store::open_at(&db).expect("打开临时库");
        let ws = store
            .tx(|tx| workspace_repo::insert(tx, "/proj/remember", "remember"))
            .expect("工作区");

        let slot: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let persist = persist_into(&store, ws.id.clone(), slot.clone());
        persist(&Msg::user("第一句")).expect("首条");

        let mut context = Context::new();
        finish(&store, &mut context, &slot, &ws.id);

        let remembered = lock_session()
            .as_ref()
            .map(|s| (s.id.clone(), s.workspace_id.clone()));
        assert_eq!(
            remembered,
            lock_slot(&slot).clone().map(|id| (id, ws.id.clone())),
            "收尾后应记住这一行会话"
        );

        // 下一轮据此续用同一行,而不是新建
        let reused = lock_session()
            .as_ref()
            .filter(|s| s.workspace_id == ws.id)
            .map(|s| s.id.clone());
        assert_eq!(reused, lock_slot(&slot).clone());

        // 全局状态还给别的用例
        *lock_session() = None;
        drop(persist);
        drop(store);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", db.display()));
        }
    }
}
