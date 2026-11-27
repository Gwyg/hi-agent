//! Tauri 命令统一管理模块。
//!
//! 所有 `#[tauri::command]` 都放在这里（按功能拆到子模块），
//! `lib.rs` 只负责声明本模块并调用 `handler()` 注册，不写具体命令实现。
//!
//! 新增命令（两步缺一不可）：
//! 1. 在对应子模块加 `#[tauri::command]`（separate module 的命令必须 `pub`）
//! 2. 把「完整定义路径」加进下方 `handler()` 的 `generate_handler!`，放在本模块的注释分组内
//!
//! 命名规则：命令名**全局唯一**（不随模块作用域，跨模块也不能重名）；
//! 命令名即前端契约（`invoke("命令名")` 直接调，忽略 `chat::` 前缀），不要随意改。
//!
//! 为什么是单列表：Tauri 2 不支持分层注册——`invoke_handler` 多次调用**只保留
//! 最后一次**（官方文档明示），「每个模块自己注册」会让未最后注册的模块命令全部
//! 静默消失；插件机制（`plugin:xxx|命令`）可分散注册，但为独立分发 crate 设计，
//! 业务命令用它代价大于收益（见项目记忆 2026-10-25 讨论）。模块化只发生在
//! **实现侧**：某域命令涨到约 8 条、或单文件约 300 行时，把 `chat.rs` 目录化为
//! `chat/{mod,stream,history}.rs`，注册列表照旧只加行。
//!
//! 注意：命令通过「完整定义路径」(`chat::chat_stream`) 注册到
//! `generate_handler!`，不要在这里用 `pub use` 重导出——Tauri 宏生成的
//! `__cmd__*` 辅助函数会落在定义所在子模块，重导出会导致路径对不上而编译失败。

pub mod app_config;
pub mod chat;
pub mod misc;
pub mod session;

/// 汇总所有 Tauri 命令，供 `lib.rs` 的 `invoke_handler` 注册。
/// 新增命令只改这里，不用动 `lib.rs`，启动文件始终精简。
/// 列表按 `pub mod` 声明顺序分域排列（注释分组一一对应），新命令加进所属分组。
pub fn handler() -> impl Fn(tauri::ipc::Invoke<tauri::Wry>) -> bool + Send + Sync + 'static {
    tauri::generate_handler![
        // ===== 对话（chat.rs）=====
        chat::chat_stream,
        chat::cancel_chat,
        chat::answer_ask,
        chat::new_chat,
        // ===== 会话（session.rs）=====
        session::list_sessions,
        session::open_session,
        session::load_older,
        session::get_resume_session,
        session::rename_session,
        session::delete_session,
        // ===== 杂项（misc.rs，脚手架遗留）=====
        misc::greet,
        // ===== 应用配置（app_config.rs）=====
        app_config::get_workspace,
        app_config::set_workspace,
        app_config::list_workspaces,
        app_config::rename_workspace,
        app_config::delete_workspace,
        app_config::get_llm_config,
        app_config::save_llm_config,
        app_config::list_models,
        app_config::get_balance
    ]
}
