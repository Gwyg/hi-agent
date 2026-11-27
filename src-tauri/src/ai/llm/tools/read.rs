use async_trait::async_trait;
use serde::Deserialize;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::time::Duration;

use super::support::path_scope::{has_sensitive_suffix, is_sensitive_path, workspace_root};
use super::{Action, Tool};
use crate::ai::llm::types::ToolDef;
use crate::util::{resolve_lexical, StopSignal};

/// 单次读取行数硬上限(防 limit 巨大把整个文件读入内存)
const MAX_LINES_PER_READ: usize = 2000;

/// 单次输出体积上限。行数不够用:一行能到 8KB,2000 行就是 16MB,两者相乘等于没有上界。
///
/// 32KB 约合 8k token、800-1000 行典型代码,一次占不到上下文窗口的十分之一;
/// 再大就会把整轮对话推到压缩线上,再小则大文件要反复分页,每轮还得多发一次上下文。
const MAX_BYTES_PER_READ: usize = 32 * 1024;

/// 单行保留上限(字节):超出只留前这么多,余下读到换行符为止全部丢弃。
///
/// 不设它就没有任何上限——`lines()` 会把整行分配成 `String`,minified JS / 单行 JSON /
/// 无换行日志一行能有几十上百 MB,分配出去那一刻就已经 OOM,再判长度来不及。
const MAX_LINE_BYTES: usize = 8 * 1024;

/// 单行扫描上限(字节):超过这么多还没见到换行,判定这行不是文本,放弃并停止本次读取。
///
/// 与 `MAX_LINE_BYTES` 是两件事:那个决定「留多少给模型看」,这个决定「扫多久就认定不是文本」。
/// 没有它,一个没有换行符的 10GB 单行文件会让读线程一路扫到 EOF——内存不涨(保留量恒定),
/// 但白跑几分钟磁盘 IO,而且超时之后线程还在跑。
///
/// 1MB 是「正常文本不可能达到」的量级(常规代码/日志最长一行通常不到 4KB),
/// 撞上它的只有 sourcemap、minified bundle、base64 内嵌数据——本来就不是 read 该看的东西。
const MAX_LINE_SCAN: usize = 1024 * 1024;

/// 同步 IO 超时(防慢盘/NFS 挂死)
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// 一行读出来的三种结局,调用方处理各不相同,故用枚举而非 `Option<bool>`。
#[derive(Debug, PartialEq)]
enum Line {
    /// 读到一行(可能是末尾没有换行符的半行),`bool` = 是否发生过截断
    Read(bool),
    /// 文件正常结束
    Eof,
    /// 单行扫过 `MAX_LINE_SCAN` 仍未换行:已放弃该行,且位置停在行中间不可恢复
    TooLong,
}

/// 本次读取提前收手的原因。
enum Cut {
    /// 页满:行数或体积到顶,后面还有内容可续读
    Page(String),
    /// 某一行超长:后面读不动了(记行号),不该再给续读 offset
    TooLong(usize),
}

/// 读取文件内容(无副作用,高频只读操作)
pub struct ReadTool;

