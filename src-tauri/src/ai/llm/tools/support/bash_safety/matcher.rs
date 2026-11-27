//! 权限规则匹配引擎:消费 `config::BashPolicy`(用户规则 + 四档内置默认)产出运行时 `Action`。
//! 本模块收口「所有对 `BashPolicy` 的匹配」,平台差异只在 windows/unix 的命令解析,匹配逻辑不散落。
//! 纯逻辑,不解析命令字符串(子命令拆分由平台 classify 完成);config 不向上依赖 `llm::tools`。

use crate::ai::llm::tools::Action;
use crate::config::{bash_policy, ActionKind};
use globset::{GlobBuilder, GlobMatcher};
use indexmap::IndexMap;
use std::collections::HashSet;
use std::sync::{LazyLock, RwLock};

// ============ 全局静态 ============
//
// 上不上锁只看一件事:`init` 之后还会不会被写。
// - 不写 → 裸 `LazyLock`(六张编译缓存):编译一次,之后解引用只是一次原子 load,不是锁
// - 写 → 内层 `RwLock`,只有一个(`GRANTED_PATTERNS`):`is_granted` 读、`record_grant` 写,
//   各自取放、无嵌套持有
// 词表数据都在 `config::BashPolicy`,本文件只放编译缓存与匹配逻辑。

/// 致命红线表(内置 + 用户 deny 合表,只增不改)
static COMPILED_DENY: LazyLock<Vec<GlobMatcher>> =
    LazyLock::new(|| compile_globs(&bash_policy().deny_rules));

/// 危险规则表(内置 + 用户 always_ask 合表,只增不改)
static COMPILED_ASK_ALWAYS: LazyLock<Vec<GlobMatcher>> =
    LazyLock::new(|| compile_globs(&bash_policy().ask_always_rules));

/// allow 白名单(纯内置;用户 allow 走用户层)
static COMPILED_ALLOW: LazyLock<Vec<GlobMatcher>> =
    LazyLock::new(|| compile_globs(&bash_policy().allow_rules));

/// allow 的伴随排除表(纯内置):路径与可执行文件引用先被它拦下,
/// 防 `dir.bat` 影子内建 `dir`
static COMPILED_FILE_REF: LazyLock<Vec<GlobMatcher>> =
    LazyLock::new(|| compile_globs(&bash_policy().file_ref_rules));

/// 内置基线(`ask_once_rules` → AskOnce):只存 (匹配器, pattern 原文),原文用于出授权键
static COMPILED_ASK_ONCE: LazyLock<Vec<(GlobMatcher, String)>> =
    LazyLock::new(|| compile_patterns(&bash_policy().ask_once_rules));

/// 用户规则(`[permissions.bash]`,合表后只剩 allow/ask)
static USER_COMPILED: LazyLock<Vec<CompiledRule>> =
    LazyLock::new(|| compile_rules(&bash_policy().user_bash_rules));

/// 会话级授权记忆:存命中的 pattern 原文。`record_grant` 是唯一运行期写入方,故这里
/// 用真锁;存原文而非匹配器,查询是 O(1) 相等,还能枚举/撤销
static GRANTED_PATTERNS: LazyLock<RwLock<HashSet<String>>> =
    LazyLock::new(|| RwLock::new(HashSet::new()));

// ============ 通用匹配原语 ============

/// 编译单条 pattern(全模块唯一入口):大小写不敏感 + `literal_separator(false)`。
/// 后者让 `*` 跨 `/` 与 `\`(命令实参里常有路径),且 pattern 里的 `\` 是转义符。
/// 写错的 pattern 返回 `None`(该规则静默不生效),不 panic。
fn compile_one(pattern: &str) -> Option<GlobMatcher> {
    GlobBuilder::new(pattern)
        .case_insensitive(true)
        .literal_separator(false)
        .build()
        .ok()
        .map(|g| g.compile_matcher())
}

/// 词表预编译:一次编译,匹配时不再解析 glob
fn compile_globs(rules: &[String]) -> Vec<GlobMatcher> {
    rules.iter().filter_map(|p| compile_one(p)).collect()
}

// ============ 词表判定(平台无关,两平台共用) ============

