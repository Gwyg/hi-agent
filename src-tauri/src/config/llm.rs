//! 大模型配置:model / api_key / effort 的解析与运行时持有。
//!
//! 本项目专做 DeepSeek:端点写死在 `ai::llm::deepseek.rs`,不再是可配置项,也没有
//! 环境变量覆盖。用户真正要填的只有 **API Key**;模型是从官方 `/models` 列表里
//! 选出来的一个字符串,落盘只为了「记住上次选了哪个」,不是手填的配置项。
//!
//! 来源优先级(文件 > env > 内置默认)**只对 api_key 成立**:
//! - `api_key`:app.toml `[llm].api_key` > 环境变量 `API_KEY` > 空;
//! - `model`:app.toml `[llm].model` > 内置兜底 `deepseek-flash`(列表拉不到或没选过时,
//!   请求体里总得有个字符串);
//! - `effort`:app.toml `[llm].effort` > 内置兜底 `high`(官方 `default_level`)。
//!   只认 `low` / `high` / `max` 三档;配置域存 String,不反向依赖 `ai::llm::types::Effort`。
//!
//! 文件值由 `init()` 在启动时从 app.toml 注入;运行中改配置走 `set_llm_config()`——它一次完成
//! 「写回文件 + 热更新内存」,故每次 LLM 调用读到的都是最新配置,无需重启。
//!
//! 文件形状与运行时状态为**同一结构体**:llm 配置是「磁盘 ↔ 内存」严格 1:1 的纯键值镜像,
//! 不区分 DTO 与领域模型(对照 path_scope:运行时还持有白名单/敏感清单等代码内策略,
//! 不在 app.toml 中,故 `PathScopeConfig` 只作磁盘形状,运行时状态另置)。
//!
//! 安全说明:api_key 以**密文**存于 `~/.hi-agent/app.toml`(见 `util::secret`:密钥由本机
//! 标识派生,文件被拷到别的机器上解不开);文件其余字段仍是明文,用户要手改工作目录 / 模型
//! 时不受影响。内存里恒为明文,前端 `get_llm_config` 只回脱敏视图,绝不回明文。

use serde::Deserialize;
use std::sync::RwLock;
use tracing::warn;

use crate::config::Value;
use crate::util::secret;

/// 兜底模型:官方 `/models` 拉不到、或用户从未选过时写进请求体的字符串。
/// `deepseek-v4-flash` 是已退役模型的临时别名(请求会被路由到 V4.1-Flash),
/// `deepseek-chat` / `deepseek-reasoner` 已过停用日期,故不再使用。
const DEFAULT_MODEL: &str = "deepseek-flash";

/// 思考强度默认档:官方 `/models` 的 `default_level` 也是 `high`。
/// 落在这里的是「展示用」的生效值;请求侧据此决定发不发 `reasoning_effort`,
/// 选 high 时不发键,让端点用它自己的默认值。
const DEFAULT_EFFORT: &str = "high";

// ============ 运行时全局单例 ============
// 配置值单一真源在 config 模块,工具/客户端只读取。

static LLM: RwLock<LlmConfig> = RwLock::new(LlmConfig {
    model: None,
    api_key: None,
    effort: None,
});

/// 大模型配置(model / api_key / effort)。
///
/// 既是 app.toml `[llm]` 段的序列化形状,也是运行时的单例状态——两者 1:1 对应。
/// 字段统一 `Option<String>` + `#[serde(default)]`,缺失字段按 None 处理(见 `AppConfig::load` 容错)。
#[derive(Clone, Debug, Default, Deserialize)]
pub struct LlmConfig {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
}

impl LlmConfig {
    /// 生效 model:文件值 > 内置兜底。不读环境变量,也不由用户手填。
    pub fn model(&self) -> String {
        self.model
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string())
    }

    /// 生效思考强度:`low` / `high` / `max`,缺键或脏值一律回落 `high`。
    /// 归一化放在这里,前端拿到的恒是三档之一,请求侧不必再判非法值。
    pub fn effort(&self) -> String {
        match self.effort.as_deref() {
            Some("low") => "low".to_string(),
            Some("max") => "max".to_string(),
            _ => DEFAULT_EFFORT.to_string(),
        }
    }

    /// 生效 api_key:已解密的文件值 > env(API_KEY) > 空。唯一还走三级优先级的字段。
    ///
    /// 返回的恒是**明文**:解密在 [`init`] 里完成,内存中的 `api_key` 字段不保存密文。
    pub fn api_key(&self) -> String {
        self.api_key
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("API_KEY").ok().filter(|s| !s.is_empty()))
            .unwrap_or_default()
    }

    /// 是否已配置:生效的 api_key 非空即为已配置(端点写死、model 有兜底)。
    pub fn is_configured(&self) -> bool {
        !self.api_key().is_empty()
    }
}