#[derive(Deserialize)]
struct Args {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

impl ReadTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "read",
            format!(
                "读取文件内容(带行号)。无副作用。\n\
                 适用:看代码/配置/日志/文档内容;确认文件当前状态。\n\
                 不适用:看目录有哪些文件用 search_files(pattern);搜文件内某文本用 search_files(content);看命令输出用 bash。\n\
                 单次最多返回 {MAX_LINES_PER_READ} 行且不超过 {}KB,超出会截断,尾部说明原因\
                 (页满时附续读 offset;单行超长时不给 offset,换工具);\
                 读大文件用 offset/limit 取需要的段,不要试图一次读全。",
                MAX_BYTES_PER_READ / 1024
            ),
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "文件路径:相对项目根、绝对路径、或 ~/开头(展开家目录)。不接受符号链接,请传真实路径"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "起始行号(从1开始),默认1"
                    },
                    "limit": {
                        "type": "integer",
                        "description": format!(
                            "最多读取行数,默认 {MAX_LINES_PER_READ};超过 {MAX_LINES_PER_READ} 也按 {MAX_LINES_PER_READ} 处理"
                        )
                    }
                },
                "required": ["path"]
            }),
        )
    }

    async fn assess(&self, args: &str) -> Action {
        let Ok(args) = serde_json::from_str::<Args>(args) else {
            return Action::Deny("参数解析失败".into());
        };
        // read 不做路径范围校验(允许读项目外),只拦敏感文件防泄露
        // 敏感文件每次必问(AskAlways,不进 grant)
        if has_sensitive_suffix(&args.path) || is_sensitive_path(&args.path) {
            Action::AskAlways
        } else {
            Action::Allow
        }
    }

    async fn execute(&self, args: &str) -> anyhow::Result<String> {
        let args: Args =
            serde_json::from_str(args).map_err(|e| anyhow::anyhow!("read 参数解析失败: {e}"))?;
        let offset = args.offset.unwrap_or(1).max(1);

        // 相对路径拼工作目录根,绝对路径直通;read 允许读项目外(读不破坏)
        // 用词法解析(不 canonicalize):下面要靠 symlink_metadata 看见软链本身
        let root = workspace_root().ok();
        let path = resolve_lexical(&args.path, root.as_deref())?;

        // 同步 IO(取元数据 + 逐行读)整体交给工作线程,超时和停止信号一起管:
        // `stop` 是 owner,本函数返回时(超时 / 取消 / 正常)随之 drop 并置位,
        // 工作线程手里那份句柄下一轮轮询就收手
        let stop = StopSignal::new();
        let handle = stop.clone();
        let read_path = path.clone();
        let limit = args.limit;

        tokio::time::timeout(
            READ_TIMEOUT,
            tokio::task::spawn_blocking(move || read_sync(&read_path, offset, limit, &handle)),
        )
        .await
        .map_err(|_| {
            // 超时几乎总是「路径挂死」或「读法不对」,给出下一步而不是让模型原地重试
            anyhow::anyhow!(
                "读取超时(>{:?}): {path} —— 路径可能在网络盘上,或文件过大、offset 太靠后;\
                 可改用 bash `tail -n 100` 读尾部,或 search_files 定位关键行",
                READ_TIMEOUT
            )
        })?
        .map_err(|e| anyhow::anyhow!("读取任务失败: {e}"))?
    }
}

