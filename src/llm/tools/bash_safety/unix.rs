//! unix(sh/bash)命令安全分类(基于 tree-sitter-bash AST 解析)
//!
//! 策略(借鉴 Codex,适配无 OS 沙箱现状):
//! 1. 严格解析证明安全:整段只含白名单构造 + 全段 safe 命令 → Allow
//! 2. 宽松解析发现危险:递归收集字面命令,查危险清单/危险包装 → Ask{false}
//! 3. 未知:保守 Ask{false}(无 OS 进程沙箱兜底,未知必须问)
//!
//! 平台特性:
//! - `&` 是后台执行(非白名单节点 → 严格解析失败 → 非 plain)
//! - 命令大小写敏感(sh 命令名约定小写)
//!
//! 与 windows.rs 的差异:unix 用 AST 解析(bash 语法统一,可树化);
//! windows 的 cmd/PowerShell 无可用 grammar,仍用字符串方案

use super::super::Action;
use crate::config::{self, PermissionAction, SimpleAction};
use tree_sitter::{Node, Parser, Tree};

/// 递归深度上限(仿 Codex,防 `bash -c 'bash -c ...'` 无限递归)
const MAX_WRAPPER_DEPTH: usize = 8;

/// 白名单节点类型(严格解析:只允许这些构造,出现其他 → 非 plain)
/// 参考 Codex tree-sitter-bash ALLOWED_KINDS
const ALLOWED_KINDS: &[&str] = &[
    "program",        // 根
    "list",           // 命令序列 `a; b` `a && b`
    "pipeline",       // `a | b`
    "command",        // 单条命令
    "command_name",   // 命令名
    "word",           // 字面词
    "string",         // 双引号串(含 string_content)
    "string_content", // 串内文本
    "raw_string",     // 单引号串(无插值)
    "number",         // 数字
    "concatenation",  // 词拼接 `"a""b"`
];

/// 白名单标点(严格解析:只允许这些操作符,出现其他 → 非 plain)
/// 拒绝:重定向 `>` `<`、命令替换 `$()`、反引号、后台 `&`、括号、控制流等
const ALLOWED_PUNCT: &[&str] = &["&&", "||", ";", "|", "\"", "'"];

/// 安全命令白名单(首词精确匹配,Allow)
/// 合并 Codex 白名单 + 本项目原有清单;纯读/显示命令,无副作用
const SAFE_COMMANDS: &[&str] = &[
    "ls", "cat", "grep", "pwd", "echo", "head", "tail", "wc", "which", "whoami", "date", "uname",
    "stat", "file", "cd", "cut", "expr", "false", "true", "id", "nl", "paste", "rev", "seq", "tr",
    "uniq",
];

/// 危险命令(首词匹配,Ask{false})
/// 有副作用且风险高:删除/网络/权限/进程/提权
/// 注:sudo 在此清单 → sudo 一律 Ask;env 不在此(env 本身不危险,需解包看内部)
const DANGER: &[&str] = &[
    "rm", "rmdir", "dd", "mkfs", "curl", "wget", "chmod", "chown", "kill", "killall", "pkill",
    "shutdown", "reboot", "halt", "poweroff", "sudo", "su", "shred",
];

/// git 不安全全局选项(出现即非 safe:可逃逸工作目录/改配置)
/// 参考 Codex UNSAFE_GIT_GLOBAL_OPTIONS
const UNSAFE_GIT_OPTS: &[&str] = &[
    "-C",
    "-c",
    "--config-env",
    "--exec-path",
    "--git-dir",
    "--namespace",
    "--super-prefix",
    "--work-tree",
    "-p",
    "--paginate",
];

/// git 安全子命令(仅这些只读子命令算 safe)
const SAFE_GIT_SUBCMDS: &[&str] = &["status", "log", "diff", "show", "branch"];

/// find 不安全选项(出现即非 safe:可执行任意命令/删除/写文件)
const UNSAFE_FIND_OPTS: &[&str] = &[
    "-exec", "-execdir", "-ok", "-okdir", "-delete", "-fls", "-fprint", "-fprint0", "-fprintf",
];

/// base64 不安全选项(写文件)
const UNSAFE_BASE64_OPTS: &[&str] = &["-o", "--output"];

