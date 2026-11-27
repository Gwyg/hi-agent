//! 权限配置:bash 判定策略的**数据定义**与单例(匹配引擎与 grant 均在 matcher)。
//!
//! 仅用户策略 `[permissions.bash]` 进 app.toml;致命红线 / 危险 / 安全词表与 OnceAsk 基线
//! 都是 `#[cfg]` 分平台的代码内置默认,**不落盘**(否则 `save()` 会把内置默认钉进 app.toml,
//! 日后升级内置列表被旧副本盖掉 → 安全基线静默失效)。
//! 用户规则由 `init` 按 kind 合表(**只增不改**):deny/always_ask 并入内置表,allow/ask 留用户层。

use indexmap::IndexMap;
use serde::Deserialize;
use serde::Serialize;
use std::sync::{LazyLock, RwLock};

/// `app.toml` `[permissions]` 段:仅用户显式写的规则,不含内置默认
#[derive(Deserialize, Serialize, Clone, Default)]
pub struct PermissionsConfig {
    /// pattern → 处置(通配符匹配);`[permissions.bash]` 段
    #[serde(default)]
    pub bash: IndexMap<String, ActionKind>,
}

/// 运行时 bash 判定策略:四档内置默认(#[cfg] 分平台)+ 用户规则合并;检测层只从本对象取数
#[derive(Clone)]
pub struct BashPolicy {
    /// 用户规则(合表后只剩 allow/ask:deny/always_ask 已在 `init` 合入内置表);
    /// 空 = 未配置,纯走内置默认。allow/ask 不摊平进表——需全段合取 + 授权键 + grant,见 `check_user_policy`
    pub user_bash_rules: IndexMap<String, ActionKind>,
    /// 致命红线表(→ Deny):内置 + 用户 deny 合表(`init` 只增不改,用户删不掉内置项);
    /// 仅命令名即致命项,参数组合项(`rm -rf /` 等)由各平台 classify 的结构匹配器处理
    pub deny_rules: Vec<String>,
    /// 危险规则表(→ AskAlways):内置 + 用户 always_ask 合表(只增不改);
    /// 命令项前缀 / 语法项 `*>*` 子串,统一通配符匹配
    pub ask_always_rules: Vec<String>,
    /// 内置基线(→ AskOnce 可永久):危险但合法命令,默认 ask
    pub ask_once_rules: Vec<String>,
    /// 内置安全规则(→ Allow)
    pub allow_rules: Vec<String>,
    /// `allow_rules` 的伴随排除表(→ 首词不算内建命令):命中即从 allow 判定中排除,
    /// 防 `dir.bat` 影子内建 `dir`、`C:\build.bat` 被静默 Allow——执行文件须每次询问。
    /// 纯内置、不落盘、无对应 `ActionKind`(不是第五种处置,是 allow 的守卫);
    /// 与 allow 白名单同机制(glob + 大小写不敏感),判定在 matcher 的 `matches_allow_rules`
    pub file_ref_rules: Vec<String>,
}

impl Default for BashPolicy {
    fn default() -> Self {
        BashPolicy {
            user_bash_rules: IndexMap::new(),
            deny_rules: default_deny_rules(),
            ask_always_rules: default_ask_always_rules(),
            ask_once_rules: default_ask_once_rules(),
            allow_rules: default_allow_rules(),
            file_ref_rules: default_file_ref_rules(),
        }
    }
}

/// 配置层动作类型;app.toml 取值 "deny" / "allow" / "ask"(问一次可永久) / "always_ask"。
/// 运行时裁决用 `llm::tools::Action`,由 matcher 翻译。
///
/// 四种 kind 的落地位置(`init` 合表,只增不改):
/// - `deny` / `always_ask`:合入内置表,命中即停、无授权键;想「每次必问」用 `always_ask`
/// - `allow` / `ask`:留用户层逐段遍历;`ask` 命中记授权键(可「永久允许」),授权后该段按 allow 计
///
/// 用户 allow 压不过内置 deny/ask_always(安全天花板);多条用户规则命中同一段时先声明先生效
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ActionKind {
    Deny,
    Allow,
    Ask,
    AlwaysAsk,
}

/// 内置基线(OnceAsk 档,→ AskOnce):危险但合法命令,默认 ask;用户可用相同 pattern 在 `[permissions.bash]` 覆盖。
/// 跨平台共享删除 / 提权 / 版本控制 / 发布类。Windows 侧 `rm`/`rmdir` 是 Remove-Item 的别名,
/// 与 cmd 时代的 del/erase/rd 同档追加 `remove-item`/`ri`,同属可授予永久允许的删除类。
fn default_ask_once_rules() -> Vec<String> {
    let shared: &[&str] = &[
        "sudo *",
        "rm *",
        "rmdir *",
        "git push --force *",
        "git push -f *",
        "git reset --hard*",
        "npm publish*",
    ];
    let mut list: Vec<String> = shared.iter().map(|s| s.to_string()).collect();
    #[cfg(windows)]
    list.extend(
        ["remove-item *", "ri *", "del *", "erase *", "rd *"]
            .iter()
            .map(|s| s.to_string()),
    );
    list
}

