//! 沙箱配置:默认常量集中于此 + 结构定义 + 初始化(把最终清单灌入 tools::sandbox 运行时)
//!
//! 常量归位原则:所有沙箱默认值(白名单/敏感清单)在本文件集中管理;
//! 逻辑层 tools::sandbox 只负责匹配判定与归一化,不再持有默认常量。
//!
//! 安全语义(不变):敏感清单为"内置基线 + 文件追加",用户只能加不能减(收紧可,放宽不可)。

use serde::Deserialize;
use std::path::PathBuf;

use crate::llm::tools::sandbox;

/// 默认额外白名单前缀:恒在(不受文件配置影响,文件只能在其上追加)
/// - 临时目录:工具常用临时读写
/// - ~/.hi-agent:agent 自身用户级目录(config/log/会话等),理应可访问
const DEFAULT_EXTRA_ALLOWED: &[&str] = &["~/.hi-agent"];

/// 内置敏感后缀基线(ends_with 匹配,小写)。文件 sensitive_suffixes 在其后追加
const DEFAULT_SENSITIVE_SUFFIXES: &[&str] = &[
    ".env",          // 环境变量(API keys, DB URLs)
    ".envrc",        // direnv
    ".pem",          // 证书/私钥
    ".key",          // 私钥
    ".p12",          // 证书
    ".pfx",          // 证书
    ".npmrc",        // npm token
    ".pypirc",       // PyPI token
    ".netrc",        // HTTP 凭证
    ".bash_history", // shell 历史
    ".zsh_history",  // shell 历史
    "id_rsa",        // SSH 私钥(ends_with,不匹配 id_rsa.pub)
    "id_ed25519",    // SSH 私钥(同上)
    "secrets.yml",   // 通用密钥文件
    "secrets.yaml",
    "secrets.json",
];

/// 内置敏感路径前缀基线(starts_with 匹配,含 ~ 由逻辑层展开)。文件 sensitive_paths 在其后追加
/// 只匹配家目录下的凭证目录,不影响项目内或其他位置的同名目录
const DEFAULT_SENSITIVE_PATHS: &[&str] = &[
    "~/.ssh",           // SSH 密钥
    "~/.aws",           // AWS 凭证
    "~/.kube",          // K8s 配置
    "~/.gnupg",         // GPG 密钥
    "~/.config/gcloud", // GCP 凭证
];

/// 沙箱配置(文件层:均为在默认基线之上追加的额外项)
#[derive(Deserialize, Clone, Default)]
pub struct SandboxConfig {
    /// 可写/改目录白名单:约束 write/edit 工具(项目根默认可写,不在此配置)
    /// 追加到默认基线(temp + ~/.hi-agent)之上;路径字符串,逻辑层展开 ~ + canonicalize
    #[serde(default)]
    pub write_allowed_paths: Vec<String>,
    /// 读确认后缀:read 工具读取这些后缀文件前需用户确认(ends_with 匹配)
    /// 追加到内置基线(.env/.pem/id_rsa…)之上,只能加不能减
    #[serde(default)]
    pub read_confirm_suffixes: Vec<String>,
    /// 读确认路径前缀:read 工具读取这些路径前需用户确认(starts_with,逻辑层展开 ~)
    /// 追加到内置基线(~/.ssh/~/.aws…)之上,只能加不能减
    #[serde(default)]
    pub read_confirm_paths: Vec<String>,
}

impl SandboxConfig {
    /// 最终可写白名单 = 默认基线(temp + ~/.hi-agent) + 文件追加(均恒在,不二选一)
    fn resolved_write_allowed(&self) -> Vec<PathBuf> {
        let mut paths = vec![std::env::temp_dir()];
        for p in DEFAULT_EXTRA_ALLOWED {
            paths.push(PathBuf::from(*p)); // 含 ~,由逻辑层 set_extra_allowed 展开
        }
        paths.extend(self.write_allowed_paths.iter().map(PathBuf::from));
        paths
    }

    /// 最终读确认后缀 = 内置基线 + 文件追加(内置在前,只加不减)
    fn resolved_read_confirm_suffixes(&self) -> Vec<String> {
        let mut all: Vec<String> = DEFAULT_SENSITIVE_SUFFIXES
            .iter()
            .map(|s| s.to_string())
            .collect();
        all.extend(self.read_confirm_suffixes.iter().cloned());
        all
    }

    /// 最终读确认路径前缀 = 内置基线 + 文件追加(内置在前,只加不减;~ 由逻辑层展开)
    fn resolved_read_confirm_paths(&self) -> Vec<String> {
        let mut all: Vec<String> = DEFAULT_SENSITIVE_PATHS
            .iter()
            .map(|s| s.to_string())
            .collect();
        all.extend(self.read_confirm_paths.iter().cloned());
        all
    }
}

/// 初始化沙箱:在本层算好最终清单(默认基线 + 文件追加),灌入逻辑层运行时
/// 逻辑层只负责归一化(normalize/展开 ~)与匹配判定,不持默认常量
pub(super) fn init_sandbox(config: &SandboxConfig) -> anyhow::Result<()> {
    sandbox::set_extra_allowed(config.resolved_write_allowed())?;
    sandbox::set_sensitive_suffixes(&config.resolved_read_confirm_suffixes())?;
    sandbox::set_sensitive_paths(&config.resolved_read_confirm_paths())
}