// 线程局部解析器(Parser 非线程安全,thread_local 避免重复初始化)
thread_local! {
    static PARSER: std::cell::RefCell<Parser> = {
        let mut p = Parser::new();
        p.set_language(&tree_sitter_bash::LANGUAGE.into())
            .expect("加载 tree-sitter-bash grammar 失败");
        std::cell::RefCell::new(p)
    };
}

/// 命令分类入口
///
/// 0. 致命命令(删根/格式化磁盘/dd 覆盖块设备)→ Deny(不可覆盖)
/// 1. 配置规则匹配(config.toml [permissions.bash]):deny/allow/ask
/// 2. 严格解析:若整段只含白名单构造且全段 safe 命令 → Allow
/// 3. 宽松解析:递归发现危险命令(rm/危险包装/bash -lc 递归) → Ask{false}
/// 4. 未知(含危险语法但无已知危险命令、未知命令) → Ask{false}
pub fn classify(cmd: &str) -> Action {
    // 0. 致命:不可覆盖,优先判定
    if let Some(reason) = find_fatal(cmd, 0) {
        return Action::Deny(reason);
    }
    // 1. 配置规则:致命之后,安全判断之前(用户策略覆盖默认安全逻辑)
    if let Some(action) = config_match_bash(cmd) {
        return action;
    }
    // 2. 严格证明安全
    if let Some(commands) = parse_plain_commands(cmd) {
        if commands.iter().all(|c| is_safe_command(c)) {
            return Action::Allow;
        }
    }
    // 3. 发现危险
    if find_dangerous(cmd, 0) {
        return Action::Ask {
            persistable: false,
            keys: Vec::new(),
        };
    }
    // 4. 未知:保守问(无 OS 沙箱兜底)
    Action::Ask {
        persistable: false,
        keys: Vec::new(),
    }
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

// ============ AST 解析 ============

/// tree-sitter 解析(复用 thread_local Parser)
fn parse(src: &str) -> Option<Tree> {
    PARSER.with(|cell| cell.borrow_mut().parse(src.as_bytes(), None))
}

/// 严格解析:整段只含白名单节点 + 白名单标点时,返回所有 command 的字面 argv
/// 任一非白名单构造(重定向/命令替换/反引号/后台&/括号/控制流/heredoc)→ 返回 None
/// 用于「证明安全」:必须严格,任何危险语法都不行
fn parse_plain_commands(src: &str) -> Option<Vec<Vec<String>>> {
    let tree = parse(src)?;
    let bytes = src.as_bytes();
    let mut commands = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if node.is_named() {
            // 命名节点:必须在白名单
            if !ALLOWED_KINDS.contains(&kind) {
                return None;
            }
            if kind == "command" {
                if let Some(argv) = extract_literal_argv(node, bytes) {
                    commands.push(argv);
                } else {
                    // 命令含动态词(${} 等):非纯 plain
                    return None;
                }
            }
        } else {
            // 匿名标点:`& ; |` 类必须白名单;其余(括号/重定向/反引号)拒
            if kind.chars().any(|c| "&;|".contains(c)) && !ALLOWED_PUNCT.contains(&kind) {
                return None;
            }
            if !(ALLOWED_PUNCT.contains(&kind) || kind.trim().is_empty()) {
                return None;
            }
        }
        // 压子节点(逆序保序)
        for i in (0..node.child_count()).rev() {
            if let Some(c) = node.child(i) {
                stack.push(c);
            }
        }
    }
    Some(commands)
}

/// 宽松解析:收集 AST 中所有 command 的字面 argv(允许任意语法构造)
/// 用于「发现危险」:深入 `bash -lc '...'` 字符串内部找 rm 等
/// 动态词(${} )被跳过(不安全,但也不算已知危险)
fn collect_literal_commands(src: &str) -> Vec<Vec<String>> {
    let Some(tree) = parse(src) else {
        return Vec::new();
    };
    let bytes = src.as_bytes();
    let mut commands = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.is_named() && node.kind() == "command" {
            if let Some(argv) = extract_literal_argv(node, bytes) {
                commands.push(argv);
            }
        }
        for i in (0..node.child_count()).rev() {
            if let Some(c) = node.child(i) {
                stack.push(c);
            }
        }
    }
    commands
}

