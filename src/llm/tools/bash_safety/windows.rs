//! Windows(cmd)命令安全分类(字符串方案)
//!
//! 策略(对齐 unix,适配无 OS 沙箱):
//! 0. 致命命令(删系统根/格式化/diskpart)→ Deny(不可覆盖)
//! 1. 危险语法(重定向/命令替换)→ Ask{false}
//! 2. 发现危险(DANGER 命令/cmd/powershell 包装器内部递归)→ Ask{false}
//! 3. 分段全段 safe → Allow
//! 4. 未知 → Ask{false}
//!
//! 平台特性(与 unix 的关键差异):
//! - `&` 是顺序连命令(分隔符,分段处理),不是危险语法
//! - 命令大小写不敏感(首词 to_lowercase 查表)
//! - cmd 无 AST,用字符串方案(cmd 动态面小,%VAR% 展开判不准但常见,不过度拦)
//! - cmd /C "..." 和 powershell -Command "..." 是包装器,递归检测内部

use super::super::Action;
use crate::config::{self, PermissionAction, SimpleAction};

/// 递归深度上限(防 cmd /C "cmd /C ..." 无限递归)
const MAX_WRAPPER_DEPTH: usize = 8;

/// 危险语法(命中即 Ask{false},不进分段)
/// `&` 不在此列(是连命令分隔符,由 split_segments 处理)
const DANGER_SYNTAX: &[&str] = &[
    ">",  // 重定向写(含 >>)
    "<",  // 重定向读
    "$(", // PowerShell 命令替换
];

/// 安全命令白名单(首词匹配,Allow,大小写不敏感)
/// 纯读/显示命令。cd 不放(影响后续 cwd)
const SAFE_COMMANDS: &[&str] = &[
    "dir", "type", "findstr", "echo", "where", "tree", "ver", "vol", "find", "sort", "more",
    "clip", "chcp", "hostname", "whoami",
];

/// 危险命令(首词匹配,Ask{false},大小写不敏感)
/// 删除/复制/移动/网络/注册表/进程/提权/PowerShell(能执行任意脚本)
const DANGER: &[&str] = &[
    "del",
    "erase",
    "rmdir",
    "rd",
    "copy",
    "xcopy",
    "robocopy",
    "move",
    "ren",
    "rename",
    "md",
    "mkdir",
    "curl",
    "wget",
    "shutdown",
    "taskkill",
    "reg",
    "runas",
    "net",
    "sc",
    "wmic",
    "bcdedit",
    "takeown",
    "icacls",
    "attrib",
    "powershell",
    "pwsh",
];

/// PowerShell 致命关键字(powershell -Command 内部扫描,大小写不敏感)
/// 仅放「几乎无合法用途」的破坏性 cmdlet(无法解析 PS AST,保守字符串扫描)
const PS_FATAL_KEYWORDS: &[&str] = &[
    "format-volume",
    "clear-disk",
    "stop-computer",
    "restart-computer",
];

// ============ 命令分类入口 ============

/// 命令分类入口
///
/// 0. 致命命令(删根/格式化/diskpart)→ Deny(不可覆盖)
/// 1. 配置规则匹配(config.toml [permissions.bash]):deny/allow/ask
/// 2. 危险语法(重定向/命令替换)→ Ask{false}
/// 3. 发现危险(DANGER 命令/包装器内部递归)→ Ask{false}
/// 4. 分段全段 safe → Allow
/// 5. 未知 → Ask{false}(无 OS 沙箱兜底)
pub fn classify(cmd: &str) -> Action {
    // 0. 致命
    if let Some(reason) = find_fatal(cmd, 0) {
        return Action::Deny(reason);
    }
    // 1. 配置规则:致命之后,安全判断之前(用户策略覆盖默认安全逻辑)
    if let Some(action) = config_match_bash(cmd) {
        return action;
    }
    // 2. 危险语法
    if has_danger_syntax(cmd) {
        return Action::Ask {
            persistable: false,
            keys: Vec::new(),
        };
    }
    // 3. 发现危险
    if find_dangerous(cmd, 0) {
        return Action::Ask {
            persistable: false,
            keys: Vec::new(),
        };
    }
    // 4. 分段全段 safe → Allow
    for seg in split_segments(cmd) {
        let argv = split_argv(seg);
        if !is_safe_command(&argv) {
            return Action::Ask {
                persistable: false,
                keys: Vec::new(),
            };
        }
    }
    Action::Allow
}

