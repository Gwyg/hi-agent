//! 对话命令:对外接口层的纯翻译。
//!
//! 本文件不含业务逻辑:装配、加锁、跑循环都在 `services::conversation`;
//! 这里只做两件事——把 invoke 参数喂给用例函数,把领域 `AgentEvent` 翻成
//! Tauri IPC 事件。传输关注点(DTO / emit)收口在本层,领域与服务层不认识它们。

use crate::ai::agent::{AgentEvent, Decision, ToolStatus};
use crate::services::conversation::TrimNotify;
use crate::store::Store;
use serde::Serialize;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};

/// 工具开始事件载荷(推给前端,供后续渲染工具调用卡片)。
/// 这是**传输层 DTO**,只在命令层构造,领域循环(agent)不认识它。
#[derive(Serialize, Clone)]
pub struct ToolStartPayload {
    pub id: String,
    pub name: String,
    pub args: String,
}

/// 工具结果事件载荷。`status` 是给 UI 的展示提示(成功 / 出错 / 未执行),
/// 不影响上下文里那份文本结果。
#[derive(Serialize, Clone)]
pub struct ToolResultPayload {
    pub id: String,
    pub content: String,
    pub status: &'static str,
}

/// 一次模型请求的用量快照。`used` 来自端点 usage 的 input_tokens,
/// `total` 是模型窗口,`cached` 是 `used` 里命中提示词缓存的部分
/// (`prompt_cache_hit_tokens`);百分比由前端算,这里不做展示口径的换算。
#[derive(Serialize, Clone)]
pub struct ContextUsagePayload {
    pub used: u32,
    pub total: u32,
    pub cached: u32,
}

/// 工具确认事件载荷:前端据此弹窗,`id` 原样回传给 `answer_ask`
#[derive(Serialize, Clone)]
pub struct AskPayload {
    pub id: String,
    pub name: String,
    pub args: String,
    pub keys: Vec<String>,
    /// 是否允许「总是允许」按钮(AskAlways 为 false)
    pub rememberable: bool,
}

/// 后台清理完成事件载荷。
///
/// `messages` 是这个会话被删掉的旧消息条数，`sessions` 是本工作区被删掉的旧会话个数
/// （两者都可能为 0，事件只在至少一个非 0 时发出）。前端据此提示「更早的内容已清理」——
/// 删除不需要用户确认，但必须让他知道。
#[derive(Serialize, Clone)]
pub struct HistoryTrimmedPayload {
    pub session_id: String,
    pub messages: usize,
    pub sessions: usize,
}

