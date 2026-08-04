//! 配置总管(facade):聚合各子模块配置,负责加载/合并/单例/编排
//! 各子模块(sandbox/permissions)自持结构与逻辑;此处 re-export 保持对外路径稳定
//!
//! 对外 API(重构后路径不变):
//! - config::init() / config::get()
//! - config::config_match() / PermissionsConfig / PermissionAction / SimpleAction
//! - config::SandboxConfig

mod llm;
mod log;
mod permissions;
mod sandbox;

pub use llm::LlmConfig;
pub use log::LogConfig;
pub use permissions::{PermissionAction, PermissionsConfig, SimpleAction, config_match};
pub use sandbox::SandboxConfig;

use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use crate::llm::tools::sandbox as tools_sandbox;

/// 全局配置(启动时加载一次,会话内不变)
#[derive(Deserialize, Clone, Default)]
pub struct Config {
    #[serde(default)]
    pub llm: LlmConfig,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub permissions: PermissionsConfig,
    #[serde(default)]
    pub log: LogConfig,
}

static CONFIG: RwLock<Option<Config>> = RwLock::new(None);

/// 启动时调一次:加载配置文件,依次初始化各子系统,最后存入全局
/// 配置文件不存在时用默认值,不报错
pub fn init() -> anyhow::Result<()> {
    let config = load_config()?;
    sandbox::init_sandbox(&config.sandbox)?;
    permissions::init_permissions(&config.permissions)?;
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
    let user = load_value(user_config_path()?.as_path())?;
    let project = load_value(project_config_path()?.as_path())?;
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
    Ok(tools_sandbox::project_root()?
        .join(".hi-agent")
        .join("config.toml"))
}
