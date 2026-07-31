use async_openai::types::chat::{ChatCompletionTool, ChatCompletionTools, FunctionObject};
use async_trait::async_trait;
use serde::Deserialize;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::Tool;

/// 结果硬上限(防 target/ 全量遍历刷屏)
const MAX_RESULTS: usize = 100;
/// 内容搜索时单文件大小上限(跳过大文件,防 OOM/慢)
const MAX_CONTENT_FILE_SIZE: u64 = 1024 * 1024;
/// 内容匹配行截断长度(防 minified 文件长行刷屏)
const MAX_LINE_LEN: usize = 200;
/// 同步 IO 超时(防 NFS/慢盘挂死)
const SEARCH_TIMEOUT: Duration = Duration::from_secs(60);

/// 搜索文件名或文件内容(无副作用,高频只读)
/// 默认尊重 .gitignore;include_ignored=true 穿透
pub struct SearchFilesTool;

#[derive(Deserialize)]
struct Args {
    /// 搜索范围目录,默认项目根(current_dir)
    #[serde(default)]
    path: Option<String>,
    /// 文件名 glob 模式,如 "*.rs"、"**/*.toml"
    #[serde(default)]
    pattern: Option<String>,
    /// 文件内容正则表达式
    #[serde(default)]
    content: Option<String>,
    /// 穿透 .gitignore(默认 false)
    #[serde(default)]
    include_ignored: bool,
}

impl SearchFilesTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for SearchFilesTool {
    fn name(&self) -> &str {
        "search_files"
    }

    fn definition(&self) -> ChatCompletionTools {
        ChatCompletionTools::Function(ChatCompletionTool {
            function: FunctionObject {
                name: "search_files".to_string(),
                description: Some(
                    "搜索本地文件:按文件名 glob 或内容正则。无副作用。\n\
                    适用:找某文件在哪(path 不确定);找哪些文件含某文本;列目录内容(pattern=*)。\n\
                    不适用:读已知路径文件内容用 read;看命令输出用 bash;联网搜索不支持(本地专用)。\n\
                    默认尊重 .gitignore(跳过 target/、node_modules/、.git/ 等)。如需搜被忽略文件(.env、Cargo.lock、target/ 下日志),设 include_ignored=true。\n\
                    pattern 搜文件名(如 *.rs、**/*.toml);content 搜文件内正则(输出 文件:行号:行)。两者可同时用:先 glob 过滤文件名,再正则筛内容。"
                        .to_string(),
                ),
                parameters: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "搜索范围目录,默认项目根"
                        },
                        "pattern": {
                            "type": "string",
                            "description": "文件名 glob 模式,如 *.rs 或 **/*.toml"
                        },
                        "content": {
                            "type": "string",
                            "description": "文件内容正则表达式"
                        },
                        "include_ignored": {
                            "type": "boolean",
                            "description": "穿透 .gitignore 搜索被忽略文件,默认 false"
                        }
                    },
                    "required": []
                })),
                strict: None,
            },
        })
    }

    async fn execute(&self, args: &str) -> anyhow::Result<String> {
        let args: Args = serde_json::from_str(args)
            .map_err(|e| anyhow::anyhow!("search_files 参数解析失败: {e}"))?;
        // 路径解析:相对路径拼项目根,绝对路径直通;默认项目根
        let root = super::sandbox::resolve_path(&args.path.unwrap_or_else(|| ".".to_string()))?;
        let pattern = args.pattern;
        let content_re = args.content;
        let include_ignored = args.include_ignored;

        // 编译 glob/正则(在 async 侧,快速失败)
        let glob_matcher = pattern
            .as_deref()
            .map(glob::Pattern::new)
            .transpose()
            .map_err(|e| anyhow::anyhow!("glob 模式无效: {e}"))?;
        let content_matcher = content_re
            .as_deref()
            .map(regex::Regex::new)
            .transpose()
            .map_err(|e| anyhow::anyhow!("正则表达式无效: {e}"))?;

        let root = root;
        if !root.exists() {
            return Err(anyhow::anyhow!("搜索路径不存在: {}", root.display()));
        }
        let timeout_path = root.display().to_string();

        // spawn_blocking 隔离遍历(同步 IO,可能慢)
        let result = tokio::time::timeout(
            SEARCH_TIMEOUT,
            tokio::task::spawn_blocking(move || {
                search_sync(&root, glob_matcher.as_ref(), content_matcher.as_ref(), include_ignored)
            }),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!("搜索超时(>{:?},路径 {timeout_path})", SEARCH_TIMEOUT)
        })?
        .map_err(|e| {
            anyhow::anyhow!("搜索任务失败: {e:#}")
        })??;

        Ok(result)
    }
}