/// 读全局配置,对 bash 命令做规则匹配
/// config 未初始化(测试环境)或无规则 → None,走默认安全逻辑
/// 返回 Ask{ask=true} 时附 keys(触发 ask=true 的子命令,加 "bash:" 前缀),
/// 供 agent_loop 做 grant_check/grant_record;ask=false/allow/deny 时 keys 空
fn config_match_bash(cmd: &str) -> Option<Action> {
    let config = config::get().ok()?;
    let (action, trigger_subs) = crate::config::config_match(&config.permissions, "bash", cmd)?;
    Some(match action {
        PermissionAction::Simple(SimpleAction::Deny) => Action::Deny("配置规则拒绝".into()),
        PermissionAction::Simple(SimpleAction::Allow) => Action::Allow,
        // ask=true:询问一次,可永久授权(persistable=true),keys = 触发子命令
        // ask=false:每次必问(persistable=false),keys 空(不进 grant)
        PermissionAction::Ask { ask } => Action::Ask {
            persistable: ask,
            keys: if ask {
                trigger_subs
                    .into_iter()
                    .map(|s| format!("bash:{s}"))
                    .collect()
            } else {
                Vec::new()
            },
        },
    })
}

// ============ 致命检测(不可覆盖 Deny) ============

/// 递归发现致命命令(返回原因;命中即 Deny)
/// 递归 cmd /C "..." 脚本 + powershell -Command "..." 内部,深度上限 MAX_WRAPPER_DEPTH
fn find_fatal(src: &str, depth: usize) -> Option<String> {
    if depth > MAX_WRAPPER_DEPTH {
        return None;
    }
    for seg in split_segments(src) {
        let argv = split_argv(seg);
        if let Some(reason) = command_is_fatal(seg, &argv, depth) {
            return Some(reason);
        }
    }
    None
}

/// 单命令是否致命(含递归解包 cmd/powershell)
/// 仅放「几乎无合法用途」的极端模式;误判会卡死合法操作,务必保守
fn command_is_fatal(seg: &str, argv: &[String], depth: usize) -> Option<String> {
    let first = argv.first()?.to_lowercase();
    match first.as_str() {
        // cmd /C "..." 或 /K:递归解析脚本(cmd 语法)
        "cmd" => {
            if let Some(script) = wrapper_script_after_flag(seg, &["/c", "/k"]) {
                if let Some(r) = find_fatal(&script, depth + 1) {
                    return Some(r);
                }
            }
            None
        }
        // powershell -Command "...":扫描 PS 致命关键字(无法解析 PS AST,字符串扫描)
        "powershell" | "pwsh" => {
            if let Some(script) =
                wrapper_script_after_flag(seg, &["-command", "-c", "-encodedcommand"])
            {
                if let Some(r) = find_fatal_ps(&script) {
                    return Some(r);
                }
            }
            None
        }
        // rd/rmdir /s 删系统根:精确匹配目标(避免误杀 rd /s .\build)
        "rd" | "rmdir" => {
            let has_s = argv.iter().skip(1).any(|a| a.eq_ignore_ascii_case("/s"));
            let target_system = argv.iter().skip(1).any(|a| {
                let a = a.to_lowercase();
                matches!(
                    a.as_str(),
                    "c:\\" | "c:/*" | "%systemroot%" | "%userprofile%" | "%windir%"
                ) || a.starts_with("c:\\windows")
                    || a.starts_with("c:/windows")
            });
            if has_s && target_system {
                Some("rd 递归删系统目录".into())
            } else {
                None
            }
        }
        // del/erase /s 删系统文件
        "del" | "erase" => {
            let has_s = argv.iter().skip(1).any(|a| a.eq_ignore_ascii_case("/s"));
            let target_system = argv.iter().skip(1).any(|a| {
                let a = a.to_lowercase();
                matches!(
                    a.as_str(),
                    "c:\\*" | "c:/*" | "c:\\windows\\*" | "%systemroot%\\*" | "%windir%\\*"
                )
            });
            if has_s && target_system {
                Some("del 递归删系统文件".into())
            } else {
                None
            }
        }
        // format:格式化磁盘,本地开发几乎不会用
        "format" => Some("格式化磁盘".into()),
        // diskpart:能 clean 清空磁盘,破坏性大
        "diskpart" => Some("磁盘分区操作".into()),
        _ => None,
    }
}

