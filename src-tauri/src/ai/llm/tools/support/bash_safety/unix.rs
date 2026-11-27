//! unix(sh/bash)命令安全分类:基于 tree-sitter-bash 的 AST。
//!
//! **为什么不用系统解析器**:bash 没有 PowerShell `ParseInput` 那样的「只解析不执行」接口。
//! `bash -n` 只给退出码,不给结构;`bash --pretty-print` 实测**会真的执行命令**(输出是命令
//! 结果而非脚本),不能用于鉴权前解析。故 AST 只能来自库,这是与 windows 侧的唯一方案差异。
//!
//! **语法模型与执行 shell 的对应**:执行侧固定 `sh -c`(见 `support/shell.rs`),而 `sh` 的
//! 实现因平台而异——macOS 10.15 起 `/bin/sh` 是 `/private/var/select/sh` 的 shim,可指向
//! bash(默认,3.2)/ dash / zsh 的 sh 模式;Linux 上多为 dash 或 ash。三者都是 POSIX 实现,
//! 语法是 bash 的子集,故 tree-sitter-bash 看得比执行的多:能执行的它都能解析(dash 不认的
//! bash 扩展是语法错误、不会执行),方向安全。
//!
//! 职责两件:解析(源码 → 字面 argv)、判定(argv → 四档)。词表在 `config::BashPolicy`,
//! 规则匹配在 `matcher`,本文件不持有任何词条,也不关心调用方是谁。
//!
//! `&` 是后台执行(非白名单节点 → 非 plain);匹配大小写不敏感与 win 一致
//! (`compile_one` 统一;unix 无数据域归一,`LS` 会命中 `ls` 白名单——命令不存在,执行即失败)。

use super::{
    check_user_policy, matches_allow_rules, matches_ask_always_rules, matches_deny_rules,
    max_action, DENY_REASON,
};
use crate::ai::llm::tools::Action;
use tree_sitter::{Node, Parser, Tree};

/// 递归深度上限(仿 Codex,防 `bash -c 'bash -c ...'` 无限递归)
const MAX_WRAPPER_DEPTH: usize = 8;

/// 白名单节点类型(严格解析:只允许这些构造,出现其他 → 非 plain);参考 Codex ALLOWED_KINDS
const ALLOWED_KINDS: &[&str] = &[
    "program",             // 根
    "list",                // 命令序列 `a; b` `a && b`
    "pipeline",            // `a | b`
    "command",             // 单条命令
    "command_name",        // 命令名
    "word",                // 字面词
    "string",              // 双引号串(含 string_content)
    "string_content",      // 串内文本
    "raw_string",          // 单引号串(无插值)
    "number",              // 数字
    "concatenation",       // 词拼接 `"a""b"`
    "variable_assignment", // `FOO=1 cmd` 的赋值前缀(值本身仍受白名单约束)
    "variable_name",       // 赋值左侧的名字
];

/// 白名单标点(严格解析);拒重定向 `>` `<`、命令替换 `$()`、反引号、后台 `&`、括号、控制流等
const ALLOWED_PUNCT: &[&str] = &["&&", "||", ";", "|", "=", "\"", "'"];

// 安全 / 危险词表已移至 `config::BashPolicy`(代码内置平台默认;用户规则经 `init` 合表,
// deny/always_ask 只增不改,allow/ask 可覆盖内置基线);
// 此处仅留解析算法常量。

/// git 不安全全局选项(出现即非 allow:可逃逸工作目录/改配置);参考 Codex UNSAFE_GIT_GLOBAL_OPTIONS
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

/// git 安全子命令(仅这些只读子命令算 allow)
const ALLOW_GIT_SUBCMDS: &[&str] = &["status", "log", "diff", "show", "branch"];

/// find 不安全选项(出现即非 allow:可执行任意命令/删除/写文件)
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

/// 命令分类入口:四档严重度,跨来源按严重度合并取最大(致命档提前返回):
/// - Deny(3)     致命硬红线 + 用户 deny
/// - AskAlways(2) 危险命令 + 用户 always_ask + 未知兜底
/// - AskOnce(1)  用户 ask + 内置基线(grant 后降为 Allow)
/// - Allow(0)    严格解析证明安全 + 用户 allow
///
/// 用户 allow 压不过 AskAlways——修「allow 绕过危险确认」的漏洞。
pub fn classify(cmd: &str) -> Action {
    classify_depth(cmd, 0)
}

