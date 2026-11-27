use async_trait::async_trait;
use serde::Deserialize;
use std::collections::VecDeque;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};

use super::support::bash_safety;
use super::support::path_scope::workspace_root;
use super::support::process_tree::{self, ProcessTree};
use super::support::shell;
use super::{Action, Tool};
use crate::ai::llm::types::ToolDef;
use crate::util::resolve_lexical;

/// 执行 shell 命令(有副作用,高危,万能兜底)
/// 支持超时和工作目录,返回退出码+stdout+stderr
pub struct BashTool;

// shell 的启动规格不在这里定:`support::shell` 是唯一真源(执行与安全解析必须同一个壳)。
// PowerShell 的 `-Command` 是把整段脚本文本收下再自己解析,因此引号与换行都是它的内建语义
// (引号不经过 argv 层转义、换行是合法语句分隔符)——这也是选它而不是 cmd 的直接原因。

/// 默认超时(秒)
const DEFAULT_TIMEOUT: u64 = 300;
/// 超时上限(秒):不设上限时模型传个大数就等于没有超时,命令会一直挂到进程结束
const MAX_TIMEOUT: u64 = 3600;
/// 单流(stdout/stderr)保留的头尾字节数,防海量输出撑爆 context。两路各一份,
/// 合计峰值 64KB。命令的结论几乎都在尾部(测试失败摘要、curl 响应、编译器最后那句
/// error),所以头部只留够看命令开头,大头让给尾部。头尾之和即单流保留总量,改时需同步
const OUTPUT_HEAD: usize = 4 * 1024;
const OUTPUT_TAIL: usize = 28 * 1024;
/// 单次读取的块大小。尾部队列按 `tail + 一块` 预留容量,正好保证读完一整块也不扩容
const READ_BUF: usize = 8 * 1024;

#[derive(Deserialize)]
struct Args {
    command: String,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    cwd: Option<String>,
}

impl BashTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn definition(&self) -> ToolDef {
        // 用 cfg! 在 description 注入当前编译目标平台,告知模型该生成哪种语法的命令
        let platform = if cfg!(target_os = "windows") {
            "Windows (PowerShell;优先 pwsh 7,回退系统自带的 Windows PowerShell 5.1)。命令用 PowerShell 语法(Get-ChildItem/Get-Content/Select-String,$env:VAR 取环境变量,多条命令用 ; 或换行分隔;5.1 不支持 &&)"
        } else if cfg!(target_os = "macos") {
            "macOS (POSIX sh)。命令用 Unix 语法(ls/cat/grep,$VAR 取环境变量)"
        } else {
            "Unix-like (POSIX sh)。命令用 Unix 语法(ls/cat/grep,$VAR 取环境变量)"
        };

