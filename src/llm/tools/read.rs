use async_openai::types::chat::{ChatCompletionTool, ChatCompletionTools, FunctionObject};
use async_trait::async_trait;
use serde::Deserialize;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::time::Duration;

use super::sandbox::is_sensitive_path;
use super::{Action, Tool};

/// 大文件阈值:超过此大小走流式读(不缓存)
/// 256KB 覆盖 99% 代码文件,日志/数据走流式
const LARGE_FILE_THRESHOLD: u64 = 256 * 1024;

/// 单次读取行数硬上限(防 limit 巨大把整个大文件读入内存)
const MAX_LINES_PER_READ: usize = 2000;

/// 同步 IO 超时(防 FIFO/慢盘/NFS 挂死)
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// 读取文件内容(无副作用,高频只读操作)
/// 小文件全量读,大文件流式(防 OOM)
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

    fn definition(&self) -> ChatCompletionTools {
        ChatCompletionTools::Function(ChatCompletionTool {
            function: FunctionObject {
                name: "read".to_string(),
                description: Some(
                    "读取文件内容(带行号)。无副作用。\n\
                    适用:看代码/配置/日志/文档内容;确认文件当前状态。\n\
                    不适用:看目录有哪些文件用 search_files(pattern);搜文件内某文本用 search_files(content);看命令输出用 bash。\n\
                    大文件用 offset/limit 分页,不要一次读全。"
                        .to_string(),
                ),
                parameters: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "文件路径(相对项目根或绝对路径)"
                        },
                        "offset": {
                            "type": "integer",
                            "description": "起始行号(从1开始),默认1"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "读取行数,不填则读到文件末尾"
                        }
                    },
                    "required": ["path"]
                })),
                strict: None,
            },
        })
    }

    fn assess(&self, args: &str) -> Action {
        let Ok(args) = serde_json::from_str::<Args>(args) else {
            return Action::Deny("参数解析失败".into());
        };
        // read 不做沙箱边界校验(允许读项目外),只拦敏感文件防泄露
        // 敏感文件每次必问(persistable=false,不进 grant),keys 空
        if is_sensitive_path(&args.path) {
            Action::Ask {
                persistable: false,
                keys: Vec::new(),
            }
        } else {
            Action::Allow
        }
    }

    async fn execute(&self, args: &str) -> anyhow::Result<String> {
        let args: Args =
            serde_json::from_str(args).map_err(|e| anyhow::anyhow!("read 参数解析失败: {e}"))?;
        let offset = args.offset.unwrap_or(1).max(1);

        // 路径解析:相对路径拼项目根,绝对路径直通
        // read 放宽沙箱:允许读项目外(读不破坏,有时需读全局配置),仍拒软链/特殊文件
        let path = super::sandbox::resolve_path(&args.path)?;
        let path_str = path.display().to_string();

        // 安全校验:拒软链 + 拒非普通文件(FIFO/设备/socket 会阻塞或无限读)
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|e| anyhow::anyhow!("访问失败 {}: {e}", path_str))?;
        if meta.file_type().is_symlink() {
            return Err(anyhow::anyhow!(
                "拒绝符号链接 {} —— 可能逃逸项目沙箱。如需读取目标,请用绝对路径显式调用",
                path_str
            ));
        }
        if !meta.is_file() {
            return Err(anyhow::anyhow!(
                "拒绝非普通文件 {} —— FIFO/设备/socket 会阻塞读取或导致 OOM",
                path_str
            ));
        }

        let size = meta.len();
        let path_for_blocking = path.clone();

        // spawn_blocking 隔离同步 IO,超时兜底 FIFO/慢盘/NFS 挂死
        let read_result = tokio::time::timeout(
            READ_TIMEOUT,
            tokio::task::spawn_blocking(move || {
                let path = path_for_blocking;
                let path_str = path.display().to_string();
                if size > LARGE_FILE_THRESHOLD {
                    read_streaming(&path_str, offset, args.limit)
                } else {
                    std::fs::read(&path)
                        .map_err(|e| anyhow::anyhow!("读取失败 {}: {e}", path_str))
                        .and_then(|bytes| {
                            let lines = decode_and_split(&bytes, &path_str)?;
                            Ok(format_lines(&lines, offset, args.limit))
                        })
                }
            }),
        )
        .await
        .map_err(|_| anyhow::anyhow!("读取超时(>{:?}): {}", READ_TIMEOUT, path_str))?
        .map_err(|e| anyhow::anyhow!("读取任务失败: {e}"))??;

        Ok(read_result)
    }
}

