use indexmap::IndexMap;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use crate::llm::tools::sandbox;

/// 全局配置(启动时加载一次,会话内不变)
#[derive(Deserialize, Clone)]
pub struct Config {
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub permissions: PermissionsConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            sandbox: SandboxConfig::default(),
            permissions: PermissionsConfig::default(),
        }
    }
}

/// 沙箱配置
#[derive(Deserialize, Clone, Default)]
pub struct SandboxConfig {
    /// 额外白名单路径(项目根由 cwd 自动推断,不在此配置)
    /// 路径字符串,启动时展开 ~ + canonicalize
    #[serde(default)]
    pub extra_allowed: Vec<String>,
    /// 额外敏感后缀(ends_with 匹配,小写),如 "id_ecdsa"、".db"
    #[serde(default)]
    pub sensitive_suffixes: Vec<String>,
    /// 额外敏感路径前缀(starts_with 匹配,运行时展开 ~),如 "~/.config/hi-agent"
    #[serde(default)]
    pub sensitive_paths: Vec<String>,
}

/// 权限配置:按工具分组的 pattern → action 规则(IndexMap 保留声明顺序)
/// 规则按声明顺序匹配,先命中先生效
#[derive(Deserialize, Clone, Default)]
pub struct PermissionsConfig {
    #[serde(default)]
    pub bash: IndexMap<String, PermissionAction>,
    #[serde(default)]
    pub write: IndexMap<String, PermissionAction>,
    #[serde(default)]
    pub edit: IndexMap<String, PermissionAction>,
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

static CONFIG: RwLock<Option<Config>> = RwLock::new(None);

/// 启动时调一次:加载配置文件,依次初始化各子系统,最后存入全局
/// 配置文件不存在时用默认值,不报错
pub fn init() -> anyhow::Result<()> {
    let config = load_config()?;
    init_sandbox(&config.sandbox)?;
    init_permissions(&config.permissions)?;
    *CONFIG
        .write()
        .map_err(|e| anyhow::anyhow!("config 锁中毒: {e}"))? = Some(config);
    Ok(())
}

/// 按需读取配置
pub fn get() -> anyhow::Result<Config> {
    CONFIG
        .read()
        .map_err(|e| anyhow::anyhow!("config 锁中毒: {e}"))?
        .clone()
        .ok_or_else(|| anyhow::anyhow!("config 未初始化,启动时需 config::init()"))
}

/// 加载配置:用户级(~/.hi-agent/config.toml) + 项目级(.hi-agent/config.toml) deep merge
/// 合并:table 递归(相同 key 覆盖,不同 key 保留);标量/数组 override 覆盖 base
fn load_config() -> anyhow::Result<Config> {
    let user = load_value(&user_config_path()?.as_path())?;
    let project = load_value(&project_config_path()?.as_path())?;
    let merged = deep_merge(user, project);
    let config: Config = merged
        .try_into()
        .map_err(|e| anyhow::anyhow!("合并配置解析失败: {e}"))?;
    Ok(config)
}

/// 加载单个文件为 toml::Value,不存在返空 table
fn load_value(path: &Path) -> anyhow::Result<toml::Value> {
    if !path.exists() {
        tracing::info!("配置文件不存在,用默认值: {}", path.display());
        return Ok(toml::Value::Table(Default::default()));
    }
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("读取配置失败 {}: {e}", path.display()))?;
    let value: toml::Value = toml::from_str(&content)
        .map_err(|e| anyhow::anyhow!("解析配置失败 {}: {e}", path.display()))?;
    tracing::info!("已加载配置: {}", path.display());
    Ok(value)
}

/// deep merge:base 为基础(用户级),override 覆盖(项目级)
/// table:递归合并(相同 key 递归,不同 key 保留);标量/数组:override 直接覆盖
fn deep_merge(base: toml::Value, override_val: toml::Value) -> toml::Value {
    match (base, override_val) {
        (toml::Value::Table(mut base_t), toml::Value::Table(over_t)) => {
            for (k, v) in over_t {
                let merged = base_t
                    .remove(&k)
                    .map(|b| deep_merge(b, v.clone()))
                    .unwrap_or(v);
                base_t.insert(k, merged);
            }
            toml::Value::Table(base_t)
        }
        // 标量/数组:override 覆盖
        (_, o) => o,
    }
}

/// 用户级配置路径:~/.hi-agent/config.toml
fn user_config_path() -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("无法获取家目录"))?;
    Ok(home.join(".hi-agent").join("config.toml"))
}

/// 项目级配置路径:项目根/.hi-agent/config.toml
fn project_config_path() -> anyhow::Result<PathBuf> {
    Ok(sandbox::project_root()?
        .join(".hi-agent")
        .join("config.toml"))
}

/// 初始化沙箱:配置有 extra_allowed 就用,没有用默认(temp_dir)
/// 同时设置配置追加的敏感文件清单(空则只用内置清单)
fn init_sandbox(config: &SandboxConfig) -> anyhow::Result<()> {
    let extra: Vec<PathBuf> = if config.extra_allowed.is_empty() {
        sandbox::default_extra_paths()
    } else {
        config.extra_allowed.iter().map(PathBuf::from).collect()
    };
    sandbox::set_extra_allowed(extra)?;
    sandbox::set_sensitive_suffixes(&config.sensitive_suffixes)?;
    sandbox::set_sensitive_paths(&config.sensitive_paths)
}

/// 初始化权限:permissions 已随 Config 存入全局,各模块按需 config::get() 读取
/// bash_safety::classify 调 config_match 做规则匹配;会话级 grant 在 Toolbox
fn init_permissions(_config: &PermissionsConfig) -> anyhow::Result<()> {
    Ok(())
}

// === 权限规则匹配 ===
// 数据(PermissionsConfig)与操作(config_match)同处一地:配置定义、加载、查询匹配统一管理
// 会话级授权记忆(grant)在 Toolbox,不在此处

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
    let rules = match tool {
        "bash" => &config.bash,
        "write" => &config.write,
        "edit" => &config.edit,
        _ => return None,
    };
    if tool == "bash" {
        let subs = split_bash_subcommands(command);
        // 空命令(拆分后为空):按整串走单条匹配,兼容
        let targets = if subs.is_empty() {
            vec![command.to_string()]
        } else {
            subs
        };
        match_bash_rules(rules, &targets)
    } else {
        match_single(rules, command).map(|a| (a, Vec::new()))
    }
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
        match match_single(rules, sub) {
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
    fn write_edit_still_integral_match() {
        let cfg = bash_rules();
        assert!(config_match(&cfg, "write", "*.env").is_none());
    }
}