        ToolDef::function(
            "bash",
            format!(
                "执行 shell 命令。有副作用,高风险,万能兜底。\n\
                当前系统: {platform}\n\
                适用:跑测试/构建/脚本;装依赖;git 操作;调 CLI 工具(rg/gh/npm);看命令输出(ls/ps/date);网络请求(curl)。\n\
                不适用:读文件内容用 read(更结构化);改文件用 edit/write(更安全可控);搜代码用 search_files(无副作用)。\n\
                优先用专用工具(read/write/edit/search_files),它们更安全、输出更结构化、权限更好配。bash 是兜底,专用工具做不到时再用。\n\
                返回 exit_code+stdout+stderr,每路不超 {}KB 就完整返回;超了只留前 {}KB 和后 {}KB\
                 (中间省略并标注省略字节数)。所以要看完整输出请用 head/tail/Select-Object 先筛,\
                 别让命令把全部输出吐出来。\n\
                超时默认 {DEFAULT_TIMEOUT}s,可传 timeout 调整(1~{MAX_TIMEOUT}s),超时连同子进程树一起杀。\
                 exit_code 非 0 是命令本身的结果,自行判断。\n\
                注意:命令派生后台进程时(Unix `&`;PowerShell Start-Job / Start-Process),它们的输出会和主命令\
                 混进同一条 stdio 并共用上面那点额度;这类进程要自行重定向到文件或空设备\
                 (Unix /dev/null、PowerShell $null),否则真实结果可能被日志挤掉。\n\
                注意:bash 只跑前台命令,命令退出即清理整棵进程树,常驻服务(dev server、`-d` 启动的容器)\
                 活不过本次调用;要临时起服务再探测,把「启动 → 等待 → 探测」放进同一条命令,探测必须在退出前完成。\n\
                注意:exit_code 是 PowerShell 自身的退出码,原生命令的非 0 多半会被归一成 1;需要真实退出码就在命令末尾写 `; exit $LASTEXITCODE`。\n\
                注意:PowerShell 5.1 向原生命令传参时会吞掉内嵌双引号(python -c / node -e 的脚本里用双引号会变语法错误),这类脚本内容请用单引号;pwsh 7 无此问题。",
                (OUTPUT_HEAD + OUTPUT_TAIL) / 1024,
                OUTPUT_HEAD / 1024,
                OUTPUT_TAIL / 1024
            ),
            serde_json::json!({
                "type": "object",
                "properties": {
                    // 纯展示字段:assess/execute 都不读它(Args 也不声明,serde 直接忽略),
                    // 只给 UI 当标题用。它是模型自述,**不可信**——确认弹窗必须同时给出
                    // command 原文,绝不能只凭这句话决定放不放行
                    "description": {
                        "type": "string",
                        "description": "用一句中文说明这条命令做什么(不超过 20 字),写给用户看。不要复述命令本身,也不要写「执行命令」这类空话"
                    },
                    "command": {
                        "type": "string",
                        "description": "要执行的 shell 命令"
                    },
                    "timeout": {
                        "type": "integer",
                        "description": format!(
                            "超时秒数,默认 {DEFAULT_TIMEOUT},超出 {MAX_TIMEOUT} 按 {MAX_TIMEOUT} 算;超时连同子进程树一起杀"
                        )
                    },
                    "cwd": {
                        "type": "string",
                        "description": "工作目录:相对项目根、绝对路径、或 ~/开头。默认项目根"
                    }
                },
                "required": ["command", "description"]
            }),
        )
    }

    async fn assess(&self, args: &str) -> Action {
        let Ok(args) = serde_json::from_str::<Args>(args) else {
            return Action::Deny("bash 参数解析失败".into());
        };
        bash_safety::classify(&args.command).await
    }

    /// 授权记录:keys 是本工具 assess 的产出(**即 pattern 原文**,如 `del *`——授权键
    /// 无域前缀,域由 `Toolbox` 按 name 分发,name 已是一等公民,自包含前缀是死结构),
    /// 直接存入 bash_safety 授权集合。返回实际记录的 pattern(原样回传,供 UI 反馈
    /// 「已永久允许:del *」)
    fn grant(&self, keys: &[String]) -> Vec<String> {
        keys.iter().for_each(|k| bash_safety::record_grant(k));
        keys.to_vec()
    }

    async fn execute(&self, args: &str) -> anyhow::Result<String> {
        let args: Args =
            serde_json::from_str(args).map_err(|e| anyhow::anyhow!("bash 参数解析失败: {e}"))?;

        // 0 秒等于「刚起来就杀」,对模型没有任何用处;上限防止一个大数字换来「永不超时」
        let timeout_secs = args
            .timeout
            .unwrap_or(DEFAULT_TIMEOUT)
            .clamp(1, MAX_TIMEOUT);
        // cwd 默认项目根;传了则解析(相对项目根/~/绝对),不做路径范围校验(bash 是兜底)
        // 绝对路径用不上基准,故工作目录没注入也能用
        let base = workspace_root().ok();
        let cwd = match args.cwd {
            Some(c) => resolve_lexical(&c, base.as_deref())
                .map_err(|e| anyhow::anyhow!("无法解析 cwd: {e}"))?,
            None => base.ok_or_else(|| anyhow::anyhow!("工作目录未初始化"))?,
        };

        // 命令摘要:防刷屏,截到 300 字符
        let cmd_summary: String = args.command.chars().take(300).collect();
        tracing::info!(command = %cmd_summary, cwd = %cwd, timeout_secs, "bash 执行");

        let mut cmd = tokio::process::Command::new(shell::current().exe);
        cmd.args(shell::current().flags)
            .arg(&args.command)
            .current_dir(&cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true); // 超时/drop 时杀直接子进程(整棵树由下方守卫兜底)

        // 平台准备:Unix 让 shell 自成一个进程组,Windows 无需动作(见 process_tree)
        process_tree::configure(&mut cmd);

        let mut child = cmd.spawn().map_err(|e| {
            tracing::warn!(command = %cmd_summary, "bash 启动失败: {e:#}");
            anyhow::anyhow!(
                "启动命令失败(shell={},cwd={}): {e}",
                shell::current().exe,
                cwd
            )
        })?;

        // 进程树回收:kill_on_drop 只杀 shell,它 fork 出的后台进程会逃逸(`npm run dev &`
        // 后 node 继续占端口)。守卫交给下面的 `run` 持有,命令一退出就清理(见那里的注释);
        // 超时或取消时 `run` 被 drop,守卫跟着 drop 也一样生效。
        // 拿不到资源时降级为只有 kill_on_drop。
        let tree = child.id().and_then(ProcessTree::new);

        // 两路流必须并发读:只读一路会让另一路的管道写满,子进程卡死在那里,等不到退出。
        // 读取时即限流——`wait_with_output` 会把全部输出缓冲进内存,`cat 大文件` 直接 OOM
        let mut stdout = child.stdout.take().expect("stdout 已设为 piped");
        let mut stderr = child.stderr.take().expect("stderr 已设为 piped");
        let run = async move {
            let reads = async {
                tokio::join!(
                    read_capped(&mut stdout, OUTPUT_HEAD, OUTPUT_TAIL),
                    read_capped(&mut stderr, OUTPUT_HEAD, OUTPUT_TAIL),
                )
            };
            // 命令一退出就清进程树,不能等读完再清:后台进程继承了 stdout/stderr 的写端,
            // 只要它还活着,EOF 就不会到来,`read_capped` 会一直挂到超时。
            // 杀掉它们 → 写端归零 → EOF 到达 → 读取自然收尾(管道里已有的数据仍读得到)
            let wait_and_clear = async move {
                let status = child.wait().await;
                drop(tree);
                status
            };
            let ((out, err), status) = tokio::join!(reads, wait_and_clear);
            (out, err, status)
        };

        // 超时:timeout 返回后 `run` 被 drop → child drop → kill_on_drop 杀直接子进程,
        // 进程树守卫随之 drop 连孙进程一起杀。取消同理(调用方丢弃整个 future),
        // 进程生命周期天然绑定 future 生命周期,无需显式取消分支
        let (out, err, status) =
            match tokio::time::timeout(Duration::from_secs(timeout_secs), run).await {
                Ok(v) => v,
                Err(_) => {
                    tracing::warn!(command = %cmd_summary, timeout_secs, "bash 超时,进程已杀");
                    return Err(anyhow::anyhow!("命令超时(>{timeout_secs}s),进程已杀"));
                }
            };
        let status = status.map_err(|e| {
            tracing::warn!(command = %cmd_summary, "bash 执行失败: {e:#}");
            anyhow::anyhow!("命令执行失败: {e}")
        })?;

        let exit_code = status.code().unwrap_or(-1);
        tracing::info!(command = %cmd_summary, exit_code, "bash 完成");

        Ok(format!(
            "exit_code: {exit_code}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            render(out),
            render(err)
        ))
    }
}