/// PowerShell 脚本致命关键字扫描(字符串 contains,大小写不敏感)
/// 无法解析 PS AST,保守扫描破坏性 cmdlet
fn find_fatal_ps(script: &str) -> Option<String> {
    let s = script.to_lowercase();
    for kw in PS_FATAL_KEYWORDS {
        if s.contains(kw) {
            return Some(format!("PowerShell {kw}"));
        }
    }
    None
}

// ============ 危险发现 ============

/// 递归发现危险命令(DANGER 首词 + cmd 包装器内部递归)
fn find_dangerous(src: &str, depth: usize) -> bool {
    if depth > MAX_WRAPPER_DEPTH {
        return false;
    }
    for seg in split_segments(src) {
        let argv = split_argv(seg);
        if command_is_dangerous(seg, &argv, depth) {
            return true;
        }
    }
    false
}

/// 单命令是否危险(含递归解包 cmd)
fn command_is_dangerous(seg: &str, argv: &[String], depth: usize) -> bool {
    let first = match argv.first() {
        Some(f) => f.to_lowercase(),
        None => return false,
    };
    match first.as_str() {
        // cmd /C "..." 递归检测内部
        "cmd" => {
            if let Some(script) = wrapper_script_after_flag(seg, &["/c", "/k"]) {
                if find_dangerous(&script, depth + 1) {
                    return true;
                }
            }
            false
        }
        // powershell 在 DANGER(能执行任意脚本)
        "powershell" | "pwsh" => true,
        // 其余:查 DANGER 黑名单
        _ => DANGER.contains(&first.as_str()),
    }
}

// ============ 语法/分段/argv ============

/// 危险语法检测(不含 &,& 是分隔符)
fn has_danger_syntax(cmd: &str) -> bool {
    DANGER_SYNTAX.iter().any(|s| cmd.contains(s))
}

/// 分段:按 & && ; || | 切
/// Windows cmd 用 & && || 连命令;PowerShell 用 ; 和 |
fn split_segments(cmd: &str) -> Vec<&str> {
    let mut segs: Vec<&str> = vec![cmd];
    for sep in &["&&", "||", "&", ";", "|"] {
        let mut next = Vec::new();
        for s in &segs {
            for part in s.split(sep) {
                let t = part.trim();
                if !t.is_empty() {
                    next.push(t);
                }
            }
        }
        segs = next;
    }
    segs
}