/// 同步读:取元数据 → 校验类型 → 按行取 `offset` 之后的一页。
///
/// 整段跑在 `spawn_blocking` 里,`stop` 每读一行轮询一次:超时或取消后外层置位,
/// 这里读到下一行就收手,不会把整个大文件白扫完。
fn read_sync(
    path: &str,
    offset: usize,
    limit: Option<usize>,
    stop: &StopSignal,
) -> anyhow::Result<String> {
    // 只拒「不能安全读的类型」:目录、FIFO、设备、socket 会阻塞或无限读
    let size = ensure_plain_file(path)?;

    let file = File::open(path).map_err(|e| anyhow::anyhow!("打开失败 {path}: {e}"))?;
    let mut reader = BufReader::new(file);
    // limit 给 0 视同没传:否则一页零行,输出全空还解释不清
    let cap = limit
        .filter(|&n| n > 0)
        .unwrap_or(MAX_LINES_PER_READ)
        .min(MAX_LINES_PER_READ);

    let mut out = String::new();
    let mut buf = Vec::new();
    let mut line_no = 0; // 已读到的行号(1 基),扫到 EOF 时正好是总行数
    let mut shown = 0; // 已输出的行数
    let mut cut: Option<Cut> = None; // 提前收手的原因,`None` 表示读到了 EOF

    loop {
        if stop.is_stopped() {
            return Err(anyhow::anyhow!("读取已中止: {path}"));
        }
        let capped = match read_line_capped(&mut reader, &mut buf, MAX_LINE_BYTES).map_err(|e| {
            // 编码问题已由 `from_utf8_lossy` 兜住(坏字节显示成 �),走到这里只会是真正的 IO 错误
            anyhow::anyhow!("读取失败 {path}: {e} —— 磁盘、权限或文件已不可读")
        })? {
            Line::Read(capped) => capped,
            Line::Eof => break,
            Line::TooLong => {
                // 位置停在行中间:再读只会把同一行的碎片当成新行,行号全乱,必须就此收手
                cut = Some(Cut::TooLong(line_no + 1));
                break;
            }
        };
        line_no += 1;
        if line_no < offset {
            continue;
        }
        // 已经给足一页,而这里又读出下一行 → 后面还有,停在该处
        if shown == cap {
            cut = Some(Cut::Page(format!("单次上限 {cap} 行")));
            break;
        }
        let text = String::from_utf8_lossy(&buf);
        let text = text.trim_end_matches('\r');
        // 首行无论如何要给:否则预算过小时一行都出不来,只剩一句看不懂的截断提示
        // 余量 64 是行开销:`%6d` 行号 + tab + 换行 + 最长的截断标记(31 字节)
        if shown > 0 && out.len() + text.len() + 64 > MAX_BYTES_PER_READ {
            cut = Some(Cut::Page(format!(
                "输出已达 {}KB 上限",
                MAX_BYTES_PER_READ / 1024
            )));
            break;
        }
        shown += 1;
        out.push_str(&format!("{line_no:>6}\t{text}"));
        if capped {
            out.push_str(" …(本行过长,已截断)");
        }
        out.push('\n');
    }

    // 一行都没输出:要么文件是空的,要么 offset 起过头(此时 line_no 已扫到总行数)
    if shown == 0 {
        // 超长行先于上面两种:否则「(空文件)」/「offset 超出范围」都在撒谎——
        // 文件有内容,只是这一行读不动
        if let Some(Cut::TooLong(line)) = &cut {
            return Ok(format!(
                "(第 {line} 行超过 {}KB 仍未换行,没有可读内容 —— \
                 可能是单行 JSON / 压缩文本,改用 bash 或 search_files 处理)",
                MAX_LINE_SCAN / 1024
            ));
        }
        return Ok(match line_no {
            0 => "(空文件)".to_string(),
            total => format!("(文件共 {total} 行,offset={offset} 超出范围)"),
        });
    }
    // 带上文件总大小:32KB 一页的模型才知道后面还有几页,值不值得继续翻。
    // 没有这个数,「还有更多行」等于没说话——800KB 的文件硬翻 25 页不如换个工具定位。
    let last = offset + shown - 1;
    match cut {
        Some(Cut::Page(reason)) => out.push_str(&format!(
            "\n[已显示 {offset}-{last} 行({reason}),全文 {}KB,用 offset={} 继续]\n",
            size.div_ceil(1024),
            last + 1
        )),
        // 不给 offset:同一堵墙后面还是同一堵墙,重复试只会再失败一次
        Some(Cut::TooLong(line)) => out.push_str(&format!(
            "\n[已显示 {offset}-{last} 行,第 {line} 行超过 {}KB 仍未换行,后续内容未读取 —— \
             可能是单行 JSON / 压缩文本,改用 bash 或 search_files 处理]\n",
            MAX_LINE_SCAN / 1024
        )),
        None => {}
    }
    Ok(out)
}

/// 闸门:确认这个路径能安全地按行读,返回文件大小(字节)。
///
/// 只拒「不能安全读的类型」——目录读不出行,FIFO / 设备 / socket 会阻塞或无限读。
/// 软链也拒:调用方给的应当是目标本身,否则「读到的是哪个文件」取决于软链当时的指向。
fn ensure_plain_file(path: &str) -> anyhow::Result<u64> {
    let meta =
        std::fs::symlink_metadata(path).map_err(|e| anyhow::anyhow!("访问失败 {path}: {e}"))?;
    if meta.file_type().is_symlink() {
        return Err(anyhow::anyhow!(
            "拒绝符号链接 {path} —— 请传目标的真实路径(bash `readlink -f` 可得到)"
        ));
    }
    if !meta.is_file() {
        return Err(anyhow::anyhow!(
            "拒绝非普通文件 {path} —— 目录 / FIFO / 设备 / socket 无法按行读取"
        ));
    }
    Ok(meta.len())
}