/// 带递归深度的分类(AST 下 `bash -c` 内层是新的命令行,由 `check_deny`/`check_ask_always`
/// 内部递归解包,故此处无需独立包装器循环)。深度上限防无限嵌套。
///
/// 四档各自是一个方法,方法名对应其产出的 `Action` 类型:
/// - `check_deny`        → Deny(致命硬红线 + 参数组合结构匹配,不可覆盖,提前 return)
/// - `check_ask_always`  → AskAlways(危险规则表:内置+用户合表)
/// - `check_user_policy` → 用户策略(Deny/AskAlways/AskOnce/Allow,含内置基线 onceask + grant)
/// - `check_allow`       → Allow(严格解析证明安全,合取:全命令安全才 Allow)
fn classify_depth(cmd: &str, depth: usize) -> Action {
    // 0. Deny:致命硬红线,不可覆盖,最高优先级 → 直接返回
    if let Some(reason) = check_deny(cmd, depth) {
        return Action::Deny(reason);
    }

    // 1~2. 其余档**逐个调用、并入同一累加器**,按严重度取最大(非「先命中先赢」,顺序无关);
    // 都没命中 → None,末尾兜底 AskAlways。
    let mut action: Option<Action> = None;
    // AskAlways:危险规则表(内置+用户合表)
    if check_ask_always(cmd, depth) {
        action = max_action(action, Action::AskAlways);
    }
    // 用户策略([permissions.bash],含内置基线 onceask + grant):按子命令粒度匹配
    if let Some(a) = check_user_policy(&split_bash_subcommands(cmd)) {
        action = max_action(action, a);
    }

    // 3. Allow 合取:整段严格解析为安全命令才整体 Allow;空命令/vacuous 不能判 Allow
    if check_allow(cmd) {
        action = max_action(action, Action::Allow);
    }

    // 未知 → 兜底 AskAlways
    action.unwrap_or(Action::AskAlways)
}