/// 提取 command 的字面 argv(named children 的静态文本)
/// 遇到动态词(substitution/${} 等)→ 返回 None(无法确定字面值)
fn extract_literal_argv(cmd: Node, src: &[u8]) -> Option<Vec<String>> {
    let mut argv = Vec::new();
    let mut i = 0;
    while i < cmd.named_child_count() {
        let child = cmd.named_child(i)?;
        match literal_text(child, src) {
            Some(t) => argv.push(t),
            None => return None, // 动态词:无法证明字面
        }
        i += 1;
    }
    Some(argv)
}

/// 节点字面文本(仅静态构造;动态构造返回 None)
fn literal_text(node: Node, src: &[u8]) -> Option<String> {
    match node.kind() {
        "word" | "command_name" | "number" => Some(node_text(node, src).to_string()),
        "raw_string" => {
            // 'content':去首尾单引号
            let t = node_text(node, src);
            strip_quotes(t)
        }
        "string" => {
            // "content":取 string_content 子节点,或去引号
            if let Some(sc) = node.named_child(0) {
                if sc.kind() == "string_content" {
                    return Some(node_text(sc, src).to_string());
                }
            }
            strip_quotes(node_text(node, src))
        }
        "concatenation" => {
            // 拼接子节点(仅静态)
            let mut s = String::new();
            let mut i = 0;
            while i < node.named_child_count() {
                if let Some(c) = node.named_child(i) {
                    match literal_text(c, src) {
                        Some(t) => s.push_str(&t),
                        None => return None,
                    }
                }
                i += 1;
            }
            if s.is_empty() { None } else { Some(s) }
        }
        // substitution/command_substitution/process_substitution/heredoc 等 → 动态,拒
        _ => None,
    }
}

/// 节点文本(byte 范围切源码)
fn node_text<'a>(node: Node, src: &'a [u8]) -> &'a str {
    std::str::from_utf8(&src[node.start_byte()..node.end_byte()]).unwrap_or("")
}

/// 去首尾引号(单/双)
fn strip_quotes(t: &str) -> Option<String> {
    let bytes = t.as_bytes();
    if bytes.len() >= 2
        && (bytes[0] == b'"' || bytes[0] == b'\'')
        && bytes[0] == bytes[bytes.len() - 1]
    {
        Some(t[1..t.len() - 1].to_string())
    } else {
        Some(t.to_string())
    }
}

// ============ 安全判定 ============

/// 单命令是否安全(白名单 + 选项级排除)
fn is_safe_command(argv: &[String]) -> bool {
    let Some(first) = argv.first() else {
        return false;
    };
    match first.as_str() {
        // git:仅只读子命令 + 无危险全局选项
        "git" => is_safe_git(argv),
        // find:无危险选项(-exec/-delete/写文件)
        "find" => !argv
            .iter()
            .skip(1)
            .any(|a| UNSAFE_FIND_OPTS.contains(&a.as_str())),
        // base64:无 -o/--output(写文件)
        "base64" => !argv
            .iter()
            .skip(1)
            .any(|a| UNSAFE_BASE64_OPTS.contains(&a.as_str())),
        // 通用安全命令白名单(首词精确匹配)
        _ => SAFE_COMMANDS.contains(&first.as_str()),
    }
}

/// git 安全:无危险全局选项 + 子命令在白名单
fn is_safe_git(argv: &[String]) -> bool {
    // 任一危险全局选项 → 非 safe
    if argv
        .iter()
        .skip(1)
        .any(|a| UNSAFE_GIT_OPTS.contains(&a.as_str()))
    {
        return false;
    }
    // 子命令 = 第一个非 - 开头的参数
    argv.iter()
        .skip(1)
        .find(|a| !a.starts_with('-'))
        .map_or(false, |s| SAFE_GIT_SUBCMDS.contains(&s.as_str()))
}

// ============ 致命检测(不可覆盖 Deny) ============

/// 递归发现致命命令(返回原因;命中即 Deny,不可覆盖)
/// 递归 bash -lc '...' 脚本字符串 + env 解包,深度上限 MAX_WRAPPER_DEPTH
fn find_fatal(src: &str, depth: usize) -> Option<String> {
    if depth > MAX_WRAPPER_DEPTH {
        return None;
    }
    for argv in collect_literal_commands(src) {
        if let Some(reason) = command_is_fatal(&argv, depth) {
            return Some(reason);
        }
    }
    None
}