/// 内置致命红线(→ Deny):仅「命令名即致命」简单项,务必保守;平台相关,不落盘。
/// Windows 侧按 PowerShell cmdlet 名匹配(解析器给的是字面命令名,前缀即可);
/// `format *` / `format.com*` 单独兜住 format.com——不能写 `format*`,那会连 Format-Table 一起 Deny。
fn default_deny_rules() -> Vec<String> {
    #[cfg(windows)]
    #[rustfmt::skip]
    let list: &[&str] = &[
        "format-volume*", "clear-disk*", "initialize-disk*", "remove-partition*",
        "stop-computer*", "restart-computer*", "diskpart*", "format *", "format.com*",
    ];
    #[cfg(unix)]
    #[rustfmt::skip]
    let list: &[&str] = &[
        "mkfs*", "mke2fs*",
        // 跨平台防御 powershell 调用
        "*format-volume*", "*clear-disk*", "*stop-computer*", "*restart-computer*",
    ];
    list.iter().map(|s| s.to_string()).collect()
}

/// 内置危险规则(→ AskAlways):命令项前缀 / 语法项子串,统一通配符匹配;平台相关,不落盘。
/// Windows 侧 remove-item/rm/rmdir 不在危险清单,改由基线管(→ OnceAsk 可授予永久允许),
/// 与 Unix 侧 rm 对齐;否则同危险度命令在 Windows 永远无法"不再询问"。
/// 命令名按 PowerShell 写法定:解析器不展开别名,故 cmdlet 与别名都要列(`copy*` 覆盖
/// Copy-Item 与 copy);`sc`/`net`/`reg`/`md` 不能写成前缀——`sc*` 会连 Select-Object 一起
/// 命中、`md*` 会命中 Measure-Object,故带尾空格只匹配带参数的真实调用。
fn default_ask_always_rules() -> Vec<String> {
    #[cfg(windows)]
    #[rustfmt::skip]
    let list: &[&str] = &[
        // 复制 / 移动 / 建目录
        "copy*", "xcopy*", "robocopy*", "move*", "ren*", "rename*", "mkdir*", "md *",
        // 网络与下载
        "curl*", "wget*", "invoke-webrequest*", "iwr*",
        // 进程 / 注册表 / 服务 / 系统
        "shutdown*", "taskkill*", "stop-process*", "kill*", "spps*", "reg *", "reg.exe*",
        "regedit*", "runas*", "net *", "net.exe*", "sc *", "sc.exe*", "wmic*", "bcdedit*",
        "takeown*", "icacls*", "attrib*", "powershell*", "pwsh*", "cmd*", "wsl*", "bash*",
        "invoke-expression*", "iex*", "invoke-command*", "icm*", "set-executionpolicy*",
        "start-process*", "saps*", "start-job*", "new-service*", "set-service*",
        "stop-service*", "restart-service*", "remove-service*",
        // 危险语法(子串匹配);`$` 不列——PowerShell 里变量随处可见,列上等于全量拦截
        "*>*",
    ];
    #[cfg(unix)]
    // rm/rmdir/sudo 不在此表:与基线重叠会被判 AlwaysAsk、grant 失效,故由基线管。
    #[rustfmt::skip]
    let list: &[&str] = &[
        "dd*", "curl*", "wget*", "chmod*", "chown*", "kill*", "killall*", "pkill*",
        "shutdown*", "reboot*", "halt*", "poweroff*", "su*", "shred*",
    ];
    list.iter().map(|s| s.to_string()).collect()
}

