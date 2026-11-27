//! agent 循环:取视图 → 流式请求 → 按 finish_reason 分流 → 鉴权(可问用户) → 并发执行 → 回灌。
//!
//! 消息统一用 `llm::types::Msg`。模型回复**整条**落进上下文(含 `thinking`),
//! 这样带 tools 的下一轮请求才能把 `reasoning_content` 原样回传——DeepSeek 缺了
//! 它会直接 400。所以 Stop / ToolCalls 两个分支都不重新构造只有正文的 assistant。
//!
//! 流式阶段 `Delta::Text` / `Delta::Thinking` 各推一种前端事件(正文 / 思考),
//! 两者是同一批增量、两个出口;思考同时仍完整累积进 `ChatResponse.message`,
//! 下一轮带 tools 的请求要靠它回传 `reasoning_content`。`refusal` 已由协议层
//! 折进正文,空正文在 Stop 分支兜一段提示,避免前端留下空气泡。
//!
//! # 待办(按优先级,均未开始)
//!
//! 1. `edit` 的红绿 diff(前端 `components/ToolCard.vue`):`old_string` / `new_string`
//!    上下块染色即可,不做行内 LCS——片段通常只有几行,行内对齐的收益远小于实现成本。
//! 2. `bash` 的命令/结果分区(同上):`content` 自带 `exit_code: N` 和 `--- stdout ---`,
//!    天然就是分界,不必再加分隔线(长文本里分隔线会被误读成输出的一部分)。
//! 3. 历史消息的**读取**与列表:消息已随每轮落库(见 `services::conversation`),
//!    但「列出会话 / 恢复某条会话」尚未接,前端 `messages` 仍只活在进程内存。

use crate::ai::llm::tools::{Action, Toolbox};
use crate::ai::llm::types::{Delta, FinishReason, Msg};
use crate::ai::llm::{chat_stream, MODEL_CONTEXT_WINDOW};
use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

pub mod context;
use context::{Context, PersistFn};

/// 一次确认请求:鉴权判定为「要问用户」时产出。
///
/// `id` 复用模型的 tool_call_id(同一调用只鉴权一次),调用方据此把用户的选择
/// 对回这次请求。领域层不认识任何传输类型。
pub struct AskRequest {
    pub id: String,
    pub name: String,
    pub args: String,
    /// 授权键:用户选「总是允许」时原样交给 `Toolbox::grant`
    pub keys: Vec<String>,
    /// 能否「总是允许」。`AskAlways` 恒为 false——它的语义是每次必问
    pub rememberable: bool,
}

/// 用户对确认请求的选择
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    AlwaysAllow,
    Deny,
}

/// 等用户回答的注入点:循环只负责按 id 等,怎么问、等多久、超时怎么办由调用方决定。
///
/// 返回 `'static` future,让实现方得以把等待状态(通道、计时器、取消令牌)搬进去。
/// 调用序是「先调本函数完成登记,再发 Ask 事件,最后 await 返回值」——顺序颠倒
/// 会丢掉一个极快到达的回答。
pub type ConfirmFn = dyn Fn(&AskRequest) -> BoxFuture<'static, Decision> + Sync;

/// 一次工具调用最终落到了哪一步。三态是真实存在的物理区分,不是样式分类:
/// 跑成了 / 跑了但报错 / 压根没跑(被拒、被取消、工具不存在)。
#[derive(Clone, Copy)]
pub enum ToolStatus {
    Ok,
    Error,
    Skipped,
}