/// 同步搜索核心
fn search_sync(
    root: &Path,
    glob: Option<&glob::Pattern>,
    content: Option<&regex::Regex>,
    include_ignored: bool,
) -> anyhow::Result<String> {
    // ignore::WalkBuilder:ripgrep 同源,默认尊重 .gitignore/.gitignore_global/.git/info/exclude
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(!include_ignored)       // include_ignored=false 跳隐藏文件
        .ignore(!include_ignored)       // .ignore 文件
        .git_ignore(!include_ignored)   // .gitignore
        .git_global(!include_ignored)   // ~/.config/git/ignore
        .git_exclude(!include_ignored); // .git/info/exclude
    let walker = builder.build();

    let mut matches: Vec<String> = Vec::new();
    let mut truncated = false;
    let mut skipped: usize = 0; // 统计因错跳过的文件,附在结果末尾供模型参考

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => {
                skipped += 1;
                continue; // 跳过无权限/损坏的条目
            }
        };
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();

        // glob 过滤文件名
        if let Some(glob) = glob {
            let rel = path.strip_prefix(root).unwrap_or(path);
            if !glob.matches_path(rel) {
                continue;
            }
        }

        // 纯文件名搜索(无 content):收集路径
        let Some(content) = content else {
            if matches.len() >= MAX_RESULTS {
                truncated = true;
                break;
            }
            matches.push(path.display().to_string());
            continue;
        };

        // 内容搜索:读文件逐行匹配。单文件失败跳过,不中止全搜
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        if meta.len() > MAX_CONTENT_FILE_SIZE {
            continue; // 跳过大文件
        }
        let file = match File::open(path) {
            Ok(f) => f,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        let reader = BufReader::new(file);
        for (i, line) in reader.lines().enumerate() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break, // 非 UTF-8/二进制,跳过该文件
            };
            if content.is_match(&line) {
                if matches.len() >= MAX_RESULTS {
                    truncated = true;
                    break;
                }
                matches.push(format!("{}:{}:{}", path.display(), i + 1, truncate_line(&line)));
            }
        }
        if truncated {
            break;
        }
    }

    if matches.is_empty() {
        let mut msg = "无匹配".to_string();
        if skipped > 0 {
            msg.push_str(&format!("(跳过 {skipped} 个无法访问的文件)"));
        }
        return Ok(msg);
    }
    let mut out = matches.join("\n");
    if truncated {
        out.push_str(&format!("\n\n(结果已达上限 {},截断)", MAX_RESULTS));
    }
    if skipped > 0 {
        out.push_str(&format!("\n(跳过 {skipped} 个无法访问的文件)"));
    }
    Ok(out)
}