/// 单命令是否致命(含递归解包 bash/env)
/// 仅放「几乎无合法用途」的极端模式;误判会卡死合法操作,务必保守
fn command_is_fatal(argv: &[String], depth: usize) -> Option<String> {
    let first = argv.first()?;
    match first.as_str() {
        // shell 包装器:递归进 -c/-lc 脚本字符串
        "bash" | "sh" | "zsh" | "dash" => {
            if let Some(script) = dash_c_script(argv) {
                if let Some(r) = find_fatal(&script, depth + 1) {
                    return Some(r);
                }
            }
            None
        }
        // env:跳过赋值,查内部命令
        "env" => env_unwrap(argv).and_then(|inner| command_is_fatal(&inner, depth + 1)),
        // rm -rf 根/家目录:删系统
        // 精确匹配目标参数(避免误杀 rm -rf ./build)
        "rm" => {
            let has_r = argv.iter().skip(1).any(|a| {
                (a.starts_with('-') && !a.starts_with("--") && a.contains('r'))
                    || a == "--recursive"
            });
            let has_f = argv.iter().skip(1).any(|a| {
                (a.starts_with('-') && !a.starts_with("--") && a.contains('f')) || a == "--force"
            });
            let target_root = argv
                .iter()
                .skip(1)
                .any(|a| matches!(a.as_str(), "/" | "/*" | "~" | "$HOME"));
            if has_r && has_f && target_root {
                Some("rm 递归强制删除根/家目录".into())
            } else {
                None
            }
        }
        // 格式化磁盘:本地开发几乎不会用
        "mkfs" | "mke2fs" | "mkfs.ext2" | "mkfs.ext3" | "mkfs.ext4" | "mkfs.btrfs" | "mkfs.xfs"
        | "mkfs.vfat" | "mkfs.ntfs" => Some("格式化磁盘".into()),
        // dd 写裸块设备:覆盖磁盘数据
        "dd" => {
            if argv.iter().any(|a| {
                a.starts_with("of=/dev/sd")
                    || a.starts_with("of=/dev/nvme")
                    || a.starts_with("of=/dev/hd")
                    || a.starts_with("of=/dev/disk/")
            }) {
                Some("dd 覆盖块设备".into())
            } else {
                None
            }
        }
        _ => None,
    }
}

// ============ 危险发现 ============

/// 宽松解析 + 递归发现危险命令
/// - 查 DANGER 黑名单(对真实首词)
/// - bash/sh -c 'script':递归 parse 脚本字符串
/// - env [NAME=VAL]... cmd:跳过赋值,查内部命令
/// 深度上限 MAX_WRAPPER_DEPTH,防无限递归
fn find_dangerous(src: &str, depth: usize) -> bool {
    if depth > MAX_WRAPPER_DEPTH {
        return false;
    }
    for argv in collect_literal_commands(src) {
        if command_is_dangerous(&argv, depth) {
            return true;
        }
    }
    false
}

/// 单命令是否危险(含递归解包)
fn command_is_dangerous(argv: &[String], depth: usize) -> bool {
    let Some(first) = argv.first() else {
        return false;
    };
    match first.as_str() {
        // shell 包装器:递归进 -c/-lc 的脚本字符串
        "bash" | "sh" | "zsh" | "dash" => {
            if let Some(script) = dash_c_script(argv) {
                if find_dangerous(&script, depth + 1) {
                    return true;
                }
            }
            // shell 本身不危险,继续(可能 argv 含其他)
            false
        }
        // env:跳过赋值/选项,查内部命令
        "env" => {
            if let Some(inner) = env_unwrap(argv) {
                if command_is_dangerous(&inner, depth + 1) {
                    return true;
                }
            }
            false
        }
        // 其余:查 DANGER 黑名单
        _ => DANGER.contains(&first.as_str()),
    }
}

/// 提取 `bash -c 'script'` / `bash -lc 'script'` 的脚本参数
fn dash_c_script(argv: &[String]) -> Option<String> {
    let mut iter = argv.iter().skip(1);
    while let Some(a) = iter.next() {
        if a == "-c" || a == "-lc" {
            return iter.next().cloned();
        }
    }
    None
}