/// 读一路输出,保留前 `head_cap` 字节和**最后** `tail_cap` 字节,中间照读但丢弃
/// (不累积),同时回报总字节数。
///
/// 头尾都留:命令行的结论几乎都在尾部,只保头时命令一旦先刷一堆日志
/// (`npm run dev & sleep 5; curl ...`),真实结果就整段被丢掉了。
///
/// 不能先全读再截:输出的上限必须由**读取**保证,而不是由「读完之后怎么处理」保证。
/// 丢弃而不是中断读取,是为了让子进程把话说完正常退出——中途关管道会让它收到 SIGPIPE,
/// exit_code 就变成 141,把真实结果盖掉。
async fn read_capped<R: AsyncRead + Unpin>(
    mut stream: R,
    head_cap: usize,
    tail_cap: usize,
) -> (Vec<u8>, Vec<u8>, usize) {
    let mut head = Vec::with_capacity(head_cap);
    // 尾部只要「最后 tail_cap 字节」:用双端队列,满了从出队端丢最旧的。VecDeque 内部
    // 是环形缓冲,丢头部不搬元素(只移起始指针);换成 Vec 的 drain 则每轮都要把窗口前移。
    let mut tail = VecDeque::with_capacity(tail_cap);
    // 读块不能超过 tail_cap,否则一块数据自己就装不下、必然丢字节。tail_cap=0 表示
    // 不保留尾部,此时块大小退回 READ_BUF(读来的字节 tail 全丢,不影响正确性)
    let buf_size = if tail_cap == 0 {
        READ_BUF
    } else {
        READ_BUF.min(tail_cap)
    };
    let mut buf = vec![0u8; buf_size];
    let mut total = 0;
    loop {
        let n = match stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(error = %e, "读取子进程输出中断");
                break;
            }
        };
        total += n;
        // 头部还有空位就先填满它,剩下的进尾部(一块数据可能跨头部边界)
        let take = n.min(head_cap.saturating_sub(head.len()));
        head.extend_from_slice(&buf[..take]);
        if tail_cap > 0 {
            let chunk = &buf[take..n];
            // 先腾位再追加:块大小恒 ≤ tail_cap,删除量不会超过现有长度,追加后长度恒 ≤ tail_cap
            tail.drain(..(tail.len() + chunk.len()).saturating_sub(tail_cap));
            tail.extend(chunk);
        }
    }
    (head, Vec::from(tail), total)
}