/// 流式读:BufReader 顺序扫到 offset,读 limit 行(防 OOM)
fn read_streaming(path: &str, offset: usize, limit: Option<usize>) -> anyhow::Result<String> {
    let file = File::open(path).map_err(|e| anyhow::anyhow!("打开失败 {}: {e}", path))?;
    let reader = BufReader::new(file);
    let start = offset.saturating_sub(1);
    let cap = limit.unwrap_or(MAX_LINES_PER_READ).min(MAX_LINES_PER_READ);
    let end = start.saturating_add(cap);

    let mut out = String::new();
    let mut line_no = 0;
    for line in reader.lines() {
        line_no += 1;
        if line_no <= start {
            continue;
        }
        if line_no > end {
            break;
        }
        let line = line.map_err(|e| {
            anyhow::anyhow!(
                "读取失败 {}: {e}。\n\
             可能是二进制(用 bash `file {}` 查类型)或非 UTF-8 编码(可用 `iconv` 转换)",
                path,
                path
            )
        })?;
        let line = line.trim_end_matches('\r');
        out.push_str(&format!("{line_no:>6}\t{line}\n"));
    }
    Ok(out)
}

/// 解码字节 → UTF-8 String → normalize 换行 → 分行
/// 非 UTF-8(含二进制)报错,带 bash 兜底引导
fn decode_and_split(bytes: &[u8], path: &str) -> anyhow::Result<Vec<String>> {
    let content = String::from_utf8(bytes.to_vec()).map_err(|e| {
        anyhow::anyhow!(
            "文件 {} 非 UTF-8 无法读取: {e}。\n\
             可能是二进制(用 bash `file {}` 查类型)或非 UTF-8 文本(可用 `iconv -f GBK -t UTF-8 {}` 转换)",
            path, path, path
        )
    })?;
    let content = content.replace("\r\n", "\n");
    Ok(content.lines().map(String::from).collect())
}

/// 切片 + 行号格式化(类似 cat -n)
fn format_lines(lines: &[String], offset: usize, limit: Option<usize>) -> String {
    let start = offset.saturating_sub(1).min(lines.len());
    let end = match limit {
        Some(l) => start.saturating_add(l).min(lines.len()),
        None => lines.len(),
    };
    let mut out = String::new();
    for (i, line) in lines[start..end].iter().enumerate() {
        let line_no = start + i + 1;
        out.push_str(&format!("{line_no:>6}\t{line}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_lines_offset_overflow_safe() {
        let lines = vec!["a".to_string(), "b".to_string()];
        // offset 巨大不应 panic,返回空
        let out = format_lines(&lines, usize::MAX, Some(10));
        assert_eq!(out, "");
    }

    #[test]
    fn format_lines_limit_overflow_safe() {
        let lines = vec!["a".to_string(), "b".to_string()];
        // limit 巨大不应 panic,截到末尾
        let out = format_lines(&lines, 1, Some(usize::MAX));
        assert!(out.contains("a") && out.contains("b"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_fifo() {
        let fifo = "/tmp/test_fifo";
        std::fs::remove_file(fifo).ok();
        nix::unistd::mkfifo(std::path::Path::new(fifo), nix::sys::stat::Mode::S_IRWXU).ok();
        let tool = ReadTool::new();
        let args = serde_json::json!({"path": fifo}).to_string();
        let res = tool.execute(&args).await;
        assert!(res.is_err(), "FIFO 应被拒绝");
        std::fs::remove_file(fifo).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlink() {
        let target = "/tmp/test_read_target.txt";
        let link = "/tmp/test_read_link.txt";
        std::fs::write(target, "hello").ok();
        std::os::unix::fs::symlink(target, link).ok();
        let tool = ReadTool::new();
        let args = serde_json::json!({"path": link}).to_string();
        let res = tool.execute(&args).await;
        assert!(res.is_err(), "symlink 应被拒绝");
        std::fs::remove_file(link).ok();
        std::fs::remove_file(target).ok();
    }

    #[tokio::test]
    async fn reads_normal_file() {
        let path = "/tmp/test_read_normal.txt";
        std::fs::write(path, "line1\nline2\n").ok();
        let tool = ReadTool::new();
        let args = serde_json::json!({"path": path}).to_string();
        let res = tool.execute(&args).await.unwrap();
        assert!(res.contains("line1") && res.contains("line2"));
        std::fs::remove_file(path).ok();
    }
}
