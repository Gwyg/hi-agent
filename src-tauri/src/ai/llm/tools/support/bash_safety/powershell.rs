//! Windows 命令安全分类:拆命令交给 PowerShell 自己的解析器(`Parser::ParseInput`,
//! 纯语法分析、不执行),裁决交给 `matcher` 的四档规则。
//!
//! 为什么不手写:PowerShell 有子表达式、脚本块、splatting、别名、`&`/`.` 调用运算符,
//! 手写必然留缺口。而「命令名是不是字面量」是解析器白送的判据——`& ('Remove'+'-Item')`
//! 的 `GetCommandName()` 返回 null,字符串启发式永远看不到这一点。
//!
//! 代价:每次鉴权起一个进程(约 0.4s),所以 `Tool::assess` 是 async。解析失败(超时、
//! 退出非 0、输出不是 JSON)一律 AskAlways——无法证明安全就不替用户担保。

use super::{
    check_user_policy, matches_allow_rules, matches_ask_always_rules, matches_deny_rules,
    max_action, DENY_REASON,
};
use crate::ai::llm::tools::support::shell;
use crate::ai::llm::tools::Action;
use serde::Deserialize;
use std::process::Stdio;
use std::time::Duration;

/// 解析脚本:只做语法分析,输出每条命令的名字、元素原文与文件重定向。
/// 待分析的命令走环境变量,不与脚本本身抢引号层。
///
/// **脚本内不能出现双引号**:它作为 argv 交给 `-Command` 会被 MSVC 转义,
/// 双引号会变成 PowerShell 不认识的形态。全用单引号/对象字面量。
const PARSE_SCRIPT: &str = r#"[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$c = $env:HI_AGENT_CMD
$e = $null; $t = $null
$a = [System.Management.Automation.Language.Parser]::ParseInput($c, [ref]$t, [ref]$e)
$cmds = @($a.FindAll({ param($n) $n -is [System.Management.Automation.Language.CommandAst] }, $true) | ForEach-Object {
  [pscustomobject]@{ name = $_.GetCommandName(); elements = @($_.CommandElements | ForEach-Object { $_.Extent.Text }); redirects = @($_.Redirections | Where-Object { $_ -is [System.Management.Automation.Language.FileRedirectionAst] } | ForEach-Object { $_.Extent.Text }) }
})
[pscustomobject]@{ errors = @($e | ForEach-Object { $_.Message }); commands = $cmds } | ConvertTo-Json -Compress -Depth 6"#;

/// 解析超时:纯语法分析正常在 1s 内;到点说明环境异常(杀软/盘慢),按无法证明安全处理
const PARSE_TIMEOUT: Duration = Duration::from_secs(10);

/// Remove-Item 及其别名。解析器**不展开别名**(写 `rm` 就给 `rm`),故按写法比
const REMOVE_NAMES: &[&str] = &["remove-item", "rm", "ri", "del", "erase", "rd", "rmdir"];

/// 从 PowerShell 里再开一层 shell:内层语法不归本分类器管,只对「绝无正当用途」的串
/// 做硬红线——尽力而为,不假装做到全量分析(那要递归解析各内层方言)
const SHELL_WRAPPERS: &[&str] = &["cmd", "powershell", "pwsh", "wsl", "bash", "sh"];
const FATAL_INNER: &[&str] = &[
    "format-volume",
    "clear-disk",
    "initialize-disk",
    "stop-computer",
    "restart-computer",
    "diskpart",
    "format c:",
];

/// 解析器输出(与 PARSE_SCRIPT 的 JSON 结构一一对应)
#[derive(Debug, Deserialize)]
struct Parsed {
    #[serde(default)]
    errors: Vec<String>,
    #[serde(default)]
    commands: Vec<ParsedCommand>,
}

#[derive(Debug, Deserialize)]
struct ParsedCommand {
    /// 字面命令名;动态构造(`& (...)`、变量、表达式)时为 null
    name: Option<String>,
    #[serde(default)]
    elements: Vec<String>,
    /// 文件重定向原文(`> out` / `2> err`):不在 CommandElements 里,单独取;
    /// `2>&1` 之类合流不算(脚本只挑 FileRedirectionAst)
    #[serde(default)]
    redirects: Vec<String>,
}