/// 流式对话命令(已接 agent 循环):
/// 前端只传**本轮用户输入** `user_input`,后端从会话上下文(进程级单例,由 context 模块持有)取历史;
/// 逐 token 经 `llm-token` 推回,思考增量经 `llm-thinking`(与正文分开渲染),
/// 工具调用经 `agent-tool-start`/`agent-tool-result`,
/// 每轮模型请求的用量经 `llm-usage` 推一次,结束发 `llm-done`,
/// 出错发 `llm-error`,用户取消发 `llm-cancelled`(取消不是错误)。
///
/// 历史不往返前端:Context 才是对话上下文的主人,长对话下只保持精简(压缩一次不回弹)。
/// 每轮消息由服务层落进 SQLite(见 `services::conversation`),`seq` 不回内存。
///
/// 返回本轮消息所属的会话 id（空串 = 一条都没落成）：前端据此刷新侧边栏列表。
/// 事件先于返回值到达，所以前端不能指望在 `llm-done` 里拿到它——要等 invoke 返回。
#[tauri::command]
pub async fn chat_stream(
    app: AppHandle,
    store: State<'_, Arc<Store>>,
    user_input: String,
) -> Result<String, String> {
    // 事件翻译:领域 AgentEvent → Tauri IPC 事件(传输关注点收口在命令层)。
    let app2 = app.clone();
    let on_event = move |e: AgentEvent| match e {
        AgentEvent::Token(t) => {
            let _ = app2.emit("llm-token", t);
        }
        AgentEvent::Thinking(t) => {
            let _ = app2.emit("llm-thinking", t);
        }
        AgentEvent::ToolStart { id, name, args } => {
            let _ = app2.emit("agent-tool-start", ToolStartPayload { id, name, args });
        }
        AgentEvent::ToolResult {
            id,
            content,
            status,
        } => {
            let status = match status {
                ToolStatus::Ok => "ok",
                ToolStatus::Error => "error",
                ToolStatus::Skipped => "skipped",
            };
            let _ = app2.emit(
                "agent-tool-result",
                ToolResultPayload {
                    id,
                    content,
                    status,
                },
            );
        }
        AgentEvent::Ask(req) => {
            let _ = app2.emit(
                "agent-ask",
                AskPayload {
                    id: req.id,
                    name: req.name,
                    args: req.args,
                    keys: req.keys,
                    rememberable: req.rememberable,
                },
            );
        }
        // 已作废(回答/超时/取消):前端只负责关窗,不再回传任何东西
        AgentEvent::AskClosed { id } => {
            let _ = app2.emit("agent-ask-closed", id);
        }
        AgentEvent::ContextUsage {
            used,
            total,
            cached,
        } => {
            let _ = app2.emit(
                "llm-usage",
                ContextUsagePayload {
                    used,
                    total,
                    cached,
                },
            );
        }
        AgentEvent::Cancelled => {
            let _ = app2.emit("llm-cancelled", ());
        }
        AgentEvent::Done => {
            let _ = app2.emit("llm-done", ());
        }
        AgentEvent::Error(m) => {
            let _ = app2.emit("llm-error", m);
        }
    };

    // 后台清理完成的通知。回调在清理线程里执行，这里只负责投成 IPC 事件。
    let app_trim = app.clone();
    let on_trim: TrimNotify = Arc::new(move |n| {
        let _ = app_trim.emit(
            "history-trimmed",
            HistoryTrimmedPayload {
                session_id: n.session_id,
                messages: n.messages,
                sessions: n.sessions,
            },
        );
    });

    match crate::services::conversation::run_turn(
        store.inner().clone(),
        &user_input,
        on_event,
        on_trim,
    )
    .await
    {
        Ok(session_id) => Ok(session_id),
        Err(e) => {
            // run_agent_turn 的 Error 路径(截断/过滤/未收敛)已发过 llm-error;
            // 其余错误(网络/序列化等经 `?` 传播的)循环内不发事件,这里补发,
            // 前端 invoke 的 catch 会一并兜底(由 finished 守卫保证只触发一次 onError)
            let _ = app.emit("llm-error", e.to_string());
            Err(e.to_string())
        }
    }
}

/// 新对话:结束当前会话、清空后端持有的上下文(对应前端"新对话"按钮)。
/// 已落库的消息与那行会话都留着——它还在侧边栏里,随时能点回去。
#[tauri::command]
pub async fn new_chat() -> Result<(), String> {
    crate::services::conversation::reset().await;
    Ok(())
}

/// 取消当前轮次(对应前端"停止"按钮)。
/// 返回 false 表示没有在跑的轮次;真正结束时 `chat_stream` 会补发 `llm-cancelled`。
#[tauri::command]
pub fn cancel_chat() -> bool {
    crate::services::conversation::cancel()
}

/// 回答工具确认弹窗(对应 `agent-ask`)。
///
/// `decision` 取 `allow` / `always` / `deny`;返回 false 表示这条请求已经不在等了
/// (超时、已答过、或用户点了停止)。未知取值同样返回 false,不当作错误上抛。
#[tauri::command]
pub fn answer_ask(id: String, decision: String) -> bool {
    let decision = match decision.as_str() {
        "allow" => Decision::Allow,
        "always" => Decision::AlwaysAllow,
        "deny" => Decision::Deny,
        _ => {
            tracing::warn!(%id, decision, "确认回答取值未知,按已作废忽略");
            return false;
        }
    };
    crate::services::conversation::answer(&id, decision)
}
