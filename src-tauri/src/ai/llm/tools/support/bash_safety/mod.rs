//! bash 命令安全分类:按平台分流,各自实现 classify
//!
//! 设计:子模块完全自治,mod.rs 只 cfg 路由 + 统一入口签名
//!
//! 平台差异:
//! - unix:    tree-sitter-bash 出 AST(bash 无「只解析不执行」的系统接口,见 unix.rs 文档);
//!   `&` 是后台执行,危险语法一律拦
//! - windows: 起 PowerShell 自己的 Parser 拿 AST(见 powershell.rs);无 OS 沙箱,
//!   分类是唯一防线,故「解析不出来」一律 AskAlways
//!
//! unix 模块额外在 `cfg(test)` 下编译:开发机是 Windows,只有这样才能跑它的解析/判定用例
//!
//! 匹配大小写不敏感两平台一致(`compile_one` 统一;数据归一在各 classify 入口)

/// 配置规则匹配引擎(消费 config 数据,产出 `Action`);平台无关,两平台共用
mod matcher;
#[cfg(windows)]
mod powershell;
/// 测试期在非 unix 平台也编译,否则本文件正文永远编不到、无法验证
#[cfg(any(unix, test))]
#[cfg_attr(not(unix), allow(dead_code))]
mod unix;

/// 平台分类入口:**统一 async 签名**——unix 是纯内存判断,Windows 要起一个解析器进程;
/// 调用方(工具的 assess)只认一个签名,不为平台分支写两套代码
#[cfg(unix)]
pub async fn classify(cmd: &str) -> crate::ai::llm::tools::Action {
    unix::classify(cmd)
}
/// 授权集合的测试隔离夹具:写全局授权的跨模块用例持之串行,Drop 清空;
/// 导出给 tools/mod.rs 的 `Toolbox::grant` 测试
#[cfg(test)]
pub(crate) use matcher::with_isolated_grants;
pub(crate) use matcher::{
    check_user_policy, matches_allow_rules, matches_ask_always_rules, matches_deny_rules,
    max_action, record_grant, DENY_REASON,
};
#[cfg(windows)]
pub use powershell::classify;