/// 启动恢复:由 `config::init` 加载 app.toml 后传入 `[llm]` 段,注入运行时全局。
///
/// `api_key` 在这里完成「磁盘形态 → 内存形态」的转换:文件里是 `enc:v1:` 密文,内存里恒为
/// 明文(`api_key()` 的调用方直接拿去拼请求头,不该关心存储形态)。
///
/// 解不开时(换了机器 / 重装系统 / 数据被改)按**未配置**处理:内存置空、记一条告警,
/// 让用户重新填一次。这是本方案的固有代价,绝不能让启动失败。
///
/// 读到明文(本功能上线前的老配置,或用户手改过)时照常使用,并立刻加密回写一次完成迁移;
/// 回写失败只告警 —— 内存里已有可用密钥,不该因为写盘问题让对话不可用。
pub fn init(section: &LlmConfig) {
    let mut cfg = section.clone();
    let stored = cfg
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    cfg.api_key = match stored {
        None => None,
        Some(raw) if secret::is_sealed(&raw) => match secret::open(&raw) {
            Some(plain) => Some(plain),
            None => {
                warn!("api_key 解密失败(本机标识已变化或密文损坏),按未配置处理,请重新填写");
                None
            }
        },
        Some(plain) => {
            migrate_plain_key(&plain);
            Some(plain)
        }
    };

    if let Ok(mut g) = LLM.write() {
        *g = cfg;
    }
}

/// 明文密钥的一次性迁移:加密后**只改 `api_key` 一个键**写回,其余键一字节不动。
///
/// 不调 `set_llm_config`:那条路会重写 model / effort,而且会拿到「已经是密文」的入参,
/// 逻辑上绕一圈只为写一个键。迁移是低频动作(每个用户一次),单键写回最简单也最安全。
fn migrate_plain_key(plain: &str) {
    let sealed = match secret::seal(plain) {
        Ok(s) => s,
        Err(e) => {
            warn!("api_key 加密失败,本次仍以明文保留: {e}");
            return;
        }
    };
    if let Err(e) = crate::config::persist(&["llm"], &[("api_key", Some(Value::from(sealed)))]) {
        warn!("api_key 密文回写失败(下次启动会再试一次): {e}");
    }
}

/// 保存配置:先写回 `[llm]` 段,成功后再热更新运行时。
///
/// 界面一次性保存整段,故三个有效键一起写(分次写既慢又留半写状态)。`None` 表示删键:
/// `model` 清空时删键回落到内置兜底,不写一条没信息的 `model = ""`;
/// `effort` 缺省时由命令层沿用当前生效值,不会把用户选过的档位抹掉;
/// `base_url` 是**已废弃的旧键**,显式传 `None` 清掉老 app.toml 里的残留——
/// `persist` 是「未声明节点一字节不动」,不声明它就会一直留在文件里误导人。
/// 值取自本结构体字段,读与写是同一个类型的投影,不会「写字符串、读数字」。
///
/// `api_key` 是唯一「写入形态 ≠ 内存形态」的字段:落盘前加密成 `enc:v1:` 密文,
/// 内存里留明文。加密失败直接返回错误 —— 写不出去就不该更新内存,否则用户以为存好了、
/// 重启后却是未配置。
///
/// 顺序不可交换:落盘失败直接返回,不留下「内存说已配置、文件里却没有」的状态——重启后用户会发现
/// 配置白填。与工作区切换相反(`services::workspace::switch` 那边目录已在界面生效,
/// 断点写不进去只告警),因为「配置」和「当前停在哪」失效的后果不是一回事。
pub fn set_llm_config(cfg: LlmConfig) -> std::io::Result<()> {
    // 落盘前加密:文件里永远只有密文,内存里留给调用方的是明文。
    // 传入值若已是密文(不该发生:命令层收到的是用户输入的明文)就原样写,
    // 否则二次加密后连自己也解不开。
    let stored_key = match cfg.api_key.as_deref().filter(|s| !s.is_empty()) {
        None => None,
        Some(v) if secret::is_sealed(v) => Some(v.to_string()),
        Some(plain) => Some(secret::seal(plain).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, format!("API Key 加密失败: {e}"))
        })?),
    };

    crate::config::persist(
        &["llm"],
        &[
            ("model", cfg.model.as_deref().map(Value::from)),
            ("api_key", stored_key.as_deref().map(Value::from)),
            ("effort", cfg.effort.as_deref().map(Value::from)),
            ("base_url", None),
        ],
    )?;
    if let Ok(mut g) = LLM.write() {
        *g = cfg;
    }
    Ok(())
}

/// 读取当前运行时配置(原始文件值层;生效值由各访问器叠加 env / 内置兜底算出)。
pub fn llm_config() -> LlmConfig {
    LLM.read().map(|c| c.clone()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `effort()` 是手写归一化:缺键 / 脏值 / 官方别名一律落回 `high`。
    /// 放行别名会造出「看着切了档、实际被静默折成 high」的假档位。
    #[test]
    fn effort_normalizes_to_three_official_levels() {
        let cfg = |v: Option<&str>| LlmConfig {
            effort: v.map(str::to_string),
            ..Default::default()
        };
        assert_eq!(cfg(None).effort(), "high");
        assert_eq!(cfg(Some("low")).effort(), "low");
        assert_eq!(cfg(Some("high")).effort(), "high");
        assert_eq!(cfg(Some("max")).effort(), "max");
        assert_eq!(cfg(Some("medium")).effort(), "high");
        assert_eq!(cfg(Some("")).effort(), "high");
    }
}