/// 解码 + 标注截断。lossy 是因为 Windows 下输出未必是 UTF-8(PS 5.1 重定向用控制台编码)。
/// 拼接后未超上限时要连着 tail 一起返回——head 满了以后所有数据都在 tail 里
fn render((head, tail, total): (Vec<u8>, Vec<u8>, usize)) -> String {
    let decode = |b: &[u8]| String::from_utf8_lossy(b).replace("\r\n", "\n");
    let kept = head.len() + tail.len();
    if total <= kept {
        let mut all = head;
        all.extend_from_slice(&tail);
        return decode(&all);
    }
    tracing::warn!(total_bytes = total, kept_bytes = kept, "bash 输出被截断");
    format!(
        "{}\n...(中间省略 {} 字节,共 {} 字节)...\n{}",
        decode(drop_partial_utf8(&head)),
        total - kept,
        total,
        decode(skip_utf8_continuation(&tail))
    )
}

/// 丢掉末尾不足一个完整字符的残字节:截断点落在多字节字符中间时,lossy 会吐出
/// U+FFFD,看起来像输出本身损坏,宁可少显示一个字
fn drop_partial_utf8(bytes: &[u8]) -> &[u8] {
    // UTF-8 字符最长 4 字节:从末尾回看最多 3 字节定位首字节
    for back in 1..=3.min(bytes.len()) {
        let b = bytes[bytes.len() - back];
        if b & 0xC0 != 0x80 {
            let need = if b >= 0xF0 {
                4
            } else if b >= 0xE0 {
                3
            } else if b >= 0xC0 {
                2
            } else {
                1
            };
            return if need > back {
                &bytes[..bytes.len() - back]
            } else {
                bytes
            };
        }
    }
    bytes
}