/// 循环向外部(UI/前端)推进的进度事件。
///
/// 刻意**不含任何 Tauri / 序列化类型**:循环只产生领域级进度,
/// "怎么转成 IPC 事件 / 用什么 DTO 结构"由调用方(命令层)决定。
/// 这样 `run_agent_turn` 不依赖任何传输层,纯粹是领域编排。
pub enum AgentEvent {
    /// 正文增量:模型最终写给用户的答案(或工具轮的旁白)
    Token(String),
    /// 思考增量:内部推理,与正文同一批流、不同展示层级。
    /// 只供 UI 展示,不影响上下文;落库与 `reasoning_content` 回传仍走整条 message。
    Thinking(String),
    ToolStart {
        id: String,
        name: String,
        args: String,
    },
    ToolResult {
        id: String,
        content: String,
        /// 结局,仅供 UI 决定怎么展示(标红 / 自动展开 / 灰显)。
        /// 不参与上下文,也不参与安全判定——工具本身就只产出文本结果
        status: ToolStatus,
    },
    /// 请求用户确认。发出后循环会阻塞在 [`ConfirmFn`] 上,直到拿到 [`Decision`]
    Ask(AskRequest),
    /// 该确认已作废(已回答 / 超时 / 取消),UI 据此关闭弹窗
    AskClosed {
        id: String,
    },
    /// 本轮请求的用量快照:真值来自端点 `usage`。`used` 是 input_tokens,
    /// `total` 是模型窗口,`cached` 是其中命中提示词缓存的部分;只交原始数字,
    /// 百分比由 UI 算——循环不认识展示口径。端点整份没给 usage 时不发这个事件。
    ContextUsage {
        used: u32,
        total: u32,
        cached: u32,
    },
    /// 用户取消:本轮已收尾(上下文保持合法),不是错误
    Cancelled,
    Done,
    Error(String),
}

/// 循环轮次上限:防止模型反复调工具陷入死循环
const MAX_TURNS: usize = 80;

/// 单轮内同时在飞的工具数上限。
/// 主要约束不是内存,而是同时打开的 shell 进程数与并发文件 IO 的排队深度;
/// 4 足以让常见的"读几个文件"重叠,又不至于一轮起十几个子进程。
const MAX_CONCURRENT_TOOLS: usize = 4;

/// 取消时给每个 tool_call_id 补的占位结果。
/// 这不是 UI 文案,是协议要求:assistant.tool_calls 里的每个 id 都必须有对应 tool 消息。
const CANCELLED_TOOL_RESULT: &str = "已取消(用户中断,工具未完成)";

/// 一次工具调用的执行参数:串行决策阶段产出,交给并发执行阶段。
struct Call {
    /// 在模型返回的 tool_calls 中的原始下标,用于执行完成后恢复顺序
    idx: usize,
    id: String,
    name: String,
    args: String,
}

