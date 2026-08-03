//! 大模型配置:model / base_url / api_key 分层解析
//! 来源优先级(高→低):环境变量 > 项目级 config.toml > 用户级 config.toml > 内置默认
//! 注意:api_key 可落 toml,但项目级 .hi-agent/config.toml 若含密钥务必加入 .gitignore

use serde::Deserialize;

/// 默认 base_url:DeepSeek OpenAI 兼容端点
const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";
/// 默认模型
const DEFAULT_MODEL: &str = "deepseek-v4-pro";

/// 大模型配置(model / base_url / api_key)
///
/// toml 反序列化字段均 optional;文件层合并(项目 > 用户)由 config::deep_merge 完成,
/// 本结构的解析方法再在其上叠加环境变量(最高优先级),缺失落内置默认
#[derive(Deserialize, Clone, Default)]
pub struct LlmConfig {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
}

impl LlmConfig {
    /// 解析最终 model:env(MODEL)> 文件 > 默认
    pub fn model(&self) -> String {
        env_or(&self.model, "MODEL").unwrap_or_else(|| DEFAULT_MODEL.to_string())
    }

    /// 解析最终 base_url:env(BASE_URL)> 文件 > 默认
    pub fn base_url(&self) -> String {
        env_or(&self.base_url, "BASE_URL").unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
    }

    /// 解析最终 api_key:env(API_KEY)> 文件 > 空
    pub fn api_key(&self) -> String {
        env_or(&self.api_key, "API_KEY").unwrap_or_default()
    }
}

/// 字段解析:非空环境变量 > 文件值;都缺失返 None
fn env_or(file_val: &Option<String>, env_key: &str) -> Option<String> {
    std::env::var(env_key)
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| file_val.clone())
}