/// 丢掉开头不属于任何字符的续字节(被切开的那个字符的前半在 head 末尾,已丢弃)
fn skip_utf8_continuation(bytes: &[u8]) -> &[u8] {
    let n = bytes
        .iter()
        .take(3)
        .take_while(|b| *b & 0xC0 == 0x80)
        .count();
    &bytes[n..]
}

/// 切点对齐是跨平台的纯字节逻辑,两个平台都跑(上面两个模块按平台各测各的行为)
#[cfg(test)]
mod render_tests {
    use super::*;

    #[test]
    fn drops_partial_char_at_cut_point() {
        // "中" = E4 B8 AD:只留前两字节,缺一个字节 → 整个字符丢掉
        assert_eq!(drop_partial_utf8(&[b'a', 0xE4, 0xB8]), b"a");
        // 完整字符不动
        assert_eq!(drop_partial_utf8("a中".as_bytes()), "a中".as_bytes());
        // 纯 ASCII 不动
        assert_eq!(drop_partial_utf8(b"abc"), b"abc");
        // 空串不 panic
        assert_eq!(drop_partial_utf8(b""), b"");
    }

    #[test]
    fn skips_leading_continuation_bytes() {
        // "中" 的后两字节是续字节,开头丢掉
        assert_eq!(skip_utf8_continuation(&[0xB8, 0xAD, b'x']), b"x");
        assert_eq!(skip_utf8_continuation(b"abc"), b"abc");
        assert_eq!(skip_utf8_continuation(b""), b"");
    }

    /// 超上限时头尾都留,且一块数据跨头部边界时不能丢字节
    #[tokio::test]
    async fn read_capped_keeps_head_and_tail() {
        let data: Vec<u8> = (0..1000u32).map(|i| b'a' + (i % 26) as u8).collect();
        let (head, tail, total) = read_capped(&data[..], 100, 50).await;
        assert_eq!(total, 1000);
        assert_eq!(&head[..], &data[..100]);
        assert_eq!(&tail[..], &data[950..]);
    }

    /// 两个上限都为 0 时退化成只保另一侧,不需要特判
    #[tokio::test]
    async fn read_capped_degenerates_when_a_cap_is_zero() {
        let data = vec![b'x'; 500];
        let (head, tail, _) = read_capped(&data[..], 0, 100).await;
        assert!(head.is_empty());
        assert_eq!(tail.len(), 100);

        let (head, tail, _) = read_capped(&data[..], 100, 0).await;
        assert_eq!(head.len(), 100);
        assert!(tail.is_empty());
    }

    /// 数据远超一轮读取容量(8KB)且远超 tail 容量:尾部必须正好是最后 cap 字节,
    /// 不能残留上一轮的旧数据
    #[tokio::test]
    async fn read_capped_tail_is_the_very_end() {
        let data: Vec<u8> = (0..10_000).map(|i| (i % 251) as u8).collect();
        let (head, tail, total) = read_capped(&data[..], 0, 400).await;
        assert!(head.is_empty());
        assert_eq!(total, 10_000);
        assert_eq!(tail, &data[9_600..]);
    }