/// 读一行到 `buf`,最多保留 `max` 字节。
///
/// 超出的部分照样从流里消费掉(否则下一轮会把同一行从头再读一遍),只是不留内存——
/// 内存占用与行长无关。这是唯一能防住「一行几百 MB」的写法:`lines()` / `read_line`
/// 都会先把整行分配成 `String`,等拿到手再判长度,OOM 已经发生了。
///
/// 循环的终点是「见到换行符」或「EOF」,与 `max` 无关——`max` 只管留多少,不管读多久。
/// 唯一让循环提前收手的是 `MAX_LINE_SCAN`:扫满这么多字节还没换行,判定这行不是文本。
fn read_line_capped<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<Line> {
    buf.clear();
    let mut got_bytes = false;
    let mut capped = false;
    let mut scanned = 0; // 这一行已扫过的字节(含丢弃的),只用于判断「是不是文本」

    loop {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            // 末行没有换行符:读过字节就交出去,一个字节都没读到才是 EOF
            return Ok(if got_bytes {
                Line::Read(capped)
            } else {
                Line::Eof
            });
        }
        got_bytes = true;
        scanned += chunk.len();

        let nl = chunk.iter().position(|&b| b == b'\n');
        // 扫过上限还没见到换行:这行不是文本,别再无止境地丢字节
        if nl.is_none() && scanned > MAX_LINE_SCAN {
            return Ok(Line::TooLong);
        }

        let take = nl.unwrap_or(chunk.len()); // 本段属于这一行的字节数
        let keep = take.min(max.saturating_sub(buf.len())); // 其中留得下的
        buf.extend_from_slice(&chunk[..keep]);
        capped |= keep < take; // 有砍就有截断
                               // 换行符本身也消费掉,否则下一轮从同一处开始、永远找不到新行
        reader.consume(take + usize::from(nl.is_some()));

        if nl.is_some() {
            return Ok(Line::Read(capped));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 写临时文件并返回**绝对路径**:绝对路径免基准,测试不依赖工作区根
    fn tmp_file(name: &str, content: &[u8]) -> String {
        let dir = std::env::temp_dir().join("hi_agent_read_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path.display().to_string()
    }

    async fn read(args: serde_json::Value) -> String {
        ReadTool::new()
            .execute(&args.to_string())
            .await
            .expect("读取应成功")
    }

    /// 基本形态:每行 `行号\t内容`,`%6d` 右对齐
    #[tokio::test]
    async fn reads_lines_with_numbers() {
        let path = tmp_file("lines.txt", b"one\ntwo\nthree\n");
        let out = read(serde_json::json!({ "path": path })).await;
        assert_eq!(out, "     1\tone\n     2\ttwo\n     3\tthree\n");
    }

    /// 分页:只给一页,并指出下一页从哪开始
    #[tokio::test]
    async fn paginates_and_points_at_next_offset() {
        let content = (1..=10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let path = tmp_file("page.txt", content.as_bytes());
        let out = read(serde_json::json!({ "path": path, "offset": 3, "limit": 2 })).await;

        assert!(out.starts_with("     3\tline3\n     4\tline4\n"), "{out}");
        assert!(out.contains("已显示 3-4 行(单次上限 2 行)"), "{out}");
        assert!(out.contains("offset=5"), "{out}");
    }

    /// 空文件要给一句话,不能返回空串——空串会被读成「工具失败了」
    #[tokio::test]
    async fn empty_file_is_stated() {
        let path = tmp_file("empty.txt", b"");
        assert_eq!(read(serde_json::json!({ "path": path })).await, "(空文件)");
    }

    /// CRLF 的行尾 `\r` 要剔掉:否则 Windows 文件的每行末尾都挂一个不可见字符
    #[tokio::test]
    async fn strips_cr_before_lf() {
        let path = tmp_file("crlf.txt", b"a\r\nb\r\n");
        assert_eq!(
            read(serde_json::json!({ "path": path })).await,
            "     1\ta\n     2\tb\n"
        );
    }

    /// offset 起过头:说明文件实际有多少行,而不是静默给空
    #[tokio::test]
    async fn offset_past_end_explains() {
        let path = tmp_file("short.txt", b"a\nb\n");
        let out = read(serde_json::json!({ "path": path, "offset": 9 })).await;
        assert!(out.contains("文件共 2 行"), "{out}");
        assert!(out.contains("offset=9 超出范围"), "{out}");
    }

    /// 单行超长只留前 MAX_LINE_BYTES 字节:整行进内存才判长度的话,OOM 已经发生了
    #[tokio::test]
    async fn caps_oversized_line() {
        let long = "x".repeat(MAX_LINE_BYTES + 500);
        let path = tmp_file("long.txt", long.as_bytes());
        let out = read(serde_json::json!({ "path": path })).await;

        assert!(out.contains("本行过长,已截断"), "{out}");
        assert!(out.len() < long.len(), "输出应远短于原行: {}", out.len());
    }

    /// 体积先到顶:行数没到 2000,但输出已经 32KB,尾注要说清是哪个原因
    #[tokio::test]
    async fn stops_on_byte_budget_before_line_cap() {
        let content = (0..3000)
            .map(|i| format!("{i:0>20}"))
            .collect::<Vec<_>>()
            .join("\n");
        let path = tmp_file("wide.txt", content.as_bytes());
        let out = read(serde_json::json!({ "path": path })).await;

        assert!(out.contains("输出已达 32KB 上限"), "{out}");
        assert!(out.contains("全文"), "{out}");
        assert!(
            out.len() < MAX_BYTES_PER_READ + 200,
            "尾注之外不该超出预算: {}",
            out.len()
        );
    }

    /// 行数先到顶:每行很短,2000 行才 26KB,不会碰预算
    #[tokio::test]
    async fn stops_on_line_cap_when_lines_are_short() {
        let content = (0..2500)
            .map(|i| format!("{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let path = tmp_file("many.txt", content.as_bytes());
        let out = read(serde_json::json!({ "path": path })).await;

        assert!(out.contains("单次上限 2000 行"), "{out}");
        assert!(out.contains("offset=2001"), "{out}");
    }

    /// 读行的边界:末行没有换行符也要交出来,之后才是 EOF
    #[test]
    fn capped_reader_stops_at_newline_and_eof() {
        let mut reader = std::io::BufReader::new(&b"a\nbc"[..]);
        let mut buf = Vec::new();

        assert_eq!(
            read_line_capped(&mut reader, &mut buf, 64).unwrap(),
            Line::Read(false)
        );
        assert_eq!(buf, b"a".to_vec());
        assert_eq!(
            read_line_capped(&mut reader, &mut buf, 64).unwrap(),
            Line::Read(false)
        );
        assert_eq!(buf, b"bc".to_vec());
        assert_eq!(
            read_line_capped(&mut reader, &mut buf, 64).unwrap(),
            Line::Eof
        );
    }

    /// 超长行的剩余部分要被消费掉,不能留在流里让下一轮重读
    #[test]
    fn capped_reader_drops_overflow() {
        let data = vec![b'x'; MAX_LINE_BYTES + 10];
        let mut reader = std::io::BufReader::new(&data[..]);
        let mut buf = Vec::new();

        assert_eq!(
            read_line_capped(&mut reader, &mut buf, MAX_LINE_BYTES).unwrap(),
            Line::Read(true)
        );
        assert_eq!(buf.len(), MAX_LINE_BYTES);
        assert_eq!(
            read_line_capped(&mut reader, &mut buf, MAX_LINE_BYTES).unwrap(),
            Line::Eof
        );
    }

    /// 扫过上限仍无换行:交出 `TooLong`,而不是继续丢字节到 EOF
    #[test]
    fn capped_reader_gives_up_on_unnewlineable_line() {
        let data = vec![b'x'; MAX_LINE_SCAN + 1];
        let mut reader = std::io::BufReader::new(&data[..]);
        let mut buf = Vec::new();

        assert_eq!(
            read_line_capped(&mut reader, &mut buf, MAX_LINE_BYTES).unwrap(),
            Line::TooLong
        );
        assert_eq!(buf.len(), MAX_LINE_BYTES, "放弃前应已保留一页的量");
    }

    /// 一行扫不动时:已读到的行照常返回,尾注说明原因且**不给**续读 offset
    #[tokio::test]
    async fn stops_on_unnewlineable_line() {
        let mut content = b"first\nsecond\n".to_vec();
        content.resize(content.len() + MAX_LINE_SCAN + 1024, b'x');
        let path = tmp_file("oneline.txt", &content);
        let out = read(serde_json::json!({ "path": path })).await;

        assert!(out.starts_with("     1\tfirst\n     2\tsecond\n"), "{out}");
        assert!(out.contains("第 3 行超过 1024KB 仍未换行"), "{out}");
        assert!(
            !out.contains("offset="),
            "超长行之后不该再给续读提示: {out}"
        );
    }

    /// 第一行就撞上超长行:不能报「空文件」,要说清是哪一行读不动
    #[tokio::test]
    async fn unnewlineable_first_line_is_explained() {
        let content = vec![b'x'; MAX_LINE_SCAN + 1024];
        let path = tmp_file("oneline_head.txt", &content);
        let out = read(serde_json::json!({ "path": path })).await;

        assert!(out.contains("第 1 行超过 1024KB 仍未换行"), "{out}");
        assert!(!out.contains("空文件"), "{out}");
        assert!(!out.contains("超出范围"), "{out}");
    }
}