/// 基础 agent 循环:在 `context` 上反复调用 LLM,
/// 遇到 ToolCalls 就经 Toolbox 执行工具并把结果回灌,直到模型自然结束(Stop)或出错。
///
/// 上下文管理交由 `Context`(常驻系统提示 + 内部滚动压缩模块):超阈值时后台异步压缩旧消息,
/// 长对话不会撑爆上下文窗口;压缩细节对循环完全透明。
///
/// 工具确认(AskOnce / AskAlways)经 `confirm` 交给调用方问用户;`Allow` / `Deny`
/// 由循环内部直接处理,不打扰用户。确认在串行判定阶段完成,故同一时刻最多一个弹窗。
///
/// 同一轮的多个工具调用在 assess 通过后**并发执行**(有界并发),结果按模型声明的
/// 原始顺序回灌上下文——顺序稳定是协议要求:tool 消息与 tool_calls 必须一一对应。
///
/// `cancel` 由调用方(用例层)每轮新建:取消发生在模型输出阶段时直接丢弃流并结束;
/// 发生在工具阶段时,**为每个 tool_call_id 补齐"已取消"结果后再结束**——否则上下文里
/// 会留下"有 tool_calls 却没有对应 tool 消息"的非法序列,下一次请求必被接口拒绝。
/// 工具本身不接收令牌:取消通过「丢弃工具 future」表达,工具内部若有比 future 更长寿的
/// 工作,由它自己的 Drop 守卫通知(见 `Tool::execute` 文档)。
///
/// `persist` 是每条消息的落地钩子(通常写库),由 `Context::add` 在入内存前调用;
/// 它返回错误时本轮立即中止——内存与库必须一致,否则压缩水位下次会算偏。
///
/// 进度通过 `on_event` 回调向外推进(流式 token / 工具事件 / 结束 / 出错),
/// 循环本身不碰任何传输层(Tauri / 序列化 / 锁)。`context` 由调用方持有并传入 `&mut`。
pub async fn run_agent_turn(
    toolbox: &Toolbox,
    context: &mut Context,
    cancel: &CancellationToken,
    persist: &PersistFn<'_>,
    on_event: impl Fn(AgentEvent) + Send + Sync,
    confirm: &ConfirmFn,
) -> anyhow::Result<()> {
    for turn in 0..MAX_TURNS {
        // 取本轮发给 LLM 的视图:messages 内部先 settle 落地上轮压缩,再取 system(含摘要) + 近期消息。
        // context 已是 &mut,无锁、无传输层;纯领域操作。
        let view = context.messages().await;
        let msg_count = view.len();
        tracing::info!(turn, msg_count, "agent 轮次开始");
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                // 流式中途取消:assistant 消息尚未落库,上下文本身仍合法,直接结束本轮
                tracing::info!(turn, "模型输出阶段被取消");
                on_event(AgentEvent::Cancelled);
                return Ok(());
            }
            r = chat_stream(view, toolbox.definitions(), |d| match d {
                Delta::Text(t) => on_event(AgentEvent::Token(t.clone())),
                Delta::Thinking(t) => on_event(AgentEvent::Thinking(t.clone())),
                // 其余增量(工具片段 / 结束原因 / 用量 / signature)只在累加器里生效,
                // 不直接对应一个 UI 动作
                _ => {}
            }) => r?,
        };

        // 用量真值在流的最后一帧才到,描述的是「刚发出去的这次请求」。拿到就推给
        // UI,并回写上下文:压缩内核用 input 差值校准新增消息的真实 token、
        // 用 input 判水位。工具轮也会更新,压缩后可能回落。
        // 端点整份没给 usage 就不发也不回写(保持未知,留在估算占位)。
        if let Some(u) = response.usage {
            on_event(AgentEvent::ContextUsage {
                used: u.input_tokens,
                total: MODEL_CONTEXT_WINDOW,
                cached: u.cached_tokens,
            });
            context.record_usage(u);
        }

        // 消息本体与结束原因分开取:三个分支都要用到 message,而 finish_reason
        // 决定去哪一支。不再有「分类对象」——直接 match 这个枚举。
        let message = response.message;
        match response.finish_reason {
            FinishReason::Stop => {
                let mut msg = message;
                let empty = match &msg {
                    Msg::Assistant(a) => a.content.trim().is_empty(),
                    _ => true,
                };
                if empty {
                    // refusal 已在协议层折进正文;走到这里说明正文和拒答全空,
                    // 一个 token 都没推过,前端会留下空气泡,补一段可展示文本。
                    let fallback = "（模型未返回内容，请重试）".to_string();
                    tracing::warn!(turn, "模型结束(Stop)但正文为空");
                    on_event(AgentEvent::Token(fallback.clone()));
                    if let Msg::Assistant(a) = &mut msg {
                        a.content = fallback;
                    }
                } else {
                    let content_len = match &msg {
                        Msg::Assistant(a) => a.content.len(),
                        _ => 0,
                    };
                    tracing::info!(turn, content_len, "模型结束(Stop)");
                }
                // 整条落库(自带 thinking),压缩判定交由 Context 内部。
                context.add(msg, persist)?;
                on_event(AgentEvent::Done);
                return Ok(());
            }
            FinishReason::ToolCalls => {
                let tool_calls = match &message {
                    Msg::Assistant(a) => a.tool_calls.clone(),
                    _ => Vec::new(),
                };
                tracing::info!(turn, tool_count = tool_calls.len(), "模型请求工具调用");
                // 先落"模型要调工具"这条 assistant 防失忆,且必须用原始 message:
                // 它带 thinking,下一轮带 tools 的请求要靠它回传 reasoning_content。
                context.add(message, persist)?;

                // 1) 串行决策:不发实现体、不 await,只产出"立即有结果"和"待执行"两堆。
                //    assess 必须留在串行阶段——弹窗与 grant 写入都不能并发(多个弹窗竞态、
                //    授权集合读写竞态)。
                let mut immediate: Vec<(usize, String, String, ToolStatus)> = Vec::new();
                let mut ready: Vec<Call> = Vec::new();
                for (idx, call) in tool_calls.iter().enumerate() {
                    let (id, name, args) =
                        (call.id.clone(), call.name.clone(), call.arguments.clone());

                    on_event(AgentEvent::ToolStart {
                        id: id.clone(),
                        name: name.clone(),
                        args: args.clone(),
                    });

                    // —— 鉴权在循环里做:toolbox.assess(name, args) 内部按 name 取工具并 assess ——
                    // 鉴权与执行分离:此处只决定 Allow/Deny/Ask,执行交给并发阶段。
                    // 要问人的两档就地停住等回答,所以「确认」永远发生在串行判定阶段,
                    // 与并发执行之间不存在竞态。
                    match toolbox.assess(&name, &args).await {
                        None => {
                            let r = format!("未知工具: {name}");
                            tracing::warn!("{r}");
                            immediate.push((idx, id, r, ToolStatus::Skipped));
                        }
                        Some(Action::Deny(reason)) => {
                            let r = format!("拒绝执行: {reason}");
                            tracing::warn!(tool = name, id, reason = %reason, "assess=Deny,拒绝执行");
                            immediate.push((idx, id, r, ToolStatus::Skipped));
                        }
                        Some(Action::Allow) => ready.push(Call {
                            idx,
                            id,
                            name,
                            args,
                        }),
                        // 只有这两档与用户有关:其余在循环内直接成型,不打扰人
                        Some(action @ (Action::AskOnce { .. } | Action::AskAlways)) => {
                            let (keys, rememberable) = match &action {
                                Action::AskOnce { keys } => (keys.clone(), true),
                                _ => (Vec::new(), false),
                            };
                            let req = AskRequest {
                                id: id.clone(),
                                name: name.clone(),
                                args: args.clone(),
                                keys: keys.clone(),
                                rememberable,
                            };
                            // 顺序固定:先在 confirm 的同步段登记等待,再发事件,最后等——
                            // 反过来会漏掉一个「人比登记更快」的回答
                            let pending = confirm(&req);
                            let ask_id = req.id.clone();
                            on_event(AgentEvent::Ask(req));
                            let decision = pending.await;
                            tracing::info!(tool = name, id = %ask_id, ?decision, "工具确认结果");
                            on_event(AgentEvent::AskClosed { id: ask_id });

                            match decision {
                                Decision::Allow => ready.push(Call {
                                    idx,
                                    id,
                                    name,
                                    args,
                                }),
                                // 只有 AskOnce 能落授权;AskAlways 前端不给该按钮,万一传了
                                // 也按「允许一次」处理,保证它永不记住
                                Decision::AlwaysAllow => {
                                    if rememberable {
                                        let granted = toolbox.grant(&name, &keys);
                                        tracing::info!(tool = name, keys = ?granted, "已记住授权");
                                    }
                                    ready.push(Call {
                                        idx,
                                        id,
                                        name,
                                        args,
                                    });
                                }
                                Decision::Deny => immediate.push((
                                    idx,
                                    id,
                                    "用户拒绝执行".to_string(),
                                    ToolStatus::Skipped,
                                )),
                            }
                        }
                    }
                }

                // 2) 并发执行:Semaphore 限流 + FuturesUnordered 先完成先回收。
                //    每个 tool_call_id 恰好产出一条结果(执行失败也转成文本结果,不丢 id)。
                let sem = tokio::sync::Semaphore::new(MAX_CONCURRENT_TOOLS);
                let mut inflight = FuturesUnordered::new();
                for call in ready {
                    let sem = &sem;
                    inflight.push(async move {
                        let _permit = match sem.acquire().await {
                            Ok(p) => p,
                            // 信号量只在 close 后报错,本函数从不 close,属不可达兜底
                            Err(_) => {
                                return (
                                    call.idx,
                                    call.id,
                                    "并发控制已关闭".to_string(),
                                    ToolStatus::Error,
                                );
                            }
                        };
                        let (content, status) = match tokio::select! {
                            biased;
                            // 取消 = 丢弃工具 future:select 丢掉未完成的 execute 后,
                            // 其局部变量随之 drop(kill_on_drop 子进程、Drop 守卫都在这一步
                            // 收尾)。spawn_blocking 闭包脱离了 future,由工具自己的 Drop
                            // 守卫负责通知(stop 标志 / deadline)
                            _ = cancel.cancelled() => {
                                return (
                                    call.idx,
                                    call.id,
                                    CANCELLED_TOOL_RESULT.to_string(),
                                    ToolStatus::Skipped,
                                );
                            }
                            r = toolbox.execute(&call.name, &call.args) => r,
                        } {
                            Ok(s) => (s, ToolStatus::Ok),
                            // 工具在取消信号之后才返回的错误(可能来自自己的检查点),
                            // 统一折算成占位结果,不当作执行失败
                            Err(_) if cancel.is_cancelled() => {
                                (CANCELLED_TOOL_RESULT.to_string(), ToolStatus::Skipped)
                            }
                            Err(e) => {
                                let r = format!("工具执行出错: {e}");
                                tracing::warn!("{r}");
                                (r, ToolStatus::Error)
                            }
                        };
                        (call.idx, call.id, content, status)
                    });
                }

                // 3) 统一回灌:按原始下标排序后串行写入 context + 推送。
                //    context 是 &mut,并发执行与落库天然分离,不存在并发写上下文。
                let mut done = immediate;
                while let Some(item) = inflight.next().await {
                    done.push(item);
                }
                done.sort_by_key(|(idx, _, _, _)| *idx);
                for (_, id, content, status) in done {
                    // 落库失败直接中止:已落库的结果是事实,缺的那几条由加载路径补占位,
                    // 不在这里硬塞——伪造「工具执行过」比缺一条更难排查。
                    context.add(Msg::tool(id.as_str(), content.as_str()), persist)?;
                    on_event(AgentEvent::ToolResult {
                        id,
                        content,
                        status,
                    });
                }
                if cancel.is_cancelled() {
                    // 结果已按 id 补齐,上下文保持合法;只是不再发起下一轮
                    tracing::info!(turn, "工具阶段被取消,已为全部 tool_call 补齐结果");
                    on_event(AgentEvent::Cancelled);
                    return Ok(());
                }
                // 继续下一轮,把工具结果喂回模型
            }
            // Length / ContentFilter / Other / Unknown 都不该被当成正常回复:
            // 消息是截断的或被拦的,落进历史只会污染后续轮次,统一报错终止。
            other => {
                let msg = format!("模型调用未正常结束（{other:?}）");
                tracing::warn!(turn, reason = ?other, "模型调用未正常结束");
                on_event(AgentEvent::Error(msg.clone()));
                return Err(anyhow::anyhow!(msg));
            }
        }
    }

    let msg = format!("达到最大轮次 {MAX_TURNS},agent 循环未收敛");
    tracing::warn!(max_turns = MAX_TURNS, "agent 循环未收敛");
    on_event(AgentEvent::Error(msg.clone()));
    Err(anyhow::anyhow!(msg))
}