/// 一个命令段:解析器给出的事实 + 词表需要的字符串形状
struct Seg {
    /// 首词(小写)
    name: String,
    /// 其余元素(去外层引号、小写),供参数结构匹配
    args: Vec<String>,
    /// 段文本 = 元素以空格拼回,供 glob 词表(deny/always_ask/用户规则)匹配
    text: String,
}

/// 元素归一:去空白 → 去外层引号 → 小写。
///
/// `Extent.Text` 是原始切片,引号还在(`'C:\'` 就是 `'C:\'`)。只去双引号会让
/// `Remove-Item -Recurse 'C:\'` 失配 `is_system_path`,Deny 降级成可授权的 AskOnce。
fn normalize(elem: &str) -> String {
    elem.trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .to_ascii_lowercase()
}

impl From<&ParsedCommand> for Seg {
    fn from(c: &ParsedCommand) -> Self {
        // 元素只归一一次:args 与 text 都从这份派生,不各扫一遍
        let elements: Vec<String> = c.elements.iter().map(|e| normalize(e)).collect();
        Seg {
            name: normalize(c.name.as_deref().unwrap_or_default()),
            args: elements.iter().skip(1).cloned().collect(),
            text: elements.join(" "),
        }
    }
}

// ============ 分类入口 ============

/// 命令分类入口:解析 → 四档裁决。解析不出结构一律 AskAlways。
pub async fn classify(command: &str) -> Action {
    match parse(command).await {
        Some(parsed) => decide(&parsed),
        None => Action::AskAlways,
    }
}