/// 按字符安全截断行(避免切在 UTF-8 多字节字符中间 panic)
/// 取前 MAX_LINE_LEN 个字符,超长加 "..."
fn truncate_line(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_LEN {
        return line.to_string();
    }
    let truncated: String = line.chars().take(MAX_LINE_LEN).collect();
    format!("{truncated}...")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup_test_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = PathBuf::from(format!("/tmp/hi_agent_search_test_{id}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.rs"), "fn main() { TODO(\"impl\") }\n").unwrap();
        fs::write(dir.join("b.txt"), "hello world\n").unwrap();
        fs::write(dir.join("c.rs"), "// TODO: fix\n").unwrap();
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/d.rs"), "fn foo() {}\n").unwrap();
        dir
    }

    #[tokio::test]
    async fn glob_search() {
        let dir = setup_test_dir();
        let tool = SearchFilesTool::new();
        let args = serde_json::json!({"path": dir, "pattern": "*.rs"}).to_string();
        let res = tool.execute(&args).await.unwrap();
        assert!(res.contains("a.rs"));
        assert!(res.contains("c.rs"));
        assert!(!res.contains("b.txt"));
        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn content_search() {
        let dir = setup_test_dir();
        let tool = SearchFilesTool::new();
        let args = serde_json::json!({"path": dir, "content": "TODO"}).to_string();
        let res = tool.execute(&args).await.unwrap();
        assert!(res.contains("a.rs:1:"));
        assert!(res.contains("c.rs:1:"));
        assert!(!res.contains("b.txt"));
        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn no_match() {
        let dir = setup_test_dir();
        let tool = SearchFilesTool::new();
        let args = serde_json::json!({"path": dir, "content": "nonexistent_xyz"}).to_string();
        let res = tool.execute(&args).await.unwrap();
        assert_eq!(res, "无匹配");
        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn nonexistent_path() {
        let tool = SearchFilesTool::new();
        let args = serde_json::json!({"path": "/tmp/nonexistent_xyz_123"}).to_string();
        let res = tool.execute(&args).await;
        assert!(res.is_err());
    }

    #[test]
    fn truncate_line_safe_on_multibyte() {
        // 300 个中文字符 = 900 字节,MAX_LINE_LEN=200 字符
        // 旧实现按字节切 [..200] 会 panic(200 落在中文 3 字节中间)
        let line: String = "中".repeat(300);
        let truncated = truncate_line(&line);
        assert!(truncated.ends_with("..."));
        assert_eq!(truncated.chars().count(), MAX_LINE_LEN + 3); // 200 字符 + "..."
    }

    #[test]
    fn truncate_line_short_unchanged() {
        let line = "short line";
        assert_eq!(truncate_line(line), line);
    }

    #[tokio::test]
    async fn content_search_skips_inaccessible() {
        // 单文件不可读不应中止整个搜索
        let dir = setup_test_dir();
        // 创建一个无读权限的文件(Unix only)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let secret = dir.join("secret.rs");
            fs::write(&secret, "TODO secret\n").unwrap();
            fs::set_permissions(&secret, fs::Permissions::from_mode(0o000)).unwrap();
        }

        let tool = SearchFilesTool::new();
        let args = serde_json::json!({"path": dir, "content": "TODO"}).to_string();
        let res = tool.execute(&args).await.unwrap();
        // 可读的 a.rs/c.rs 仍应被搜到
        assert!(res.contains("a.rs:1:"), "可读文件应被搜到: {res}");
        assert!(res.contains("c.rs:1:"), "可读文件应被搜到: {res}");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // 恢复权限以便清理
            let secret = dir.join("secret.rs");
            fs::set_permissions(&secret, fs::Permissions::from_mode(0o644)).ok();
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn invalid_regex() {
        let tool = SearchFilesTool::new();
        let args = serde_json::json!({"path": ".", "content": "[invalid"}).to_string();
        let res = tool.execute(&args).await;
        assert!(res.is_err());
        let msg = res.unwrap_err().to_string();
        assert!(msg.contains("正则"), "错误应提示正则: {msg}");
    }

    #[tokio::test]
    async fn invalid_glob() {
        let tool = SearchFilesTool::new();
        let args = serde_json::json!({"path": ".", "pattern": "[invalid"}).to_string();
        let res = tool.execute(&args).await;
        assert!(res.is_err());
        let msg = res.unwrap_err().to_string();
        assert!(msg.contains("glob"), "错误应提示 glob: {msg}");
    }
}
