//! 引擎：真正与磁盘打交道的那一半，**只对 `store` 内部可见**。
//!
//! 划界：本模块只认识「连接 / 事务 / 参数化语句 / 迁移」，
//! 不认识会话、消息、工作空间——连这些词都不应该出现在本目录下。
//! 表名属于 schema（`sql/`，按版本全局有序），也不属于这里的函数。
//!
//! 边界只有一道，靠编译器：`store/mod.rs` 里写的是 `mod engine;`（无 `pub`），
//! `crate::store::engine::*` 在 `store` 之外连路径都拼不出来。
//!
//! 执行路径上**不造类型**：事务、行、绑定值全用 `rusqlite` 的，
//! `exec.rs` 只是把「prepare → query → next」与错误上下文收成三个函数。
//!
//! 内部布局：
//! - `conn`：打开、参数（WAL 等）、完整性检查、事务边界。
//! - `migrate`：版本号 + 有序迁移，以及正式的 [`migrate::MIGRATIONS`] 列表。
//! - `exec`：三个执行/查询函数（`execute` / `query_one` / `query_all`）。

mod conn;
pub(in crate::store) mod exec;
mod migrate;

pub(in crate::store) use conn::Conn;
pub use conn::Info;