/// 命中危险词表(内置 + 用户,大小写不敏感)→ AskAlways
pub(crate) fn matches_ask_always_rules(cmd: &str) -> bool {
    COMPILED_ASK_ALWAYS.iter().any(|m| m.is_match(cmd))
}

/// 首词是否在只读白名单内。文件引用(带路径分隔符/可执行扩展名)先被排除,
/// 防 `dir.bat` 影子内建 `dir` 被静默 Allow
pub(crate) fn matches_allow_rules(first: &str) -> bool {
    !COMPILED_FILE_REF.iter().any(|m| m.is_match(first))
        && COMPILED_ALLOW.iter().any(|m| m.is_match(first))
}

/// Deny 文案不分来源:合表后「谁配的」对判定无意义
pub(crate) const DENY_REASON: &str = "命令被安全策略拒绝";

/// 命中致命红线表(内置 + 用户合表)→ Deny
pub(crate) fn matches_deny_rules(cmd: &str) -> bool {
    COMPILED_DENY.iter().any(|m| m.is_match(cmd))
}

// ============ pattern 规则匹配 ============

/// 编好的用户规则;`pattern` 留原文,命中后作授权键
struct CompiledRule {
    matcher: GlobMatcher,
    kind: ActionKind,
    pattern: String,
}

/// 编译纯 pattern 表(内置基线),原文只为出授权键
fn compile_patterns(rules: &[String]) -> Vec<(GlobMatcher, String)> {
    rules
        .iter()
        .filter_map(|p| compile_one(p).map(|matcher| (matcher, p.clone())))
        .collect()
}

fn compile_rules(rules: &IndexMap<String, ActionKind>) -> Vec<CompiledRule> {
    rules
        .iter()
        .filter_map(|(p, a)| {
            compile_one(p).map(|matcher| CompiledRule {
                matcher,
                kind: a.clone(),
                pattern: p.clone(),
            })
        })
        .collect()
}

/// 记录授权:插入 pattern 原文(该 pattern 的 AskOnce 段此后按 Allow 计)。
/// 会话级、不落盘,重复插入无害。
pub(crate) fn record_grant(pattern: &str) {
    if let Ok(mut g) = GRANTED_PATTERNS.write() {
        g.insert(pattern.to_string());
    }
}

/// 本段命中的 pattern 是否已授权(精确相等)。按子命令重新 glob 匹配语义更宽——
/// 任何能匹配它的授权都会放行,用户不可理解,故只认命中的那一条
fn is_granted(pattern: &str) -> bool {
    GRANTED_PATTERNS.read().unwrap().contains(pattern)
}

/// 用户规则匹配:首个命中即用(声明序),返回 (处置, pattern 原文作授权键)
fn match_user_rules(target: &str) -> Option<(ActionKind, &'static str)> {
    USER_COMPILED
        .iter()
        .find(|r| r.matcher.is_match(target))
        .map(|r| (r.kind.clone(), r.pattern.as_str()))
}

/// 内置基线匹配:首个命中,返回 pattern(作授权键)
fn match_baseline(target: &str) -> Option<&'static str> {
    COMPILED_ASK_ONCE
        .iter()
        .find(|(m, _)| m.is_match(target))
        .map(|(_, p)| p.as_str())
}

/// 两级:先用户规则(命中即短路),再内置 ask_once 基线——用户可用同 pattern 覆盖内置
fn match_subcommand(target: &str) -> Option<(ActionKind, &'static str)> {
    if let Some(hit) = match_user_rules(target) {
        return Some(hit);
    }
    if let Some(pattern) = match_baseline(target) {
        return Some((ActionKind::Ask, pattern));
    }
    None
}

