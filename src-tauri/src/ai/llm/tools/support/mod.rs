//! 工具的共享支撑方法:被各具体工具(read/write/edit/bash…)调用的可复用基础方法,
//! 与工具的 schema / 执行逻辑无关。
//!
//! 容器层只描述**角色**(支撑工具实现),不预设**职责**——当前成员里
//! path_scope 与 bash_safety 是安全校验、shell 是执行侧 shell 选择,但结构上允许任何
//! 工具共用的辅助方法平铺进来(如记忆类辅助):新增 = 加一个文件 + 本文件加一行声明,
//! 层级不变。
//!
//! 配置值不在此层:路径范围配置归 `config::path_scope`、bash 规则归
//! `config::permissions`;本层只放「读配置并给出判定」的方法——纯粹的路径解析在 `util::path`。

pub(crate) mod bash_safety;
pub mod path_scope;
pub(crate) mod process_tree;
pub(crate) mod shell;
pub(crate) mod text;
