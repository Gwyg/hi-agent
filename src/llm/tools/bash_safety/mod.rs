//! bash 命令安全分类:按平台分流,各自实现 classify
//!
//! 设计:子模块完全自治,mod.rs 只 cfg 路由 re-export
//! 平台差异(分隔符/危险语法/命令清单)全在子模块内消化,不强行抽共享抽象
//!
//! 平台差异本质:
//! - unix:  `&` 是后台执行(危险语法,拦);分隔符 | && ; ||
//! - win:   `&` 是顺序连命令(分隔符,分段);分隔符 & && ; || |
//!           命令大小写不敏感;无 OS 沙箱,分类是唯一防线,白名单更保守

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::classify;
#[cfg(windows)]
pub use windows::classify;
