//! AI 能力层:本项目**特有的能力**集合(agent 循环 + LLM 客户端/消息/工具)。
//!
//! 为什么单收一层:
//! - `config/`(配置)与 `util/`(横切基础原语)任何项目都要有,属于共享关注点,留在外面;
//! - `commands/`(接口层)与 `services/`(应用层)是**架构层**;
//! - 本目录装的是 hi-agent 这个项目才有的东西——它们是"能力"而不是"层",
//!   因此收在同一容器下,不与上面四类平级(避免"层"与"能力"两个维度混在同一级)。
//!
//! 层内依赖方向:`agent` → `llm`(llm 是更基础的一层,agent 循环在其上组装),
//! 反向依赖不允许。二者都不依赖 `commands` / `services`。
//!
//! 对外只有一扇门:上层(`services::conversation`)经用例入口调用,命令层不直接触碰本目录。
//!
//! 传输层边界:领域循环本身不 emit、不序列化(进度以 `AgentEvent` 交给调用方翻译),
//! 但 `agent::context` 目前用了 `tauri::async_runtime::Mutex` 持有单例——
//! 若将来要拆成独立 crate,须先把它换成非 Tauri 的异步锁。

pub mod agent;
pub mod llm;
