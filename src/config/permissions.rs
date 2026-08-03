//! 权限配置:按工具分组的 pattern → action 规则 + 匹配引擎
//! 数据(PermissionsConfig)与操作(config_match)同处一地:定义、加载、查询匹配统一管理
//! 会话级授权记忆(grant)在 Toolbox,不在此处

use indexmap::IndexMap;
use serde::Deserialize;

/// 权限配置:bash 命令的 pattern → action 规则(IndexMap 保留声明顺序)
/// 规则按声明顺序匹配,先命中先生效
/// 注:write/edit 工具当前不走此配置(assess 硬编码为白名单内统一 Ask),故不含对应字段
#[derive(Deserialize, Clone, Default)]
pub struct PermissionsConfig {
    #[serde(default)]
    pub bash: IndexMap<String, PermissionAction>,
}

/// 单条权限规则的 action
/// config.toml 格式:"deny" / "allow" / { ask = true } / { ask = false }
#[derive(Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum PermissionAction {
    Simple(SimpleAction),
    Ask { ask: bool },
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "lowercase")]
pub enum SimpleAction {
    Deny,
    Allow,
}

/// 初始化权限:permissions 已随 Config 存入全局,各模块按需 config::get() 读取
/// bash_safety::classify 调 config_match 做规则匹配;会话级 grant 在 Toolbox
pub(super) fn init_permissions(_config: &PermissionsConfig) -> anyhow::Result<()> {
    Ok(())
}

/// 拆分 bash 命令为子命令列表(按 & | ; 分隔符)
/// config_match 用,保证规则匹配按子命令粒度(避免 "ls *" 放行 "ls && rm x")
/// 简单方案:不解析引号内的操作符(边缘情况,YAGNI)
fn split_bash_subcommands(command: &str) -> Vec<String> {
    command
        .split(['&', '|', ';'])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// 配置规则匹配
/// - bash:拆子命令逐个匹配,合并结果(任一 deny→deny;任一 ask=false→ask=false;
///   任一 ask=true→ask=true;任一未命中→None 走默认;全 allow→allow)
///   这样 "ls *" = "allow" 只放行单条 ls,不会放行 "ls && rm x"
/// - write/edit:对单路径整串匹配(路径无串联)
/// 规则按声明顺序匹配(toml preserve_order 保证),先命中先生效
///
/// 返回 (action, trigger_keys):trigger_keys 是触发 ask=true 的子命令列表
/// (bash 用,供 assess 生成授权 key);其他情况(deny/allow/ask=false)trigger_keys 空
pub fn config_match(
    config: &PermissionsConfig,
    tool: &str,
    command: &str,
) -> Option<(PermissionAction, Vec<String>)> {
    // 当前仅 bash 走配置规则;write/edit 的 assess 硬编码,不查此处
    if tool != "bash" {
        return None;
    }
    let rules = &config.bash;
    let subs = split_bash_subcommands(command);
    // 空命令(拆分后为空):按整串走单条匹配,兼容
    let targets = if subs.is_empty() {
        vec![command.to_string()]
    } else {
        subs
    };
    match_bash_rules(rules, &targets)
}

/// bash 多子命令规则合并
/// 优先级:deny > ask=false > ask=true > (任一未命中→None) > 全 allow
/// 返回 (合并 action, 触发 ask=true 的子命令列表)
/// trigger_keys 只在合并为 ask=true 时非空(供 assess 生成授权 key);其余情况空
fn match_bash_rules(
    rules: &IndexMap<String, PermissionAction>,
    subs: &[String],
) -> Option<(PermissionAction, Vec<String>)> {
    let mut has_ask_true = false;
    let mut has_ask_false = false;
    let mut ask_true_subs = Vec::new();
    for sub in subs {
        match match_bash_single(rules, sub) {
            // deny 最高优先级,立即返回(无 keys)
            Some(PermissionAction::Simple(SimpleAction::Deny)) => {
                return Some((PermissionAction::Simple(SimpleAction::Deny), Vec::new()));
            }
            // allow:累积,看其他子命令
            Some(PermissionAction::Simple(SimpleAction::Allow)) => {}
            Some(PermissionAction::Ask { ask: false }) => has_ask_false = true,
            // 收集触发 ask=true 的子命令(用于生成授权 key)
            Some(PermissionAction::Ask { ask: true }) => {
                has_ask_true = true;
                ask_true_subs.push(sub.clone());
            }
            // 任一未命中:不能 allow,走默认 assess
            None => return None,
        }
    }
    // deny 已提前返回;合并 ask(ask=false 每次问,严于 ask=true 可永久)
    if has_ask_false {
        Some((PermissionAction::Ask { ask: false }, Vec::new()))
    } else if has_ask_true {
        Some((PermissionAction::Ask { ask: true }, ask_true_subs))
    } else {
        // 全 allow
        Some((PermissionAction::Simple(SimpleAction::Allow), Vec::new()))
    }
}

/// 单条命令/路径规则匹配:按声明顺序,返回首个命中
fn match_single(
    rules: &IndexMap<String, PermissionAction>,
    target: &str,
) -> Option<PermissionAction> {
    for (pattern, action) in rules.iter() {
        if wildcard_match(pattern, target) {
            return Some(action.clone());
        }
    }
    None
}

/// bash 单子命令匹配:文件规则优先,未命中回退内置默认基线(危险命令 ask)
/// "文件优先、内置兜底":用户可在 config 覆盖内置(如把 sudo 从 ask 改 allow)
fn match_bash_single(
    rules: &IndexMap<String, PermissionAction>,
    target: &str,
) -> Option<PermissionAction> {
    match_single(rules, target).or_else(|| match_single(default_bash_rules(), target))
}

/// 内置 bash 默认基线:危险但合法的命令,默认 ask=true(问一次可永久)
/// 安全底线,删掉 config.toml 也生效;致命命令(rm -rf / 等)由 bash_safety::find_fatal 管,不在此
/// 用户可在 [permissions.bash] 用相同 pattern 覆盖(文件优先)
fn default_bash_rules() -> &'static IndexMap<String, PermissionAction> {
    static RULES: std::sync::OnceLock<IndexMap<String, PermissionAction>> =
        std::sync::OnceLock::new();
    RULES.get_or_init(|| {
        let mut m = IndexMap::new();
        for pattern in [
            "sudo *",
            "rm *",
            "git push --force *",
            "git push -f *",
            "git reset --hard*",
            "npm publish",
        ] {
            m.insert(pattern.to_string(), PermissionAction::Ask { ask: true });
        }
        m
    })
}