/// 逐段两级解析后汇总:
/// - 全段 Allow(用户 allow,或 ask 段已授权)→ `Some(Allow)`
/// - 有 ask 键 → `Some(AskOnce{keys})`,全段累加
/// - 无意见 / 未全覆盖 → `None`,交调用方走白名单合取与兜底
///
/// 「无意见」不算 allow:未知段不能因兄弟段安全被掩盖。
/// deny/always_ask 已在 `init` 合表进预查层,下方两臂只作防御。
pub(crate) fn check_user_policy(subcommands: &[String]) -> Option<Action> {
    if subcommands.is_empty() {
        return None;
    }
    let mut ask_keys: Vec<String> = Vec::new();
    let mut all_allow = true;
    for sub in subcommands {
        let Some((kind, pattern)) = match_subcommand(sub) else {
            all_allow = false; // 无意见:不算 allow,也不贡献键
            continue;
        };
        match kind {
            ActionKind::Allow => {}
            ActionKind::Ask => {
                if !is_granted(pattern) {
                    ask_keys.push(pattern.to_string());
                    all_allow = false;
                }
                // 已授权 → 按 allow 级计(不收键、不减 all_allow)
            }
            // 防御臂:init 合表后不可达。不用 `_` 通配(混入档位会被静默吞掉),也不 panic
            ActionKind::Deny => return Some(Action::Deny(DENY_REASON.into())),
            ActionKind::AlwaysAsk => return Some(Action::AskAlways),
        }
    }
    if all_allow {
        Some(Action::Allow)
    } else if !ask_keys.is_empty() {
        Some(Action::AskOnce { keys: ask_keys })
    } else {
        None
    }
}

// ============ 跨来源严重度合并(供各平台 classify 复用) ============

/// 裁决严重度(用于跨来源合并取最大):Deny(3) > AskAlways(2) > AskOnce(1) > Allow(0)
fn severity(a: &Action) -> u8 {
    match a {
        Action::Deny(_) => 3,
        Action::AskAlways => 2,
        Action::AskOnce { .. } => 1,
        Action::Allow => 0,
    }
}

/// 按严重度取更严重的裁决;`cur=None` 表示「本来源无意见」,直接用候选覆盖
pub(crate) fn max_action(cur: Option<Action>, cand: Action) -> Option<Action> {
    match cur {
        None => Some(cand),
        Some(c) => {
            if severity(&cand) >= severity(&c) {
                Some(cand)
            } else {
                Some(c)
            }
        }
    }
}

// ============ 测试 ============