/// env 解包:跳过 -i/--ignore-environment 选项和 NAME=VALUE 赋值,返回首个真命令及其 argv
fn env_unwrap(argv: &[String]) -> Option<Vec<String>> {
    let mut i = 1;
    while i < argv.len() {
        let a = &argv[i];
        if a == "--" {
            i += 1;
            break;
        }
        if a == "-i" || a == "--ignore-environment" {
            i += 1;
            continue;
        }
        // NAME=VALUE 形式的赋值(跳过)
        if let Some((name, _)) = a.split_once('=') {
            if !name.is_empty() && !name.starts_with('-') {
                i += 1;
                continue;
            }
        }
        break; // 首个非赋值非选项 = 真命令
    }
    if i < argv.len() {
        Some(argv[i..].to_vec())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- 原有用例回归 ---

    #[test]
    fn readonly_allow() {
        assert!(matches!(classify("ls -la"), Action::Allow));
        assert!(matches!(classify("cat foo.txt"), Action::Allow));
        assert!(matches!(classify("grep -r pattern src/"), Action::Allow));
        assert!(matches!(classify("pwd"), Action::Allow));
        assert!(matches!(classify("wc -l file"), Action::Allow));
    }

    #[test]
    fn danger_ask() {
        assert!(matches!(
            classify("rm -rf /tmp/x"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
        assert!(matches!(
            classify("curl http://x"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
        assert!(matches!(
            classify("sudo apt update"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
        assert!(matches!(
            classify("dd if=/dev/zero of=/dev/sda"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
    }

    #[test]
    fn danger_syntax_ask() {
        // 命令替换/重定向/后台 → 严格解析失败,且无已知危险命令 → 未知 Ask{false}
        assert!(matches!(
            classify("echo $(whoami)"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
        assert!(matches!(
            classify("echo hi > file"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
        assert!(matches!(
            classify("sleep 10 &"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
    }

    #[test]
    fn compound_strictest_wins() {
        // ls Allow + rm Ask{false} → 整体 Ask{false}(rm 命中 DANGER)
        assert!(matches!(
            classify("ls && rm x"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
        // 两段都 safe → Allow
        assert!(matches!(classify("ls | grep foo"), Action::Allow));
        // 管道 + safe
        assert!(matches!(classify("cat a | head -1 | wc -l"), Action::Allow));
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
            classify("make install"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
    }

    // --- 新增:选项级排除 ---

    #[test]
    fn find_safe_and_unsafe() {
        // 无危险选项 → safe(Allow)
        assert!(matches!(classify("find . -name '*.rs'"), Action::Allow));
        // -exec/-delete → 非 safe → Ask{false}
        assert!(matches!(
            classify("find . -exec rm {} \\;"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
        assert!(matches!(
            classify("find . -delete"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
    }

    #[test]
    fn git_safe_subcommand() {
        // 只读子命令 → safe
        assert!(matches!(classify("git status"), Action::Allow));
        assert!(matches!(classify("git log --oneline -5"), Action::Allow));
        assert!(matches!(classify("git diff"), Action::Allow));
        // 危险全局选项 → 非 safe
        assert!(matches!(
            classify("git -C /evil status"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
        assert!(matches!(
            classify("git --git-dir=/x log"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
        // 非只读子命令 → 非 safe
        assert!(matches!(
            classify("git push"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
        assert!(matches!(
            classify("git reset --hard"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
    }

    // --- 新增:env/sudo 解包 + bash -lc 递归 ---

    #[test]
    fn env_unwrap_danger() {
        // env FOO=bar rm → 解包发现 rm → Ask{false}
        assert!(matches!(
            classify("env FOO=bar rm -rf /tmp/x"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
        // env 仅赋值,无命令 → 未知 Ask
        assert!(matches!(
            classify("env FOO=bar"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
    }

    #[test]
    fn bash_lc_recursion_finds_danger() {
        // bash -lc 'rm -rf /tmp' → 递归发现 rm → Ask{false}
        assert!(matches!(
            classify("bash -lc 'rm -rf /tmp/x'"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
        // 双引号脚本
        assert!(matches!(
            classify("bash -c \"rm -rf /tmp/x\""),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
        // 嵌套:sh -c 'bash -c "rm /tmp/x"' → 深度递归仍发现
        assert!(matches!(
            classify("sh -c 'bash -c \"rm /tmp/x\"'"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
    }

    #[test]
    fn bash_lc_safe_script() {
        // bash -lc 'ls' → 递归发现 ls safe,但 bash 本身非白名单 → 未知 Ask
        // (bash 不在 SAFE_COMMANDS,保持保守)
        assert!(matches!(
            classify("bash -lc 'ls'"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
    }

    #[test]
    fn empty_command() {
        assert!(matches!(
            classify(""),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
        assert!(matches!(
            classify("   "),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
    }

    #[test]
    fn env_unwrap_unit() {
        // 单元测试 env 解包逻辑
        let argv = vec![
            "env".to_string(),
            "FOO=bar".into(),
            "rm".into(),
            "-rf".into(),
        ];
        let inner = env_unwrap(&argv).unwrap();
        assert_eq!(inner, vec!["rm".to_string(), "-rf".into()]);

        // 无命令
        let argv2 = vec!["env".to_string(), "FOO=bar".into()];
        assert!(env_unwrap(&argv2).is_none());

        // -- 后跟命令
        let argv3 = vec!["env".to_string(), "--".to_string(), "ls".into()];
        assert_eq!(env_unwrap(&argv3).unwrap(), vec!["ls".to_string()]);
    }

    #[test]
    fn dash_c_script_unit() {
        let argv = vec!["bash".to_string(), "-lc".into(), "rm -rf".into()];
        assert_eq!(dash_c_script(&argv), Some("rm -rf".to_string()));

        let argv2 = vec!["bash".to_string(), "-c".into(), "ls".into()];
        assert_eq!(dash_c_script(&argv2), Some("ls".to_string()));

        let argv3 = vec!["bash".to_string(), "-e".into(), "script".into()];
        assert_eq!(dash_c_script(&argv3), None);
    }

    // --- 致命命令(Deny 不可覆盖) ---

    #[test]
    fn fatal_rm_root_deny() {
        assert!(matches!(classify("rm -rf /"), Action::Deny(_)));
        assert!(matches!(classify("rm -fr /"), Action::Deny(_)));
        assert!(matches!(classify("rm -rf /*"), Action::Deny(_)));
        assert!(matches!(classify("rm -rf ~"), Action::Deny(_)));
        assert!(matches!(classify("rm -rf $HOME"), Action::Deny(_)));
        assert!(matches!(
            classify("rm --recursive --force /"),
            Action::Deny(_)
        ));
        // 非根目录:不命中致命,仍 Ask{false}(rm 在 DANGER)
        assert!(matches!(
            classify("rm -rf ./build"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
        assert!(matches!(
            classify("rm -rf /tmp/x"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
    }

    #[test]
    fn fatal_mkfs_deny() {
        assert!(matches!(classify("mkfs.ext4 /dev/sda"), Action::Deny(_)));
        assert!(matches!(classify("mkfs /dev/sda1"), Action::Deny(_)));
        assert!(matches!(classify("mke2fs /dev/sda"), Action::Deny(_)));
    }

    #[test]
    fn fatal_dd_device_deny() {
        assert!(matches!(
            classify("dd if=/dev/zero of=/dev/sda"),
            Action::Deny(_)
        ));
        assert!(matches!(
            classify("dd if=/dev/zero of=/dev/nvme0n1"),
            Action::Deny(_)
        ));
        // 写普通文件:不命中致命,仍 Ask{false}(dd 在 DANGER)
        assert!(matches!(
            classify("dd if=/dev/zero of=/tmp/img bs=1M count=10"),
            Action::Ask {
                persistable: false,
                ..
            }
        ));
    }

    #[test]
    fn fatal_recursion_through_wrapper() {
        // bash -lc 内部藏致命命令 → 递归发现 Deny
        assert!(matches!(classify("bash -lc 'rm -rf /'"), Action::Deny(_)));
        assert!(matches!(
            classify("sh -c 'mkfs.ext4 /dev/sda'"),
            Action::Deny(_)
        ));
        // env 解包
        assert!(matches!(classify("env FOO=bar rm -rf /"), Action::Deny(_)));
    }
}