/// 通配符匹配:`*` 匹配任意(含空格/斜杠),`?` 匹配单字符
/// 不引入 glob/glob 的 `*` 不跨 `/`,命令匹配需跨 `/`(路径)
fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    match_inner(&p, 0, &t, 0)
}

fn match_inner(p: &[char], pi: usize, t: &[char], ti: usize) -> bool {
    if pi == p.len() {
        return ti == t.len();
    }
    match p[pi] {
        '*' => {
            for skip in ti..=t.len() {
                if match_inner(p, pi + 1, t, skip) {
                    return true;
                }
            }
            false
        }
        '?' => ti < t.len() && match_inner(p, pi + 1, t, ti + 1),
        c => ti < t.len() && t[ti] == c && match_inner(p, pi + 1, t, ti + 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_exact() {
        assert!(wildcard_match("cargo build", "cargo build"));
        assert!(!wildcard_match("cargo build", "cargo test"));
        assert!(!wildcard_match("cargo build", "cargo build --release"));
    }

    #[test]
    fn wildcard_star_end() {
        assert!(wildcard_match("rm -rf *", "rm -rf /tmp"));
        assert!(wildcard_match("rm -rf *", "rm -rf /tmp/x"));
        assert!(wildcard_match("ls *", "ls -la"));
        assert!(!wildcard_match("rm -rf *", "rm /tmp"));
    }

    #[test]
    fn wildcard_star_mid() {
        assert!(wildcard_match("mkfs*", "mkfs.ext4 /dev/sda"));
        assert!(wildcard_match("mkfs*", "mkfs"));
    }

    #[test]
    fn wildcard_star_match_slash() {
        assert!(wildcard_match("rm *", "rm -rf /tmp/x"));
    }

    #[test]
    fn wildcard_question() {
        assert!(wildcard_match("cargo ?", "cargo x"));
        assert!(!wildcard_match("cargo ?", "cargo xy"));
    }

    // --- bash 子命令拆分 + 合并语义 ---

    #[test]
    fn split_bash_subcommands_basic() {
        assert_eq!(split_bash_subcommands("ls -a"), vec!["ls -a"]);
        assert_eq!(split_bash_subcommands("ls && rm x"), vec!["ls", "rm x"]);
        assert_eq!(split_bash_subcommands("a; b | c"), vec!["a", "b", "c"]);
        assert!(split_bash_subcommands("").is_empty());
        assert!(split_bash_subcommands("   ").is_empty());
    }

    /// 构造测试配置(顺序敏感,模拟 toml preserve_order):
    /// rm -rf / → deny; rm * → ask=false; ls * → allow; cargo build → allow
    fn bash_rules() -> PermissionsConfig {
        let mut config = PermissionsConfig::default();
        config.bash.insert(
            "rm -rf /".into(),
            PermissionAction::Simple(SimpleAction::Deny),
        );
        config
            .bash
            .insert("rm *".into(), PermissionAction::Ask { ask: false });
        config
            .bash
            .insert("ls *".into(), PermissionAction::Simple(SimpleAction::Allow));
        config.bash.insert(
            "cargo build".into(),
            PermissionAction::Simple(SimpleAction::Allow),
        );
        config
            .bash
            .insert("git push *".into(), PermissionAction::Ask { ask: true });
        config
    }

    #[test]
    fn bash_single_allow() {
        let cfg = bash_rules();
        assert!(matches!(
            config_match(&cfg, "bash", "ls -a"),
            Some((PermissionAction::Simple(SimpleAction::Allow), _))
        ));
    }

    #[test]
    fn bash_compound_allow_plus_ask_not_bypassed() {
        let cfg = bash_rules();
        assert!(matches!(
            config_match(&cfg, "bash", "ls -a && rm x"),
            Some((PermissionAction::Ask { ask: false }, _))
        ));
    }

    #[test]
    fn bash_compound_deny_wins() {
        let cfg = bash_rules();
        assert!(matches!(
            config_match(&cfg, "bash", "ls -a && rm -rf /"),
            Some((PermissionAction::Simple(SimpleAction::Deny), _))
        ));
    }

    #[test]
    fn bash_compound_unmatched_falls_through() {
        let cfg = bash_rules();
        assert!(config_match(&cfg, "bash", "ls -a && unknown_cmd").is_none());
        assert!(config_match(&cfg, "bash", "cargo build && unknown").is_none());
    }

    #[test]
    fn bash_bare_command_unmatched() {
        let cfg = bash_rules();
        assert!(config_match(&cfg, "bash", "ls").is_none());
    }

    #[test]
    fn bash_compound_all_allow() {
        let cfg = bash_rules();
        assert!(matches!(
            config_match(&cfg, "bash", "ls -a && cargo build"),
            Some((PermissionAction::Simple(SimpleAction::Allow), _))
        ));
    }

    #[test]
    fn bash_compound_ask_true_merged() {
        let cfg = bash_rules();
        let result = config_match(&cfg, "bash", "cargo build && git push origin");
        assert!(matches!(
            result,
            Some((PermissionAction::Ask { ask: true }, _))
        ));
        if let Some((_, keys)) = result {
            assert_eq!(keys, vec!["git push origin".to_string()]);
        }
        let result = config_match(&cfg, "bash", "rm x && git push origin");
        assert!(matches!(
            result,
            Some((PermissionAction::Ask { ask: false }, _))
        ));
        if let Some((_, keys)) = result {
            assert!(keys.is_empty(), "ask=false 时 keys 应空,实际 {keys:?}");
        }
    }

    #[test]
    fn bash_ask_true_keys_only_triggered() {
        let cfg = bash_rules();
        let result = config_match(&cfg, "bash", "git push origin && git push other");
        assert!(matches!(
            result,
            Some((PermissionAction::Ask { ask: true }, _))
        ));
        if let Some((_, keys)) = result {
            assert_eq!(
                keys,
                vec!["git push origin".to_string(), "git push other".to_string()]
            );
        }
        let result = config_match(&cfg, "bash", "ls -a && cargo build");
        if let Some((_, keys)) = result {
            assert!(keys.is_empty(), "全 allow 时 keys 应空,实际 {keys:?}");
        }
    }

    #[test]
    fn bash_empty_command() {
        let cfg = bash_rules();
        assert!(config_match(&cfg, "bash", "").is_none());
    }

    #[test]
    fn non_bash_tool_returns_none() {
        // write/edit 等非 bash 工具不走配置规则,统一返 None(由各自 assess 硬编码处理)
        let cfg = bash_rules();
        assert!(config_match(&cfg, "write", "*.env").is_none());
        assert!(config_match(&cfg, "edit", "foo.rs").is_none());
    }

    // --- 内置默认基线:文件优先、内置兜底 ---

    #[test]
    fn builtin_bash_defaults_apply_without_config() {
        // 空配置:危险命令命中内置默认(ask=true)
        let empty = PermissionsConfig::default();
        assert!(matches!(
            config_match(&empty, "bash", "sudo apt install x"),
            Some((PermissionAction::Ask { ask: true }, _))
        ));
        assert!(matches!(
            config_match(&empty, "bash", "rm -f foo"),
            Some((PermissionAction::Ask { ask: true }, _))
        ));
        assert!(matches!(
            config_match(&empty, "bash", "git reset --hard HEAD~1"),
            Some((PermissionAction::Ask { ask: true }, _))
        ));
        // 非危险且无文件规则:仍未命中,走默认安全引擎
        assert!(config_match(&empty, "bash", "some_unknown_cmd").is_none());
    }

    #[test]
    fn file_rule_overrides_builtin() {
        // 文件把 sudo 设为 allow → 覆盖内置的 ask
        let mut cfg = PermissionsConfig::default();
        cfg.bash.insert(
            "sudo *".into(),
            PermissionAction::Simple(SimpleAction::Allow),
        );
        assert!(matches!(
            config_match(&cfg, "bash", "sudo ls"),
            Some((PermissionAction::Simple(SimpleAction::Allow), _))
        ));
    }
}