/// 拆 bash 命令为子命令(按 & | ; 分隔),供 config 规则按子命令粒度匹配(避免 "ls *" 放行 "ls && rm x")。
/// 已知限制:不认引号(`echo 'a && b'` 被切两段)——只影响 check_user_policy 的段粒度,
/// 段更短只会更易命中用户规则/基线,方向保守(deny/ask_always 走 AST,不受影响)
fn split_bash_subcommands(command: &str) -> Vec<String> {
    command
        .split(['&', '|', ';'])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// 整条命令是否严格证明安全(→ Allow):AST 解析为全白名单命令且无危险构造;
/// 空命令(vacuous truth 陷阱)→ 非安全,落保守 Ask
fn check_allow(cmd: &str) -> bool {
    parse_plain_commands(cmd)
        .is_some_and(|cmds| !cmds.is_empty() && cmds.iter().all(|c| command_is_allow(c)))
}

// ============ AST 解析 ============

/// tree-sitter 解析(复用 thread_local Parser)
fn parse(src: &str) -> Option<Tree> {
    PARSER.with(|cell| cell.borrow_mut().parse(src.as_bytes(), None))
}

/// 严格解析:整段只含白名单节点 + 白名单标点时,返回所有 command 的字面 argv
/// 任一非白名单构造(重定向/命令替换/反引号/后台&/括号/控制流/heredoc)→ None
fn parse_plain_commands(src: &str) -> Option<Vec<Vec<String>>> {
    let tree = parse(src)?;
    let bytes = src.as_bytes();
    let mut commands = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if node.is_named() {
            if !ALLOWED_KINDS.contains(&kind) {
                return None;
            }
            if kind == "command" {
                // 命令含动态词(${} 等)时返回 None:非纯 plain
                commands.push(extract_literal_argv(node, bytes)?);
            }
        } else if !(ALLOWED_PUNCT.contains(&kind) || kind.trim().is_empty()) {
            // 匿名标点只放行白名单(&& || ; | 引号 + 换行);重定向/括号/反引号/单个 & 一律拒
            return None;
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

/// 宽松解析:收集 AST 中所有 command 的字面 argv(允许任意语法),用于「发现危险」
/// 动态词(${} )跳过(不安全,但也不算已知危险)
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

/// 提取 command 的字面 argv;遇动态词(substitution/${} 等)→ None
fn extract_literal_argv(cmd: Node, src: &[u8]) -> Option<Vec<String>> {
    let mut argv = Vec::new();
    for i in 0..cmd.named_child_count() {
        let child = cmd.named_child(i)?;
        // `FOO=1 cmd`:赋值前缀设的是环境,不是 argv,跳过(否则整条命令被丢弃 → 漏 Deny)
        if child.kind() == "variable_assignment" {
            continue;
        }
        argv.push(literal_text(child, src)?);
    }
    Some(argv)
}

/// 节点字面文本(仅静态构造;动态构造返回 None)
fn literal_text(node: Node, src: &[u8]) -> Option<String> {
    match node.kind() {
        "word" | "number" => Some(node_text(node, src).to_string()),
        // 命令名可能包着 word,也可能包着 simple_expansion($CMD);递归到内部节点判定
        "command_name" => literal_text(node.named_child(0)?, src),
        // 变量按原文当字面串:不展开值,但也不能整条丢弃——`rm -rf $HOME` 要靠这个字面
        // 命中 `is_root_target`。它换不来 Allow(`parse_plain_commands` 的白名单没有 expand)
        "simple_expansion" | "expansion" => Some(node_text(node, src).to_string()),
        // 'text':两侧引号由 grammar 保证成对
        "raw_string" => {
            let t = node_text(node, src);
            Some(
                t.get(1..t.len().saturating_sub(1))
                    .unwrap_or("")
                    .to_string(),
            )
        }
        // "text":只认纯字符串内容。含 expansion / command_substitution 的串不是字面量
        // (实测 `"a$(rm -rf /)c"` 的 string 下有 string_content + command_substitution +
        // string_content,只取首个子节点会把后半个参数悄悄丢掉)
        "string" => {
            let mut s = String::new();
            for i in 0..node.named_child_count() {
                let c = node.named_child(i)?;
                if c.kind() != "string_content" {
                    return None;
                }
                s.push_str(node_text(c, src));
            }
            Some(s)
        }
        "concatenation" => {
            // 拼接子节点(仅静态)
            let mut s = String::new();
            for i in 0..node.named_child_count() {
                s.push_str(&literal_text(node.named_child(i)?, src)?);
            }
            (!s.is_empty()).then_some(s)
        }
        // substitution/command_substitution/process_substitution/heredoc 等 → 动态,拒
        _ => None,
    }
}

/// 节点文本(byte 范围切源码)
fn node_text<'a>(node: Node, src: &'a [u8]) -> &'a str {
    std::str::from_utf8(&src[node.start_byte()..node.end_byte()]).unwrap_or("")
}

// ============ 安全判定 ============

/// 单命令是否 Allow(白名单 + 选项级排除)
fn command_is_allow(argv: &[String]) -> bool {
    let Some(first) = argv.first() else {
        return false;
    };
    match first.as_str() {
        // git:仅只读子命令 + 无危险全局选项
        "git" => git_is_allow(argv),
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
        // 通用白名单(首词匹配)
        _ => matches_allow_rules(first),
    }
}

/// git 安全:无危险全局选项 + 子命令在白名单
fn git_is_allow(argv: &[String]) -> bool {
    // 任一危险全局选项 → 非 allow
    // 支持 --opt 和 --opt=value 两种形式(=value 前缀匹配)
    if argv.iter().skip(1).any(|a| {
        let a = a.as_str();
        UNSAFE_GIT_OPTS.contains(&a)
            || UNSAFE_GIT_OPTS
                .iter()
                .any(|opt| a.starts_with(opt) && a[opt.len()..].starts_with('='))
    }) {
        return false;
    }
    // 子命令 = 第一个非 - 开头的参数
    argv.iter()
        .skip(1)
        .find(|a| !a.starts_with('-'))
        .is_some_and(|s| ALLOW_GIT_SUBCMDS.contains(&s.as_str()))
}

// ============ 致命检测(不可覆盖 Deny) ============

/// 递归发现致命命令(命中即 Deny,不可覆盖):bash -lc 脚本 + env 解包,深度上限 MAX_WRAPPER_DEPTH
fn check_deny(src: &str, depth: usize) -> Option<String> {
    if depth > MAX_WRAPPER_DEPTH {
        return None;
    }
    for argv in collect_literal_commands(src) {
        if let Some(reason) = command_is_deny(&argv, depth) {
            return Some(reason);
        }
    }
    None
}

/// 单命令是否致命(含递归解包 bash/env);仅放「几乎无合法用途」的极端模式,务必保守
/// 致命红线表(内置+用户合表,编译缓存于 matcher)覆盖命令名即致命项;参数组合致命项
/// (rm -rf /、dd of=/dev/sd*)pattern 表达不了,故结构匹配
fn command_is_deny(argv: &[String], depth: usize) -> Option<String> {
    let first = argv.first()?;
    match first.as_str() {
        // shell 包装器:递归进 -c 脚本;自身仍查表(用户可配 `bash *` = deny)
        "bash" | "sh" | "zsh" | "dash" => dash_c_script(argv)
            .and_then(|script| check_deny(&script, depth + 1))
            .or_else(|| deny_by_table(argv)),
        // env:跳过选项/赋值后查内部命令(`env -u FOO rm -rf /`)
        "env" => env_unwrap(argv)
            .and_then(|inner| command_is_deny(&inner, depth + 1))
            .or_else(|| deny_by_table(argv)),
        // rm -rf 根/家目录(参数组合致命,glob 表达不了)
        "rm" => {
            // 短选项簇里的字母:`-rf` / `-Rf` / `-fr`;长选项另判
            let has = |c: char| {
                argv.iter()
                    .skip(1)
                    .any(|a| a.starts_with('-') && !a.starts_with("--") && a.contains(c))
            };
            let recursive = has('r') || has('R') || argv.iter().skip(1).any(|a| a == "--recursive");
            let force = has('f') || argv.iter().skip(1).any(|a| a == "--force");
            let root = argv.iter().skip(1).any(|a| is_root_target(a));
            if recursive && force && root {
                Some("rm 递归强制删除根/家目录".into())
            } else {
                // 非根目录的 rm:非致命,交给基线(AskOnce);顺带查致命红线表
                deny_by_table(argv)
            }
        }
        // dd 写裸块设备(参数组合致命)
        "dd" => {
            if argv.iter().any(|a| is_block_device(a)) {
                Some("dd 覆盖块设备".into())
            } else {
                deny_by_table(argv)
            }
        }
        // 其余:查致命红线表(内置+用户合表,pattern 形式,如 mkfs*)
        _ => deny_by_table(argv),
    }
}

/// 整行查致命红线表(内置+用户合表)
fn deny_by_table(argv: &[String]) -> Option<String> {
    matches_deny_rules(&argv.join(" ")).then(|| DENY_REASON.to_string())
}

/// 目标是否文件系统根 / 家目录根:收 `/`、`/*`、`//`、`~/`、`~/*`、`$HOME`、`${HOME}` 等变形。
/// 只认「整根」——`/tmp/x` 是正常清理,不算。
///
/// 花括号:bash 执行前会把 `{/,tmp}` 拆成 `/ tmp` 两个目标,静态看却只是一个字符串。
/// 不拆就会把 `rm -rf {/,tmp}` 判成普通清理,故先按成员拆一遍,任一成员是根即按根处理
/// (`{src/,build/}` 拆完都不是根,照旧走 AskOnce)。
fn is_root_target(a: &str) -> bool {
    if let Some(members) = brace_members(a) {
        return members.iter().any(|m| is_root_target(m));
    }
    let t = a.trim_end_matches("/*").trim_end_matches('/');
    matches!(t, "" | "/" | "~" | "$HOME" | "${HOME}")
}

/// 拆一层花括号,成员拼回前后缀(`{/,tmp}/*` → `//*`、`tmp/*`)。只拆顶层逗号,嵌套留给递归;
/// `{1..3}` / `{}` 没有顶层逗号 → `None`(它们展开不出根)。
/// 边界:引号内的花括号 bash 不展开(`"{/,tmp}"` 是字面名),字符串层区分不了,会保守判成根。
fn brace_members(a: &str) -> Option<Vec<String>> {
    let open = a.find('{')?;
    let mut level = 0usize;
    let mut close = None;
    for (i, c) in a[open..].char_indices() {
        match c {
            '{' => level += 1,
            '}' => {
                level -= 1;
                if level == 0 {
                    close = Some(open + i);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close?;
    let (head, inner, tail) = (&a[..open], &a[open + 1..close], &a[close + 1..]);

    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (i, c) in inner.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&inner[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if parts.is_empty() {
        return None;
    }
    parts.push(&inner[start..]);
    Some(parts.iter().map(|p| format!("{head}{p}{tail}")).collect())
}

/// 是否为裸块设备(写它等于毁盘);`/dev/null`、`/dev/zero` 等字符设备不在其中
fn is_block_device(a: &str) -> bool {
    [
        "of=/dev/sd",
        "of=/dev/hd",
        "of=/dev/vd",
        "of=/dev/xvd",
        "of=/dev/nvme",
        "of=/dev/mmcblk",
        "of=/dev/disk/",
    ]
    .iter()
    .any(|p| a.starts_with(p))
}

// ============ 危险发现 ============

/// 宽松解析 + 递归发现危险命令(bash/sh -c 脚本、env 解包),深度上限 MAX_WRAPPER_DEPTH
fn check_ask_always(src: &str, depth: usize) -> bool {
    if depth > MAX_WRAPPER_DEPTH {
        return false;
    }
    for argv in collect_literal_commands(src) {
        if command_is_ask_always(&argv, depth) {
            return true;
        }
    }
    false
}

/// 单命令是否危险(含递归解包)
fn command_is_ask_always(argv: &[String], depth: usize) -> bool {
    let Some(first) = argv.first() else {
        return false;
    };
    match first.as_str() {
        // shell 包装器:递归进 -c 脚本;自身仍查表(用户可配 `bash *` = always_ask)
        "bash" | "sh" | "zsh" | "dash" => {
            dash_c_script(argv).is_some_and(|s| check_ask_always(&s, depth + 1))
                || matches_ask_always_rules(&argv.join(" "))
        }
        // env:跳过选项/赋值后查内部命令
        "env" => {
            env_unwrap(argv).is_some_and(|inner| command_is_ask_always(&inner, depth + 1))
                || matches_ask_always_rules(&argv.join(" "))
        }
        // 其余:查危险规则表(内置+用户合表:命令项 + 语法项,统一通配符匹配)
        _ => matches_ask_always_rules(&argv.join(" ")),
    }
}

/// 提取 shell 的 `-c` 脚本参数:任一短选项簇含 c(`-c` / `-lc` / `-cl` / `-euc`)都算,
/// 其后第一个参数即脚本。`--` 之后是位置参数,不再当选项
fn dash_c_script(argv: &[String]) -> Option<String> {
    let mut iter = argv.iter().skip(1);
    while let Some(a) = iter.next() {
        if a == "--" {
            return None;
        }
        if a.len() >= 2 && a.starts_with('-') && !a.starts_with("--") && a.contains('c') {
            return iter.next().cloned();
        }
    }
    None
}

/// env 解包:跳过选项与 NAME=VALUE 赋值,返回首个真命令及其 argv。
/// `-u`/`-C`/`-S` 各吃一个参数,必须连带跳过,否则会把参数值当成命令名
fn env_unwrap(argv: &[String]) -> Option<Vec<String>> {
    let mut i = 1;
    while i < argv.len() {
        let a = argv[i].as_str();
        match a {
            "--" => {
                i += 1;
                break;
            }
            "-u" | "-C" | "-S" => {
                i += 2;
                continue;
            }
            "-i" | "-0" | "-v" | "--ignore-environment" | "--null" | "--debug" => {
                i += 1;
                continue;
            }
            _ => {}
        }
        // --unset=NAME / --chdir=DIR / --split-string=STR:自带值,不吃后面的参数
        if ["--unset=", "--chdir=", "--split-string="]
            .iter()
            .any(|p| a.starts_with(p))
        {
            i += 1;
            continue;
        }
        // NAME=VALUE 形式的赋值
        if let Some((name, _)) = a.split_once('=') {
            if !name.is_empty() && !name.starts_with('-') {
                i += 1;
                continue;
            }
        }
        break; // 首个非赋值非选项 = 真命令
    }
    (i < argv.len()).then(|| argv[i..].to_vec())
}

// ============ 测试 ============
// 只测平台无关的解析与结构判定(各档词表是 `#[cfg]` 分平台的,`classify` 跨平台结果不同)。
// 依赖:模块在 `cfg(any(unix, test))` 下编译,Windows 开发机也能跑这些用例。
#[cfg(test)]
mod tests {
    use super::*;

    fn deny(cmd: &str) -> bool {
        check_deny(cmd, 0).is_some()
    }

    /// 取第一条命令的字面 argv
    fn argv_of(cmd: &str) -> Option<Vec<String>> {
        extract_literal_argv(find_first_command(parse(cmd)?.root_node())?, cmd.as_bytes())
    }

    fn find_first_command(n: Node) -> Option<Node> {
        if n.kind() == "command" {
            return Some(n);
        }
        (0..n.child_count()).find_map(|i| find_first_command(n.child(i)?))
    }

    /// 递归强删根/家目录的各种写法;`./build` 这类正常清理不能误判
    #[test]
    fn recursive_force_remove_of_root_is_denied() {
        for cmd in [
            "rm -rf /",
            "rm -Rf /", // 大写 R 与 -r 同义
            "rm -fr ~/",
            "rm -rf /*",
            "rm -rf $HOME",
            "rm -rf ${HOME}/",
            "rm -rf //",
            "rm --recursive --force /",
        ] {
            assert!(deny(cmd), "应判致命: {cmd}");
        }
        for cmd in [
            "rm -rf ./build",
            "rm -rf /tmp/x",
            "rm -r /tmp/x", // 递归但没 force
            "rm -f /",      // force 但没递归
        ] {
            assert!(!deny(cmd), "不应判致命: {cmd}");
        }
    }

    /// 花括号展开:静态只是一个字符串,展开后才是真目标(`rm -rf {/,tmp}` → `/ tmp`)
    #[test]
    fn brace_expansion_of_root_is_denied() {
        for cmd in [
            "rm -rf {/,tmp}",
            "rm -rf {a,{/,b}}", // 嵌套
            "rm -rf {/,tmp}/*", // 前缀/后缀
            "rm -rf ~{/,x}",    // `~/` 与 `~x`
            "rm -rf {/,}",      // 空成员
        ] {
            assert!(deny(cmd), "应判致命: {cmd}");
        }
        // 展开后都不是根 → 不致命,照旧由基线问一句
        for cmd in ["rm -rf {src/,build/}", "rm -rf {a,b}", "rm -rf {1..3}"] {
            assert!(!deny(cmd), "不应判致命: {cmd}");
        }
    }

    /// 赋值前缀是环境设置,不能把整条命令吃掉(否则 `FOO=1 rm -rf /` 直接漏判)
    #[test]
    fn assignment_prefix_keeps_command() {
        assert!(deny("FOO=1 rm -rf /"));
        assert_eq!(
            argv_of("FOO=1 ls -l"),
            Some(vec!["ls".to_string(), "-l".to_string()])
        );
    }

    /// 含替换/展开的串不是字面量(不能只取前半个 string_content)
    #[test]
    fn interpolated_string_is_not_literal() {
        assert_eq!(
            argv_of(r#"echo "plain""#),
            Some(vec!["echo".to_string(), "plain".to_string()])
        );
        assert_eq!(argv_of(r#"echo "a${X}c""#), None);
    }

    /// 内层脚本要解包:短选项簇不能只认 `-c` / `-lc`
    #[test]
    fn shell_dash_c_is_unwrapped() {
        for cmd in [
            "bash -c 'rm -rf /'",
            "bash -lc 'rm -rf /'",
            "bash -cl 'rm -rf /'",
            "sh -euc 'rm -rf /'",
            "bash --norc -c 'rm -rf /'",
        ] {
            assert!(deny(cmd), "应判致命: {cmd}");
        }
        assert!(!deny("bash -c 'echo hi'"));
        assert!(!deny("bash script.sh"));
    }

    /// env 的选项各吃一个参数(`-u FOO` 不能把 FOO 当命令名)
    #[test]
    fn env_options_are_skipped() {
        for cmd in [
            "env -u FOO rm -rf /",
            "env -C /tmp rm -rf /",
            "env -S 'FOO=1' rm -rf /",
            "env -i FOO=1 rm -rf /",
            "env --unset=FOO rm -rf /",
        ] {
            assert!(deny(cmd), "应判致命: {cmd}");
        }
        assert_eq!(
            env_unwrap(&["env", "-u", "FOO", "ls"].map(String::from)),
            Some(vec!["ls".to_string()])
        );
    }

    /// dd 写裸盘致命,写 /dev/null 是正常用法
    #[test]
    fn dd_to_block_device_is_denied() {
        assert!(deny("dd if=/dev/zero of=/dev/sda"));
        assert!(deny("dd if=/dev/zero of=/dev/nvme0n1"));
        assert!(deny("dd if=/dev/zero of=/dev/mmcblk0"));
        assert!(!deny("dd if=/dev/zero of=/dev/null"));
    }

    /// 严格解析只放行纯只读构造:重定向/后台/替换一律非 plain
    #[test]
    fn allow_requires_plain_readonly() {
        assert!(check_allow("ls -l | cat"));
        assert!(check_allow("ls; pwd"));
        assert!(check_allow("FOO=1 ls"));
        assert!(!check_allow("ls > out.txt"));
        assert!(!check_allow("ls &"));
        assert!(!check_allow("ls $(id)"));
        assert!(!check_allow("ls `id`"));
        assert!(!check_allow(""));
    }
}
