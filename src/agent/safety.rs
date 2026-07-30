/// 安全裁决:工具调用检查结果
pub enum SafetyVerdict {
    /// 允许执行
    Allow,
    /// 拒绝执行(危险操作)
    Deny(String),
    /// 需用户确认
    AskUser(String),
}

/// 安全检查器:工具调用前拦截危险操作(如 bash 的 rm -rf、写敏感文件)
pub struct SafetyChecker;

impl SafetyChecker {
    pub fn new() -> Self {
        Self
    }

    /// 检查工具调用是否安全
    // TODO: 危险命令判断(bash 黑名单/正则)、敏感路径拦截、写操作需确认
    pub fn check(&self, _tool_name: &str, _args: &str) -> SafetyVerdict {
        SafetyVerdict::Allow
    }
}

impl Default for SafetyChecker {
    fn default() -> Self {
        Self::new()
    }
}