    /// 头尾两个上限 × 数据总量的交叉组合。断言用的是定义本身:head 正好是前
    /// `head_cap` 字节,剩余部分里 tail 正好是最后 `tail_cap` 字节。
    /// 覆盖空输入、单字节、正好一个读块、跨块、容量大于数据、上限为 0 这些边界
    #[tokio::test]
    async fn read_capped_matches_definition_across_caps() {
        let data: Vec<u8> = (0..30_000).map(|i| (i % 251) as u8).collect();
        for &total in &[0usize, 1, 8_191, 8_192, 8_193, 30_000] {
            for &head_cap in &[0usize, 1, 100, 8_192, 40_000] {
                for &tail_cap in &[0usize, 1, 100, 8_192, 40_000] {
                    let ctx = format!("total={total} head_cap={head_cap} tail_cap={tail_cap}");
                    let (head, tail, got_total) =
                        read_capped(&data[..total], head_cap, tail_cap).await;
                    assert_eq!(got_total, total, "{ctx}");

                    let head_len = head_cap.min(total);
                    assert_eq!(&head[..], &data[..head_len], "头部, {ctx}");

                    let rest = &data[head_len..total];
                    let tail_len = tail_cap.min(rest.len());
                    assert_eq!(&tail[..], &rest[rest.len() - tail_len..], "尾部, {ctx}");
                }
            }
        }
    }

    #[test]
    fn render_keeps_everything_when_under_limit() {
        let head = b"start\n".to_vec();
        let tail = b"end\n".to_vec();
        let total = head.len() + tail.len();
        assert_eq!(render((head, tail, total)), "start\nend\n");
    }

    #[test]
    fn render_marks_omitted_middle() {
        let head = b"start\n".repeat(4);
        let tail = b"end\n".to_vec();
        let total = 1000;
        let out = render((head, tail, total));
        assert!(out.starts_with("start\n"), "{out}");
        assert!(out.ends_with("end\n"), "{out}");
        assert!(out.contains("中间省略"), "{out}");
    }
}

/// 回归:cmd 时代的两类硬伤(引号被 MSVC 转义改写、LF 被当命令行终止)在新链路必须消失。
/// 走真实 `execute`(模型命令的完整路径),不单独复刻 spawn;与 powershell.rs 的解析端到端
/// 用例一起,分别钉住「执行侧」与「判定侧」
#[cfg(all(test, windows))]
mod tests {
    use super::*;

    async fn run(command: &str) -> String {
        let args = serde_json::json!({
            "command": command,
            "cwd": std::env::temp_dir().to_string_lossy(),
        });
        BashTool.execute(&args.to_string()).await.unwrap()
    }