/// 内置安全命令(→ Allow);平台相关,不落盘;条目用户删不掉,但配 ask 可改判 AskOnce(格B)。
/// Windows 侧按 PowerShell 命名字面量枚举只读 cmdlet 与常见别名——**不用 `get-*` 通配**:
/// 解析器只给出键入的名字,`get-foo` 可能是 PATH 里的脚本,通配会把影子脚本一起放行。
///
/// 收录门槛:只读或只作用于本次 shell 子进程,且不是「套一层就能执行任意命令」的包装器。
/// 故意不收:`sc`(Set-Content)/`tee`(Tee-Object) 会写盘、`irm`(Invoke-RestMethod) 走网络、
/// `?` 在 glob 里是单字符通配符(写进词表会放行任意单字符命令名);
/// `sed`/`awk`/`env`/`command`/`xargs` 同理,首词静态匹配看不出它们带的写操作或内层命令。
/// macOS 与 Linux 共用 `#[cfg(unix)]` 分支,没有 `target_os = "macos"` 专属词表,
/// 故 macOS 原生只读命令也收在这一份里;GNU 名在 macOS 上不存在不代表该命令被放行。
fn default_allow_rules() -> Vec<String> {
    #[cfg(windows)]
    #[rustfmt::skip]
    let list: &[&str] = &[
        // 读类 cmdlet
        "get-childitem", "get-content", "get-item", "get-itemproperty", "get-location",
        "get-command", "get-help", "get-member", "get-date", "get-host", "get-process",
        "get-service", "get-psdrive", "get-module", "get-variable", "get-history",
        "get-alias", "get-verb", "get-random", "select-object", "select-string",
        "where-object", "sort-object", "group-object", "measure-object", "compare-object",
        "format-table", "format-list", "format-wide", "format-custom", "format-hex",
        "out-host", "out-string", "out-null", "write-output", "write-host",
        "write-information", "test-path", "resolve-path", "split-path", "join-path",
        "convert-path", "convertto-json", "convertfrom-json", "convertto-csv", "convertfrom-csv",
        // 别名与外部只读命令
        "dir", "ls", "cat", "type", "echo", "where", "sort", "more", "findstr", "find",
        "tree", "whoami", "hostname", "pwd", "cls", "clear-host", "clear",
        // 位置切换:执行侧每次 spawn 都 .current_dir(),只改本次子进程;与 Unix 侧 cd 对齐
        "set-location", "cd", "chdir", "sl", "push-location", "pop-location", "pushd", "popd",
        // 管道遍历:脚本块里的 CommandAst 由 ParseInput 递归拆出逐段判定,这里只认外层命令名。
        // cmdlet 是 ForEach-Object(驼峰 → foreach-object),不是 for-each-object
        "foreach-object", "foreach", "%", "get-unique",
        // PowerShell 内置别名(Get-Alias 真机核对):解析器不展开别名,按字面量补一份
        "gci", "gc", "gi", "gp", "gl", "gv", "gal", "gm", "gcm", "gps", "gsv", "gdr",
        "gmo", "select", "group", "compare", "diff", "measure", "sls", "ft", "fl", "fw",
        "h", "history",
    ];
    #[cfg(unix)]
    #[rustfmt::skip]
    let list: &[&str] = &[
        // 基础只读
        "ls", "cat", "grep", "pwd", "echo", "head", "tail", "wc", "which", "whoami",
        "date", "uname", "stat", "file", "cd", "cut", "expr", "false", "true", "id",
        "nl", "paste", "rev", "seq", "tr", "uniq",
        // 纯读取 / 纯输出,严格 AST 已挡掉重定向、命令替换与后台执行;
        // 不收 sed/awk(可写盘或执行命令)、env/command/xargs(包装器)、rg(--pre 会起子进程)
        "printf", "basename", "dirname", "realpath", "readlink", "diff", "comm", "join",
        "fold", "fmt", "column", "expand", "unexpand", "tac", "od", "hexdump", "strings",
        "printenv", "du", "df", "ps", "free", "uptime", "locale", "cksum", "md5sum",
        "sha1sum", "sha256sum", "test",
        // macOS / BSD 原生只读命令。GNU 名(md5sum/sha256sum/tac/free)在 macOS 上不存在,
        // 对应的是 md5 / shasum / tail -r / vm_stat;两边都按字面收,缺哪个都不影响另一平台。
        // 故意不收:open / osascript(启动程序、执行脚本)、defaults / launchctl /
        // diskutil / networksetup / brew / installer(改配置或装包)、security(钥匙串)、
        // pbcopy / screencapture(写)、sysctl / plutil / xcode-select(带写子命令)、
        // top(交互式)、ping / dig(主动网络探测)
        "sw_vers", "mdls", "mdfind", "shasum", "md5", "hostname", "who", "w", "last",
        "users", "groups", "cal", "ncal", "bc", "lsof", "netstat", "vm_stat",
    ];
    list.iter().map(|s| s.to_string()).collect()
}

