use async_openai::types::chat::{ChatCompletionTool, ChatCompletionTools, FunctionObject};
use async_trait::async_trait;
use lru::LruCache;
use serde::Deserialize;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::time::SystemTime;

use super::Tool;

/// 大文件阈值:超过此大小走流式读(不缓存)
/// 256KB 覆盖 99% 代码文件,日志/数据走流式
const LARGE_FILE_THRESHOLD: u64 = 256 * 1024;

/// LRU 缓存容量:最多缓存这么多文件
/// 50 条 × 最坏 512KB(2×256KB)= ~25MB 内存上限
const CACHE_CAPACITY: usize = 50;

/// 读取文件内容(无副作用,高频只读操作)
/// 小文件走 LRU 缓存(命中零 IO),大文件走流式(防 OOM)
pub struct ReadTool {
    cache: Mutex<LruCache<String, CacheEntry>>,
}

#[derive(Deserialize)]
struct Args {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

/// 缓存条目:存 normalize 后全文 + 预分行 + mtime(失效检测)
struct CacheEntry {
    lines: Vec<String>,
    mtime: SystemTime,
    size: u64,
}

impl ReadTool {
    pub fn new() -> Self {
        // CACHE_CAPACITY 是常量 50,编译期已知非 0;expect 表意图,真出问题早崩暴露 bug
        let cache = LruCache::new(
            NonZeroUsize::new(CACHE_CAPACITY).expect("CACHE_CAPACITY must be > 0"),
        );
        Self {
            cache: Mutex::new(cache),
        }
    }

    /// 小文件路径:查缓存 → 命中切片,未命中全读入缓存
    fn read_cached(
        &self,
        path: &str,
        offset: usize,
        limit: Option<usize>,
        meta: std::fs::Metadata,
    ) -> anyhow::Result<String> {
        let size = meta.len();
        let mtime = meta.modified()?;

        // 查缓存(mtime + size 检测失效)
        let cached = {
            let mut cache = self.lock_cache()?;
            cache
                .get(path)
                .filter(|e| e.mtime == mtime && e.size == size)
                .map(|e| e.lines.clone())
        };

        // 命中:直接切片返,不重读
        if let Some(lines) = cached {
            return Ok(format_lines(&lines, offset, limit));
        }

        // 未命中/失效:读文件(不持锁,避免锁内长 IO)
        let bytes = std::fs::read(path)
            .map_err(|e| anyhow::anyhow!("读取失败 {}: {e}", path))?;
        let lines = decode_and_split(&bytes, path)?;

        // 入缓存
        let mut cache = self.lock_cache()?;
        cache.put(
            path.to_string(),
            CacheEntry {
                lines: lines.clone(),
                mtime,
                size,
            },
        );

        Ok(format_lines(&lines, offset, limit))
    }

    /// 锁缓存,锁中毒转 Err 而非 panic
    fn lock_cache(&self) -> anyhow::Result<std::sync::MutexGuard<LruCache<String, CacheEntry>>> {
        self.cache
            .lock()
            .map_err(|e| anyhow::anyhow!("缓存锁中毒: {e}"))
    }

    /// 大文件路径:流式 BufReader 顺序扫到 offset,读 limit 行(防 OOM)
    fn read_streaming(
        &self,
        path: &str,
        offset: usize,
        limit: Option<usize>,
    ) -> anyhow::Result<String> {
        let file = File::open(path).map_err(|e| anyhow::anyhow!("打开失败 {}: {e}", path))?;
        let reader = BufReader::new(file);
        let start = offset.saturating_sub(1);
        let end = limit.map(|l| start + l);

        let mut out = String::new();
        let mut line_no = 0;
        for line in reader.lines() {
            line_no += 1;
            if line_no <= start {
                continue;
            }
            if let Some(end) = end {
                if line_no > end {
                    break;
                }
            }
            // lines() 解码失败说明非 UTF-8(含二进制),统一报错带引导
            let line = line.map_err(|e| anyhow::anyhow!(
                "读取失败 {}: {e}。\n\
                 可能是二进制(用 bash `file {}` 查类型)或非 UTF-8 编码(可用 `iconv` 转换)",
                path, path
            ))?;
            let line = line.trim_end_matches('\r');
            out.push_str(&format!("{line_no:>6}\t{line}\n"));
        }
        Ok(out)
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

    async fn execute(&self, args: &str) -> anyhow::Result<String> {
        let args: Args = serde_json::from_str(args)?;
        let offset = args.offset.unwrap_or(1);

        let meta = std::fs::metadata(&args.path)
            .map_err(|e| anyhow::anyhow!("访问失败 {}: {e}", args.path))?;

        if meta.len() > LARGE_FILE_THRESHOLD {
            self.read_streaming(&args.path, offset, args.limit)
        } else {
            self.read_cached(&args.path, offset, args.limit, meta)
        }
    }
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
        Some(l) => (start + l).min(lines.len()),
        None => lines.len(),
    };
    let mut out = String::new();
    for (i, line) in lines[start..end].iter().enumerate() {
        let line_no = start + i + 1;
        out.push_str(&format!("{line_no:>6}\t{line}\n"));
    }
    out
}