/// argv 切分(引号感知,去外层双引号)
/// `"C:\Program Files"` 作为一个参数
fn split_argv(seg: &str) -> Vec<String> {
    let mut argv = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    for c in seg.chars() {
        match c {
            '"' => in_quote = !in_quote,
            c if c.is_whitespace() && !in_quote => {
                if !cur.is_empty() {
                    argv.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        argv.push(cur);
    }
    argv
}

/// 单命令是否安全(首词在白名单)
fn is_safe_command(argv: &[String]) -> bool {
    let Some(first) = argv.first() else {
        return false;
    };
    SAFE_COMMANDS.contains(&first.to_lowercase().as_str())
}

/// 提取包装器 flag 后的脚本(/C/-Command 等)
/// `cmd /C "rd /s /q C:\"` → `rd /s /q C:\`
/// `powershell -Command "Format-Volume"` → `Format-Volume`
fn wrapper_script_after_flag(seg: &str, flags: &[&str]) -> Option<String> {
    let tokens: Vec<&str> = seg.split_whitespace().collect();
    let lower_tokens: Vec<String> = tokens.iter().map(|t| t.to_lowercase()).collect();
    for (i, t) in lower_tokens.iter().enumerate() {
        if flags.contains(&t.as_str()) {
            let rest: String = tokens[i + 1..].join(" ");
            if rest.is_empty() {
                return None;
            }
            let rest = rest.trim();
            // 去外层引号
            if rest.len() >= 2 && rest.starts_with('"') && rest.ends_with('"') {
                return Some(rest[1..rest.len() - 1].to_string());
            }
            return Some(rest.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- 原有用例回归 ---

    #[test]
    fn readonly_allow() {
        assert!(matches!(classify("dir"), Action::Allow));
        assert!(matches!(classify("type foo.txt"), Action::Allow));
        assert!(matches!(classify("findstr pattern *.rs"), Action::Allow));
        assert!(matches!(classify("where python"), Action::Allow));
        assert!(matches!(classify("whoami"), Action::Allow));
    }

    #[test]
    fn danger_ask() {
        assert!(matches!(
            classify("del foo.txt"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
        assert!(matches!(
            classify("rmdir /s /q foo"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
        assert!(matches!(
            classify("taskkill /f /im x.exe"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
        assert!(matches!(
            classify("reg query HKLM\\Software"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
    }

    #[test]
    fn danger_syntax_ask() {
        assert!(matches!(
            classify("echo hi > file"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
        assert!(matches!(
            classify("powershell $(whoami)"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
    }

    #[test]
    fn ampersand_is_separator_not_danger() {
        // & 是连命令分隔符:dir Allow + del Ask{false} → 整体 Ask{false}
        assert!(matches!(
            classify("dir & del x"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
        // 两段都 Allow → Allow
        assert!(matches!(classify("dir & echo hi"), Action::Allow));
        // 管道:两段都 safe → Allow
        assert!(matches!(classify("dir | findstr foo"), Action::Allow));
    }

    #[test]
    fn case_insensitive() {
        assert!(matches!(classify("DIR"), Action::Allow));
        assert!(matches!(
            classify("Del foo"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
        assert!(matches!(classify("ECHO hi"), Action::Allow));
    }

    #[test]
    fn unknown_ask() {
        assert!(matches!(
            classify("cargo build"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
        assert!(matches!(
            classify("msbuild"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
    }

    // --- 致命命令(Deny 不可覆盖) ---

    #[test]
    fn fatal_rd_root_deny() {
        assert!(matches!(classify("rd /s /q C:\\"), Action::Deny(_)));
        assert!(matches!(classify("rmdir /S /Q C:\\"), Action::Deny(_)));
        assert!(matches!(classify("rd /s /q %SystemRoot%"), Action::Deny(_)));
        assert!(matches!(
            classify("rd /s /q %USERPROFILE%"),
            Action::Deny(_)
        ));
        // 非系统目录:不命中致命,仍 Ask{false}(rd 在 DANGER)
        assert!(matches!(
            classify("rd /s /q .\\build"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
        assert!(matches!(
            classify("rd /s /q D:\\tmp"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
    }

    #[test]
    fn fatal_format_deny() {
        assert!(matches!(classify("format D:"), Action::Deny(_)));
        assert!(matches!(classify("format C:"), Action::Deny(_)));
    }

    #[test]
    fn fatal_diskpart_deny() {
        assert!(matches!(classify("diskpart"), Action::Deny(_)));
        assert!(matches!(
            classify("diskpart /s script.txt"),
            Action::Deny(_)
        ));
    }

    #[test]
    fn fatal_del_system_deny() {
        assert!(matches!(classify("del /f /s /q C:\\*"), Action::Deny(_)));
        assert!(matches!(
            classify("del /s /q C:\\Windows\\*"),
            Action::Deny(_)
        ));
        // 非系统目录:不命中致命,仍 Ask{false}(del 在 DANGER)
        assert!(matches!(
            classify("del /f /s /q .\\build\\*"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
    }

    #[test]
    fn fatal_cmd_c_recursion() {
        // cmd /C "..." 内部藏致命命令 → 递归发现 Deny
        assert!(matches!(
            classify("cmd /C \"rd /s /q C:\\\""),
            Action::Deny(_)
        ));
        assert!(matches!(classify("cmd /c \"format C:\""), Action::Deny(_)));
    }

    #[test]
    fn fatal_powershell_recursion() {
        // powershell -Command "..." 内部含 PS 致命关键字 → Deny
        assert!(matches!(
            classify("powershell -Command \"Format-Volume\""),
            Action::Deny(_)
        ));
        assert!(matches!(
            classify("powershell -Command \"Stop-Computer\""),
            Action::Deny(_)
        ));
    }

    #[test]
    fn cmd_c_danger_recursion() {
        // cmd /C "del x" → 递归发现 del(DANGER)→ Ask{false}
        assert!(matches!(
            classify("cmd /C \"del x\""),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
        // powershell -Command "Get-Process" → powershell 在 DANGER → Ask{false}(非致命)
        assert!(matches!(
            classify("powershell -Command \"Get-Process\""),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
    }
}