/// 起解析器拿 AST。失败一律 `None`(调用方按无法证明安全处理),不抛错——
/// 单条命令鉴权失败不该中断整轮对话,只该让它降级为问人
async fn parse(command: &str) -> Option<Parsed> {
    let spec = shell::current();
    // 第一层:整体超时(外层是 Elapsed,内层是 io)
    let out = tokio::time::timeout(
        PARSE_TIMEOUT,
        tokio::process::Command::new(spec.exe)
            .args(spec.flags)
            .arg(PARSE_SCRIPT)
            .env("HI_AGENT_CMD", command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // 超时后 future 被 drop:tokio 默认不杀子进程,只是我们不再等它的输出。
            // 显式打开,让「超时」等于「终止这个解析器」而不是「留它在后台跑完」
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?;
    // 第二层:起进程或收集输出失败
    let out = out.ok()?;
    if !out.status.success() {
        tracing::warn!(
            code = out.status.code(),
            "命令解析器退出非 0,按无法证明安全处理"
        );
        return None;
    }
    match serde_json::from_slice::<Parsed>(&out.stdout) {
        Ok(parsed) => Some(parsed),
        Err(e) => {
            tracing::warn!("命令解析器输出不可解析: {e}");
            None
        }
    }
}

/// 结构上无法分析:这三种情况都拿不到可信的「命令名 + 实参」,
/// 后面的词表/白名单判断全都会落空 → 一律不担保
fn unanalyzable(parsed: &Parsed) -> bool {
    // 语法错误(5.1 下的 `&&` 就落这里)或空命令
    if !parsed.errors.is_empty() || parsed.commands.is_empty() {
        return true;
    }
    // 命令名不是字面量:静态分析到此为止(`& ('Remove' + '-Item')`)
    parsed
        .commands
        .iter()
        .any(|c| c.name.as_deref().is_none_or(|n| n.trim().is_empty()))
}

/// 纯裁决:解析结果 → 四档(不碰进程,单测可直接构造事实)
fn decide(parsed: &Parsed) -> Action {
    if unanalyzable(parsed) {
        return Action::AskAlways;
    }

    let segs: Vec<Seg> = parsed.commands.iter().map(Seg::from).collect();

    // Deny 必须最先:它不可覆盖,放到重定向之后会被降级成可批准的 AskAlways
    if let Some(reason) = segs.iter().find_map(deny_reason) {
        return Action::Deny(reason);
    }

    // 三条「不可放行」信号同档(Deny 已判完,它们只会把结论钉在 AskAlways),命中即返回:
    // 后面的用户策略与白名单已无翻案余地,不必再跑。
    // ① 文件重定向会写盘 ② 危险词表 ③ 包装器段(内层脚本不可证)
    if parsed.commands.iter().any(|c| !c.redirects.is_empty())
        || segs.iter().any(|s| matches_ask_always_rules(&s.text))
        || segs
            .iter()
            .any(|s| SHELL_WRAPPERS.contains(&s.name.as_str()))
    {
        return Action::AskAlways;
    }

    // 余下只可能 Allow / AskOnce,按严重度取最大
    let texts: Vec<String> = segs.iter().map(|s| s.text.clone()).collect();
    let mut action = check_user_policy(&texts);
    // Allow 是合取:一段未知不能因兄弟段安全被掩盖
    if segs.iter().all(|s| matches_allow_rules(&s.name)) {
        action = max_action(action, Action::Allow);
    }
    action.unwrap_or(Action::AskAlways)
}

// ============ 致命检测(不可覆盖 Deny) ============

fn deny_reason(seg: &Seg) -> Option<String> {
    if recursive_system_remove(seg) {
        return Some("递归删除系统路径".into());
    }
    if matches_deny_rules(&seg.text) {
        return Some(DENY_REASON.into());
    }
    // seg.text 已归一为小写(见 Seg::from),FATAL_INNER 全是小写字面量,直接比
    if SHELL_WRAPPERS.contains(&seg.name.as_str())
        && FATAL_INNER.iter().any(|f| seg.text.contains(f))
    {
        return Some(DENY_REASON.into());
    }
    None
}

/// 递归删除系统路径:参数组合,pattern 表达不了,保留结构匹配(与 cmd 版的 rd /s 同档)
fn recursive_system_remove(seg: &Seg) -> bool {
    if !REMOVE_NAMES.contains(&seg.name.as_str()) {
        return false;
    }
    // PowerShell 参数支持前缀缩写:`-Recurse` 可写 `-r` / `-rec`
    let recursive = seg.args.iter().any(|a| a == "-r" || a.starts_with("-rec"));
    recursive && seg.args.iter().any(|a| is_system_path(a))
}

/// 系统路径:字面量与 PowerShell/cmd 的环境变量写法都收(`cmd /c` 里的 `%VAR%`
/// 是同类灾难)。只列「整根」,不写通配前缀——否则 `C:\Users\...\node_modules`
/// 这种正常清理也会被误判成致命
fn is_system_path(arg: &str) -> bool {
    matches!(
        arg,
        "c:\\"
            | "c:/"
            | "c:\\*"
            | "c:/*"
            | "$env:systemroot"
            | "$env:windir"
            | "$env:userprofile"
            | "%systemroot%"
            | "%windir%"
            | "%userprofile%"
    ) || arg.starts_with("c:\\windows")
        || arg.starts_with("c:/windows")
        || arg.starts_with("$env:systemroot")
        || arg.starts_with("$env:windir")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::llm::tools::support::bash_safety::{record_grant, with_isolated_grants};

    fn pc(name: Option<&str>, elements: &[&str]) -> ParsedCommand {
        ParsedCommand {
            name: name.map(str::to_string),
            elements: elements.iter().map(|e| e.to_string()).collect(),
            redirects: Vec::new(),
        }
    }

    /// 单条命令(可执行形态)的裁决
    fn one(name: &str, elements: &[&str]) -> Action {
        decide(&Parsed {
            errors: Vec::new(),
            commands: vec![pc(Some(name), elements)],
        })
    }

    #[test]
    fn literal_commands_follow_the_word_lists() {
        // 读类 cmdlet → Allow
        assert_eq!(
            one("Get-ChildItem", &["Get-ChildItem", "-Name"]),
            Action::Allow
        );
        assert_eq!(one("Format-Table", &["Format-Table"]), Action::Allow);
        // 未知程序 → 兜底 AskAlways
        assert_eq!(one("foo.exe", &["foo.exe"]), Action::AskAlways);
        // 顶层 shell 调用 → AskAlways(内层不归这里管,但绝不静默放行)
        assert_eq!(
            one("pwsh", &["pwsh", "-Command", "Get-Date"]),
            Action::AskAlways
        );
    }

    /// Allow 是合取:一段安全不能掩盖另一段未知
    #[test]
    fn allow_is_conjunctive_across_pipeline() {
        let parsed = Parsed {
            errors: Vec::new(),
            commands: vec![
                pc(Some("Get-ChildItem"), &["Get-ChildItem"]),
                pc(Some("Select-Object"), &["Select-Object", "-First", "3"]),
            ],
        };
        assert_eq!(decide(&parsed), Action::Allow);

        let parsed = Parsed {
            errors: Vec::new(),
            commands: vec![
                pc(Some("Get-ChildItem"), &["Get-ChildItem"]),
                pc(Some("foo.exe"), &["foo.exe"]),
            ],
        };
        assert_eq!(decide(&parsed), Action::AskAlways);
    }

    /// 动态命令名是解析器贡献的关键判据:字符串启发式看不见,这里必须落到「问人」
    #[test]
    fn dynamic_command_name_asks_always() {
        let parsed = Parsed {
            errors: Vec::new(),
            commands: vec![pc(None, &["('Remove' + '-Item')", "-Recurse", "C:\\x"])],
        };
        assert_eq!(decide(&parsed), Action::AskAlways);
    }

    /// 文件重定向不在 CommandElements 里,单独取:只读命令 + 重定向不能判 Allow
    /// (否则写路径绕过只读白名单);`2>&1` 是合流不是写盘,不拦
    #[test]
    fn file_redirection_asks_always() {
        let parsed = Parsed {
            errors: Vec::new(),
            commands: vec![ParsedCommand {
                name: Some("Get-ChildItem".into()),
                elements: vec!["Get-ChildItem".into()],
                redirects: vec!["> out.txt".into()],
            }],
        };
        assert_eq!(decide(&parsed), Action::AskAlways);
        assert_eq!(
            decide(&Parsed {
                errors: Vec::new(),
                commands: vec![ParsedCommand {
                    name: Some("Get-ChildItem".into()),
                    elements: vec!["Get-ChildItem".into()],
                    redirects: Vec::new(),
                }],
            }),
            Action::Allow
        );
    }

    /// 语法错误(含 5.1 的 `&&`)与空命令:不认识就不担保
    #[test]
    fn parse_errors_and_empty_ask_always() {
        assert_eq!(
            decide(&Parsed {
                errors: vec!["The token '&&' is not a valid statement separator".into()],
                commands: vec![pc(Some("Get-ChildItem"), &["Get-ChildItem"])],
            }),
            Action::AskAlways
        );
        assert_eq!(
            decide(&Parsed {
                errors: Vec::new(),
                commands: Vec::new(),
            }),
            Action::AskAlways
        );
    }

    /// 致命红线:Format-Volume 类 cmdlet 与「递归删系统根」不可覆盖
    #[test]
    fn fatal_commands_are_denied() {
        assert_eq!(
            one("Format-Volume", &["Format-Volume", "-DriveLetter", "D"]),
            Action::Deny(DENY_REASON.into())
        );
        for (name, args) in [
            (
                "Remove-Item",
                vec!["Remove-Item", "-Recurse", "-Force", "C:\\"],
            ),
            ("rm", vec!["rm", "-r", "$env:SystemRoot"]),
            ("del", vec!["del", "-Recurse", "C:\\Windows"]),
        ] {
            assert!(
                matches!(one(name, &args), Action::Deny(_)),
                "{name} {args:?} 应判 Deny"
            );
        }
    }

    /// 重定向不能盖过 Deny:致命命令 + 写盘仍是「拒绝」,不是「问一下就能放行」。
    /// (回归:redirects 曾在前置检查里 early return AskAlways,把 Deny 降级成可批准)
    #[test]
    fn redirection_does_not_downgrade_deny() {
        let parsed = Parsed {
            errors: Vec::new(),
            commands: vec![ParsedCommand {
                name: Some("Format-Volume".into()),
                elements: vec!["Format-Volume".into(), "-DriveLetter".into(), "D".into()],
                redirects: vec!["> log.txt".into()],
            }],
        };
        assert_eq!(decide(&parsed), Action::Deny(DENY_REASON.into()));
    }

    /// 实参的引号必须先剥掉再判系统路径:解析器的 `Extent.Text` 保留原始引号,
    /// 只去双引号会让单引号写法绕过 `is_system_path`,Deny 降级为可授权的 AskOnce
    #[test]
    fn quoted_system_path_is_still_denied() {
        // 单引号 / 双引号 / 无引号三种写法等价,都必须是 Deny
        for quoted in ["'C:\\'", "\"C:\\\"", "C:\\"] {
            assert!(
                matches!(
                    one("Remove-Item", &["Remove-Item", "-Recurse", quoted]),
                    Action::Deny(_)
                ),
                "Remove-Item -Recurse {quoted} 应判 Deny"
            );
        }
        // 环境变量写法同理
        assert!(matches!(
            one("rm", &["rm", "-r", "'$env:SystemRoot'"]),
            Action::Deny(_)
        ));
    }

    /// 普通删除不致命,走基线 → AskOnce 可授权;系统路径之外的 recurse 也是 AskOnce
    #[test]
    fn normal_remove_is_ask_once_not_deny() {
        let _fx = with_isolated_grants();
        assert_eq!(
            one("Remove-Item", &["Remove-Item", "build"]),
            Action::AskOnce {
                keys: vec!["remove-item *".to_string()]
            }
        );
        assert_eq!(
            one(
                "Remove-Item",
                &["Remove-Item", "-Recurse", "-Force", "C:\\proj\\target"]
            ),
            Action::AskOnce {
                keys: vec!["remove-item *".to_string()]
            }
        );
    }

    /// 从 PowerShell 再开一层 shell:内层含致命串时硬拒,普通调用只到 AskAlways
    #[test]
    fn nested_shell_fatal_inner_is_denied() {
        assert!(matches!(
            one("cmd", &["cmd", "/c", "format C:"]),
            Action::Deny(_)
        ));
        assert_eq!(one("cmd", &["cmd", "/c", "dir"]), Action::AskAlways);
    }

    /// 端到端:真起 PowerShell 解析器,钉住「脚本 → 环境变量 → JSON → 裁决」整条链路
    #[tokio::test]
    async fn parser_pipeline_end_to_end() {
        let _fx = with_isolated_grants();
        assert_eq!(classify("Get-ChildItem -Name").await, Action::Allow);
        assert_eq!(
            classify("Get-ChildItem | Select-Object -First 3").await,
            Action::Allow
        );
        assert_eq!(
            classify("Remove-Item build").await,
            Action::AskOnce {
                keys: vec!["remove-item *".to_string()]
            }
        );
        // 动态拼接:只有 AST 能看出来
        assert_eq!(
            classify("& ('Remove' + '-Item') -Recurse C:\\x").await,
            Action::AskAlways
        );
        // 文件重定向写成问人,合流(2>&1)不影响只读判定
        assert_eq!(classify("Get-ChildItem > out.txt").await, Action::AskAlways);
        assert_eq!(classify("Get-ChildItem 2>&1").await, Action::Allow);
        // 多行脚本在 PowerShell 里是合法语句序列,两条命令都要参与判定
        assert_eq!(
            classify("Get-ChildItem\nRemove-Item build").await,
            Action::AskOnce {
                keys: vec!["remove-item *".to_string()]
            }
        );
    }

    /// 白名单补全回归:位置切换 + 管道遍历 + 脚本块内层的真实命令形态,
    /// 补进 `cd` / `foreach-object` / 只读别名后整条 Allow(判定链一行未改)。
    #[tokio::test]
    async fn readonly_cwd_and_pipeline_is_allowed() {
        let _fx = with_isolated_grants();
        for name in [
            "cd",
            "Get-ChildItem",
            "ForEach-Object",
            "Get-Location",
            "echo",
        ] {
            assert_eq!(one(name, &[name]), Action::Allow, "单段 {name} 应放行");
        }
        let cmd = "cd 'C:\\'; Get-ChildItem -Name | ForEach-Object { $_.Replace((Get-Location).Path, '') }; echo '--- UI DIR ---'";
        assert_eq!(classify(cmd).await, Action::Allow);
    }

    /// 故意不收的别名仍须问人:写盘 / 网络 / glob 单字符通配符,防日后「顺手补全」踩坑
    #[test]
    fn non_readonly_aliases_still_ask_always() {
        for name in ["sc", "tee", "irm", "?"] {
            assert_eq!(
                one(name, &[name]),
                Action::AskAlways,
                "{name} 不应被静默放行"
            );
        }
    }

    /// 授权降级端到端:记录的 pattern 被该段命中后,AskOnce 段按 Allow 计
    #[tokio::test]
    async fn grant_downgrades_ask_once_to_allow() {
        let _fx = with_isolated_grants();
        record_grant("remove-item *");
        assert_eq!(classify("Remove-Item build").await, Action::Allow);
    }
}
