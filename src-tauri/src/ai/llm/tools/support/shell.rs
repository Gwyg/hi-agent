//! 「用哪个 shell」的唯一真源:执行侧(bash.rs)与安全解析侧(bash_safety/powershell.rs)
//! 必须用同一个 shell —— 否则判定与执行对的是两套语法模型,分类器就成了摆设。
//!
//! 启动形态是跨平台不变的约定:可执行文件 + 固定前置参数,脚本作为**最后一个参数**追加。
//! 脚本字符串内部怎么被解析由各平台 shell 自己决定(unix `-c`、Windows `-Command`),
//! 那个决定只影响引号/换行语义,不影响这里的形状。

/// shell 的启动规格
pub struct ShellSpec {
    pub exe: &'static str,
    pub flags: &'static [&'static str],
}

#[cfg(unix)]
pub fn current() -> &'static ShellSpec {
    static S: ShellSpec = ShellSpec {
        exe: "sh",
        flags: &["-c"],
    };
    &S
}

#[cfg(windows)]
pub fn current() -> &'static ShellSpec {
    static S: std::sync::OnceLock<ShellSpec> = std::sync::OnceLock::new();
    S.get_or_init(|| {
        let exe = find_pwsh().unwrap_or("powershell");
        tracing::info!(shell = exe, "shell 选定(PowerShell)");
        ShellSpec {
            exe,
            flags: &["-NoProfile", "-NonInteractive", "-Command"],
        }
    })
}

/// PowerShell 7(pwsh)优先:5.1 没有 `&&`、解析器也更旧。
/// 找不到就回退系统自带的 Windows PowerShell(Windows 上必然存在)。
#[cfg(windows)]
fn find_pwsh() -> Option<&'static str> {
    // PATH 里命中就用裸名,由 CreateProcess 自己解析,避免拼绝对路径带来的空格/引号问题
    if let Some(path) = std::env::var_os("PATH") {
        if std::env::split_paths(&path).any(|d| d.join("pwsh.exe").is_file()) {
            return Some("pwsh");
        }
    }
    // 装了但没进 PATH 的常见位置(与 Codex 的兜底路径一致)
    const FALLBACK: &str = r"C:\Program Files\PowerShell\7\pwsh.exe";
    std::path::Path::new(FALLBACK).is_file().then_some(FALLBACK)
}