/// 授权集合会被用例并行读写,凡 `record_grant` 或调 classify 的用例须先持此锁
#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// RAII 夹具:持锁,Drop 时清空授权,断言失败也不污染后续用例
#[cfg(test)]
pub(crate) struct GrantFixture {
    _guard: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
pub(crate) fn with_isolated_grants() -> GrantFixture {
    let guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    GrantFixture { _guard: guard }
}

#[cfg(test)]
impl Drop for GrantFixture {
    fn drop(&mut self) {
        if let Ok(mut g) = GRANTED_PATTERNS.write() {
            g.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ============ glob 匹配能力(compile_one 产物) ============

    /// 字符集 `[rf]` 匹配其中任一字符(wildmatch 不支持,换 globset 的主因);
    /// 大小写不敏感由编译期 `case_insensitive` 保证;`*` 跨 `/` 与 `\`(已实测:
    /// `*windows*` 命中 `rd /s C:\Windows`——输入按字符串匹配,非 Path 语义)
    #[test]
    fn glob_matcher_capabilities() {
        let m = compile_one("rm -[rf]f").unwrap();
        assert!(m.is_match("rm -rf"));
        assert!(m.is_match("rm -ff"));
        assert!(!m.is_match("rm -xf"));
        let m = compile_one("del*").unwrap();
        assert!(m.is_match("DEL /F"));
        let m = compile_one("*format-volume*").unwrap();
        assert!(m.is_match("Format-Volume"));
        let m = compile_one("*windows*").unwrap();
        assert!(m.is_match("rd /s C:\\Windows"));
        let m = compile_one("rd*").unwrap();
        assert!(m.is_match("rd /s C:\\Windows"));
        // pattern 写大写 + 数据小写(classify 入口归一的常态)必须仍命中:
        // case_insensitive(true) 是 pattern 域的大小写保障,编译期折叠,与数据域归一正交
        let m = compile_one("NPM *").unwrap();
        assert!(m.is_match("npm install"));
        assert!(m.is_match("NPM INSTALL"));
    }

    // ============ allow 白名单文件引用守卫(排除表) ============

    /// 文件引用不进 allow 白名单:可执行扩展名(含大写——unix 无入口归一,靠 pattern 域
    /// `case_insensitive` 编译期兜住)、两种路径分隔符,皆拒;裸内建词放行
    #[test]
    fn file_ref_guard_rejects_executables_and_paths() {
        // 拒绝侧(平台无关:排除表先于白名单判定)
        assert!(!matches_allow_rules("dir.bat")); // 影子内建
        assert!(!matches_allow_rules("echo.BAT")); // 大写后缀
        assert!(!matches_allow_rules("foo.cmd"));
        assert!(!matches_allow_rules("foo.exe"));
        assert!(!matches_allow_rules("foo.ps1"));
        assert!(!matches_allow_rules(r"C:\build.bat")); // 反斜杠路径
        assert!(!matches_allow_rules("c:/build.bat")); // 斜杠路径
                                                       // 通过侧:裸内建词(白名单内容分平台)
        #[cfg(windows)]
        assert!(matches_allow_rules("dir"));
        #[cfg(unix)]
        assert!(matches_allow_rules("ls"));
    }

    // ============ 用户规则层覆盖说明(无直测,与基线层对称) ============
    //
    // 用户规则匹配(`match_user_rules`)与基线匹配(`match_baseline`)同构:都读模块自有
    // 静态、都无直测——生产不为测试开注入口(全局注入 + RwLock、cfg 分流、纯函数传参
    // 三条路都试过,皆本末倒置,已全部回退),两端一并经 classify 端到端覆盖:
    // - 基线层:classify 用例走真实基线表(`del foo.txt` → AskOnce{del *})
    // - 用户层:默认配置(空用户规则)下与基线同路径;配了规则的场景走真实
    //   `[permissions.bash]`,不造测试数据
    // 汇总层三分支由 powershell.rs classify 端到端覆盖:ask 键累加 = 多行
    // `Get-ChildItem` + `Remove-Item build`;全段 Allow = grant 降级用例与 pipeline 用例;
    // 弃权 → None 由「含未知段」的 classify 兜底为 AskAlways。
    // 「用户先于基线」由 `match_subcommand` 短路结构保证。
    // 若将来要为用户层补直测,干净路径是配置层注入(permissions 测试钩子 + 用户规则
    // 缓存可重编译),而非匹配层注入

    // ============ 会话授权集合(record_grant / is_granted) ============

    /// 授权集合语义(与旧 `Vec<GlobMatcher>` 按子命令匹配的本质差异,钉死防回退):
    /// record_grant 插原文,is_granted 按**pattern 原文**精确命中(O(1))——子命令原文
    /// 不在集合内(查询键不是子命令);不同 pattern 即使能匹配同一子命令也互不授权
    /// (语义收紧:用户授权的是他看到的那个键,不是「任何能匹配它的授权」)。
    /// 端到端(授权后 classify 该 pattern 的 AskOnce 段 → Allow)见 powershell.rs
    /// 的 grant_downgrades_ask_once_to_allow 用例
    #[test]
    fn grant_set_matches_by_pattern_not_subcommand() {
        let _fx = with_isolated_grants();
        record_grant("del *");
        assert!(is_granted("del *"));
        assert!(!is_granted("del foo.txt")); // 子命令原文不是集合元素
        assert!(!is_granted("rd *")); // 未授权的 pattern 不命中
    }

    // ============ 严重度合并(classify 跨来源取最大) ============

    /// 格B 的 classify 侧保障:用户 ask 命中内置 allow 白名单命令时,Allow(最低档)
    /// 盖不住 AskOnce——severity 合并取最大,不按来源取舍
    #[test]
    fn severity_merge_keeps_more_severe() {
        let ask = || Action::AskOnce {
            keys: vec!["echo *".to_string()],
        };
        assert_eq!(max_action(Some(Action::Allow), ask()), Some(ask()));
        assert_eq!(
            max_action(Some(Action::AskAlways), ask()),
            Some(Action::AskAlways)
        );
        assert_eq!(max_action(None, Action::Allow), Some(Action::Allow));
    }
}