/// `allow_rules` 的伴随排除表(→ 不算内建命令):纯内置,不落盘,用户配不了也不需配
/// (`[permissions.bash]` 没有对应 kind)。两平台通用——`*/*` 覆盖两种路径分隔符
/// (globset 视 `/` 与 `\` 皆为分隔符),扩展名在 Unix 上同样该拒(`ls.bat` 不是内建命令)
fn default_file_ref_rules() -> Vec<String> {
    ["*/*", "*.bat", "*.cmd", "*.exe", "*.ps1"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

// ============ 运行时全局单例 ============
// 与 llm / path_scope 同一模式:策略值单一真源在 config,bash_safety 只读取

static BASH_POLICY: LazyLock<RwLock<BashPolicy>> =
    LazyLock::new(|| RwLock::new(BashPolicy::default()));

/// 启动注入:重置为内置默认后合入用户规则(**幂等**:重复 init 不累积)。
/// 合表规则见 `merge_user_rules`;预编译由 matcher 的 LazyLock 负责——
/// 「init 先于首次 classify」的启动顺序契约不变
pub fn init(config: &PermissionsConfig) {
    if let Ok(mut g) = BASH_POLICY.write() {
        *g = BashPolicy::default();
        merge_user_rules(&mut g, &config.bash);
    }
}

/// 把用户 `[permissions.bash]` 规则合入策略(**只增不改**,内置条目一条不少):
/// - `deny` / `always_ask`:命中即停、无跨段状态 → 追加进内置表后从 map 移除,
///   与内置 pattern 同表同机制(预查层天然覆盖;复合段也不会被「未命中即弃权」丢弃)
/// - `allow` / `ask`:需全段合取 + 授权键收集 + grant 降级 → 留 `user_bash_rules`,
///   由 `check_user_policy` 逐段遍历(匹配机制不同,不摊平进表)
fn merge_user_rules(policy: &mut BashPolicy, user_rules: &IndexMap<String, ActionKind>) {
    let mut rest: IndexMap<String, ActionKind> = IndexMap::new();
    for (pattern, kind) in user_rules {
        match kind {
            ActionKind::Deny => policy.deny_rules.push(pattern.clone()),
            ActionKind::AlwaysAsk => policy.ask_always_rules.push(pattern.clone()),
            ActionKind::Allow | ActionKind::Ask => {
                rest.insert(pattern.clone(), kind.clone());
            }
        }
    }
    policy.user_bash_rules = rest;
}

/// 读取当前 bash 判定策略(访问器;锁中毒回退内置默认)。
pub fn bash_policy() -> BashPolicy {
    BASH_POLICY.read().map(|g| g.clone()).unwrap_or_default()
}

// ============ 测试 ============

#[cfg(test)]
mod tests {
    use super::*;

    fn user_map(entries: &[(&str, ActionKind)]) -> IndexMap<String, ActionKind> {
        entries
            .iter()
            .map(|(p, k)| (p.to_string(), k.clone()))
            .collect()
    }

    /// 合表按 kind 分发:deny→deny_rules、always_ask→ask_always_rules(只增不改),
    /// allow/ask 留 user_bash_rules(匹配机制不同,不摊平进表)
    #[test]
    fn merge_dispatches_user_rules_by_kind() {
        let mut policy = BashPolicy::default();
        let (builtin_deny, builtin_ask_always) =
            (policy.deny_rules.len(), policy.ask_always_rules.len());
        merge_user_rules(
            &mut policy,
            &user_map(&[
                ("foo *", ActionKind::Deny),
                ("bar *", ActionKind::AlwaysAsk),
                ("baz *", ActionKind::Allow),
                ("qux *", ActionKind::Ask),
            ]),
        );
        assert_eq!(policy.deny_rules.len(), builtin_deny + 1);
        assert!(policy.deny_rules.iter().any(|p| p == "foo *"));
        assert_eq!(policy.ask_always_rules.len(), builtin_ask_always + 1);
        assert!(policy.ask_always_rules.iter().any(|p| p == "bar *"));
        // 用户层只剩 allow/ask
        assert_eq!(policy.user_bash_rules.len(), 2);
        assert!(policy.user_bash_rules.contains_key("baz *"));
        assert!(policy.user_bash_rules.contains_key("qux *"));
    }

    /// 只增不改:内置条目一条不少(用户规则删不掉、改不了内置基线)
    #[test]
    fn merge_is_additive_only() {
        let mut policy = BashPolicy::default();
        let before = policy.deny_rules.clone();
        merge_user_rules(&mut policy, &user_map(&[("foo *", ActionKind::Deny)]));
        assert!(policy.deny_rules.starts_with(&before));
        assert_eq!(policy.deny_rules.len(), before.len() + 1);
    }

    /// init 幂等:同一份配置重复注入不累积(先重置默认再合表);
    /// 收尾复原全局单例,避免污染同进程其他用例
    #[test]
    fn init_is_idempotent() {
        let config = PermissionsConfig {
            bash: user_map(&[("foo *", ActionKind::Deny), ("bar *", ActionKind::Allow)]),
        };
        init(&config);
        let first = bash_policy();
        init(&config);
        let second = bash_policy();
        assert_eq!(first.deny_rules, second.deny_rules);
        assert_eq!(first.user_bash_rules, second.user_bash_rules);
        assert!(second.deny_rules.iter().any(|p| p == "foo *"));
        assert!(second.user_bash_rules.contains_key("bar *"));
        // 复原:全局单例是进程级状态
        init(&PermissionsConfig::default());
    }
}