    #[tokio::test]
    async fn quotes_and_newlines_survive_the_shell_layer() {
        // 带空格的引号参数
        assert!(run(r#"Write-Output "a b""#).await.contains("a b"));
        // 引号内尾随反斜杠:MSVC 转义的经典边界
        assert!(run(r#"Write-Output "C:\temp\""#)
            .await
            .contains(r"C:\temp\"));
        // 嵌套引号:双引号串里含双引号
        assert!(run(r#"Write-Output 'a "b" c'"#)
            .await
            .contains(r#"a "b" c"#));
        // 多行(LF / CRLF)必须是「多条语句」而不是只跑第一行
        assert!(run("$x = 41\n$y = 1\nWrite-Output ($x + $y)")
            .await
            .contains("42"));
        assert!(run("$x = 41\r\n$y = 1\r\nWrite-Output ($x + $y)")
            .await
            .contains("42"));
    }

    /// 后台进程继承了 stdout/stderr 时,命令退出后必须立刻清进程树:只要还有进程握着
    /// 写端,EOF 就不会到来,读取会一直挂到超时(见 `run` 里 `wait_and_clear` 的注释)。
    /// 这里用 timeout=20 兜底——修复失效时测试失败,而不是挂满默认 300s
    #[tokio::test]
    async fn does_not_hang_when_background_process_holds_the_pipe() {
        let args = serde_json::json!({
            "command": r#"cmd /c "start /b ping -n 20 127.0.0.1""#,
            "cwd": std::env::temp_dir().to_string_lossy(),
            "timeout": 20,
        });
        let started = std::time::Instant::now();
        let out = BashTool.execute(&args.to_string()).await.unwrap();
        assert!(out.contains("exit_code: 0"), "{out}");
        assert!(
            started.elapsed().as_secs() < 10,
            "不该等后台进程跑完,实际耗时 {:?}",
            started.elapsed()
        );
    }

    /// 超上限时必须头尾都留:命令行结论在尾部,只保头会让真实结果整段丢失
    /// (`npm run dev & sleep 5; curl ...` 这种日志先刷屏的命令)
    #[tokio::test]
    async fn keeps_head_and_tail_when_output_exceeds_limit() {
        let args = serde_json::json!({
            "command": r#"Write-Output "START_MARKER"; Write-Output ("A" * 100000); Write-Output "END_MARKER""#,
            "cwd": std::env::temp_dir().to_string_lossy(),
        });
        let out = BashTool.execute(&args.to_string()).await.unwrap();
        assert!(out.contains("START_MARKER"), "头部标记丢失:\n{out}");
        assert!(out.contains("END_MARKER"), "尾部标记丢失:\n{out}");
        assert!(out.contains("中间省略"), "缺少截断标注:\n{out}");
    }
}

/// Unix 侧的契约:退出码 / 两路流 / 输出上限 / 超时。Windows 那份测的是引号与多行这类
/// 平台特有语义,这里测的是所有平台都该成立的承诺
#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;

    async fn run(command: &str, timeout: Option<u64>) -> anyhow::Result<String> {
        let args = serde_json::json!({
            "command": command,
            "cwd": std::env::temp_dir().to_string_lossy(),
            "timeout": timeout,
        });
        BashTool.execute(&args.to_string()).await
    }

    #[tokio::test]
    async fn reports_exit_code_and_both_streams() {
        let out = run("echo hello; echo oops >&2; exit 3", None)
            .await
            .unwrap();
        assert!(out.contains("exit_code: 3"), "{out}");
        assert!(out.contains("hello"), "{out}");
        assert!(out.contains("oops"), "stderr 要单独成段: {out}");
    }

    /// 5MB 输出:只留单流上限(头 4KB + 尾 28KB),且必须报出原始字节数——上限由读取保证,不是读完再截
    #[tokio::test]
    async fn caps_huge_output() {
        let out = run("yes x | head -c 5000000", None).await.unwrap();
        assert!(out.contains("输出截断"), "{out}");
        assert!(out.contains("5000000"), "应报出真实总字节数: {out}");
        assert!(
            out.len() < 200 * 1024,
            "返回值不该接近原始大小: {}",
            out.len()
        );
    }

    #[tokio::test]
    async fn timeout_kills_long_command() {
        let msg = run("sleep 5", Some(1)).await.unwrap_err().to_string();
        assert!(msg.contains("超时"), "{msg}");
    }

    /// timeout=0 会被抬到 1 秒,而不是「刚起来就杀」——后者对模型没有任何意义
    #[tokio::test]
    async fn timeout_zero_is_raised_to_one_second() {
        assert!(run("echo ok", Some(0)).await.unwrap().contains("ok"));
    }

    #[tokio::test]
    async fn missing_cwd_is_reported_with_context() {
        let args = serde_json::json!({ "command": "true", "cwd": "/no/such/dir" });
        let msg = BashTool
            .execute(&args.to_string())
            .await
            .unwrap_err()
            .to_string();
        assert!(msg.contains("启动命令失败"), "{msg}");
    }

    /// 命令退出后,它 fork 出的后台进程必须跟着走——否则 `npm run dev &` 会永久占住端口。
    /// 这是 `process_group(0)` + `KillGroup` 的存在理由:kill_on_drop 只管 shell 本身
    #[tokio::test]
    async fn kills_background_children() {
        let out = run("sleep 300 & echo $!", None).await.unwrap();
        let pid: i32 = out
            .lines()
            .find_map(|l| l.trim().parse().ok())
            .expect("应能拿到后台进程 pid");

        // SIGKILL 立即生效,但进程从退出到被回收有微小延迟
        tokio::time::sleep(Duration::from_millis(300)).await;
        // SAFETY: 信号 0 只做存在性探测,不发送任何信号、不碰内存
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(!alive, "后台进程 {pid} 应随命令结束被杀");
    }
}
