use async_openai::types::chat::{ChatCompletionTool, ChatCompletionTools, FunctionObject};
use async_trait::async_trait;
use serde::Deserialize;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::Tool;
use super::sandbox::ensure_within_sandbox;

/// 内容大小上限(防模型生成超大内容拖慢/占内存)
const MAX_CONTENT_SIZE: usize = 10 * 1024 * 1024;
/// 同步 IO 超时
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);

/// 创建或覆盖写文件(有副作用,危险操作)
/// 已存在文件需显式 overwrite=true,强制模型区分新建 vs 覆盖,防误丢内容
pub struct WriteTool;

#[derive(Deserialize)]
struct Args {
    path: String,
    content: String,
    #[serde(default)]
    overwrite: bool,
}

impl WriteTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn definition(&self) -> ChatCompletionTools {
        ChatCompletionTools::Function(ChatCompletionTool {
            function: FunctionObject {
                name: "write".to_string(),
                description: Some(
                    "创建或完全覆盖写文件。高风险。\n\
                    新建文件直接写;已存在文件需 overwrite=true(否则报错,防误丢内容)。\n\
                    改局部用 edit,更安全。\n\
                    受沙箱白名单限制(项目根 + 启动时配置的额外路径);拒越界、.git/;软链按真实目标校验。\n\
                    路径越界会报错,按提示调整。"
                        .to_string(),
                ),
                parameters: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "文件路径:相对项目根、绝对路径、或 ~/开头(展开家目录)"
                        },
                        "content": {
                            "type": "string",
                            "description": "要写入的完整内容"
                        },
                        "overwrite": {
                            "type": "boolean",
                            "description": "已存在文件是否覆盖,默认 false。false 时若文件已存在则报错"
                        }
                    },
                    "required": ["path", "content"]
                })),
                strict: None,
            },
        })
    }

    async fn execute(&self, args: &str) -> anyhow::Result<String> {
        let args: Args = serde_json::from_str(args)
            .map_err(|e| anyhow::anyhow!("write 参数解析失败: {e}"))?;

        if args.content.len() > MAX_CONTENT_SIZE {
            return Err(anyhow::anyhow!(
                "内容过大: {} 字节,上限 {} 字节",
                args.content.len(),
                MAX_CONTENT_SIZE
            ));
        }

        // 路径校验在 write_sync 内(ensure_within_sandbox 接 str,内部解析+校验)
        let path_str = args.path;
        let timeout_path = path_str.clone();
        let content = args.content;
        let overwrite = args.overwrite;

        // spawn_blocking 隔离同步 IO,超时兜底
        let result = tokio::time::timeout(
            WRITE_TIMEOUT,
            tokio::task::spawn_blocking(move || write_sync(&path_str, &content, overwrite)),
        )
        .await
        .map_err(|_| anyhow::anyhow!("写入超时(>{:?},路径 {})", WRITE_TIMEOUT, timeout_path))?
        .map_err(|e| anyhow::anyhow!("写入任务失败: {e:#}"))??;

        Ok(result)
    }
}

/// 同步写入核心:沙箱校验 → 检查存在性 → 原子写 → 权限保留
fn write_sync(path_str: &str, content: &str, overwrite: bool) -> anyhow::Result<String> {
    // 1. 沙箱校验(内部 resolve_path + canonicalize + 白名单;拒越界/拒 .git/解析软链)
    let real_path = ensure_within_sandbox(path_str)?;

    // 显式拒绝目录(write 只写文件,不写目录;若需建目录用 bash mkdir)
    if real_path.is_dir() {
        return Err(anyhow::anyhow!(
            "目标是目录不是文件: {}(write 只写文件;若需创建目录用 bash mkdir)",
            real_path.display()
        ));
    }

    // 2. 检查文件是否存在 + overwrite 语义
    let existed = real_path.exists();
    if existed && !overwrite {
        return Err(anyhow::anyhow!(
            "文件已存在: {}(需 overwrite=true 才覆盖)",
            real_path.display()
        ));
    }

    // 3. 保留原文件权限(覆盖时)
    #[cfg(unix)]
    let old_perms = if existed {
        std::fs::metadata(&real_path).ok().map(|m| m.permissions())
    } else {
        None
    };
    #[cfg(not(unix))]
    let old_perms: Option<std::fs::Permissions> = None;

    // 4. 原子写:写临时文件 → fsync → rename
    //    崩溃时:临时文件残留,目标文件完整(旧或新,不会半残)
    //    临时文件放同目录(rename 跨目录非原子),文件名前缀 . 避免 glob 误匹配
    let file_name = real_path.file_name().ok_or_else(|| {
        anyhow::anyhow!("路径无文件名: {}", real_path.display())
    })?;
    let tmp_name = format!(".{}.tmp", file_name.to_string_lossy());
    let tmp_path = real_path.with_file_name(tmp_name);
    let bytes_written = atomic_write(&real_path, &tmp_path, content.as_bytes())?;

    // 5. 恢复原权限(覆盖时)
    #[cfg(unix)]
    if let Some(perms) = old_perms {
        std::fs::set_permissions(&real_path, perms).ok();
    }

    let action = if existed { "覆盖" } else { "新建" };
    Ok(format!("已{} {}({} 字节)", action, real_path.display(), bytes_written))
}

/// 原子写:临时文件 → fsync → rename(Unix rename 原子)
/// 崩溃时目标文件保持完整(旧或新之一,不会半残)
fn atomic_write(target: &Path, tmp: &Path, data: &[u8]) -> anyhow::Result<usize> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(tmp)
        .map_err(|e| anyhow::anyhow!("创建临时文件失败 {}: {e}", tmp.display()))?;

    file.write_all(data)
        .map_err(|e| anyhow::anyhow!("写入临时文件失败 {}: {e}", tmp.display()))?;
    file.sync_all()
        .map_err(|e| anyhow::anyhow!("fsync 临时文件失败 {}: {e}", tmp.display()))?;
    drop(file); // 关闭 fd,Windows 上 rename 前需关闭

    std::fs::rename(tmp, target)
        .map_err(|e| {
            std::fs::remove_file(tmp).ok(); // rename 失败清理临时文件
            anyhow::anyhow!("rename 失败 {} -> {}: {e}", tmp.display(), target.display())
        })?;

    Ok(data.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;

    // 全局 PROJECT_ROOT 跨测试共享,并发跑会互相干扰,用 Mutex 序列化
    static ROOT_LOCK: Mutex<()> = Mutex::new(());

    fn setup_write_test() -> PathBuf {
        let dir = PathBuf::from("/tmp/hi_agent_write_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(dir.join("existing.txt"), "old content").unwrap();
        super::super::sandbox::set_project_root(dir.clone()).unwrap();
        dir
    }

    fn cleanup(dir: &Path) {
        fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn create_new_file() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_write_test();
        let path = dir.join("new.txt");
        let tool = WriteTool::new();
        let args = serde_json::json!({
            "path": path,
            "content": "hello"
        }).to_string();
        let res = tool.execute(&args).await.unwrap();
        assert!(res.contains("新建"), "应提示新建: {res}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello");
        cleanup(&dir);
    }

    #[tokio::test]
    async fn reject_overwrite_without_flag() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_write_test();
        let path = dir.join("existing.txt");
        let tool = WriteTool::new();
        let args = serde_json::json!({
            "path": path,
            "content": "new"
        }).to_string();
        let res = tool.execute(&args).await;
        assert!(res.is_err(), "无 overwrite 应拒绝");
        // 原内容应保留
        assert_eq!(fs::read_to_string(dir.join("existing.txt")).unwrap(), "old content");
        cleanup(&dir);
    }

    #[tokio::test]
    async fn overwrite_with_flag() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_write_test();
        let path = dir.join("existing.txt");
        let tool = WriteTool::new();
        let args = serde_json::json!({
            "path": path,
            "content": "new content",
            "overwrite": true
        }).to_string();
        let res = tool.execute(&args).await.unwrap();
        assert!(res.contains("覆盖"), "应提示覆盖: {res}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "new content");
        cleanup(&dir);
    }

    #[tokio::test]
    async fn reject_outside_sandbox() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_write_test();
        let tool = WriteTool::new();
        let args = serde_json::json!({
            "path": "/etc/passwd",
            "content": "hacked"
        }).to_string();
        let res = tool.execute(&args).await;
        assert!(res.is_err(), "项目外应拒绝");
        let msg = res.unwrap_err().to_string();
        assert!(msg.contains("越界"), "应提示越界: {msg}");
        cleanup(&dir);
    }

    #[tokio::test]
    async fn reject_directory() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_write_test();
        // 建一个子目录用于测试
        let subdir = dir.join("subdir");
        fs::create_dir_all(&subdir).unwrap();
        let tool = WriteTool::new();
        let args = serde_json::json!({
            "path": subdir,
            "content": "hello",
            "overwrite": true
        }).to_string();
        let res = tool.execute(&args).await;
        assert!(res.is_err(), "目录应拒绝");
        let msg = res.unwrap_err().to_string();
        assert!(msg.contains("目录"), "应提示目录: {msg}");
        cleanup(&dir);
    }

    #[tokio::test]
    async fn reject_dot_git() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_write_test();
        let path = dir.join(".git").join("HEAD");
        let tool = WriteTool::new();
        let args = serde_json::json!({
            "path": path,
            "content": "hacked"
        }).to_string();
        let res = tool.execute(&args).await;
        assert!(res.is_err(), ".git 应拒绝");
        let msg = res.unwrap_err().to_string();
        assert!(msg.contains(".git"), "应提示 .git: {msg}");
        cleanup(&dir);
    }

    #[tokio::test]
    async fn reject_nonexistent_parent() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_write_test();
        let path = dir.join("nonexistent").join("file.txt");
        let tool = WriteTool::new();
        let args = serde_json::json!({
            "path": path,
            "content": "hello"
        }).to_string();
        let res = tool.execute(&args).await;
        assert!(res.is_err(), "父目录不存在应拒绝");
        cleanup(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn preserve_permissions_on_overwrite() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_write_test();
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("existing.txt");
        // 设为 0o600
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        let tool = WriteTool::new();
        let args = serde_json::json!({
            "path": path,
            "content": "new",
            "overwrite": true
        }).to_string();
        tool.execute(&args).await.unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "权限应保留 0600,实际 {:o}", mode & 0o777);
        cleanup(&dir);
    }

    #[tokio::test]
    async fn reject_too_large_content() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_write_test();
        let path = dir.join("big.txt");
        let tool = WriteTool::new();
        let big = "x".repeat(MAX_CONTENT_SIZE + 1);
        let args = serde_json::json!({
            "path": path,
            "content": big
        }).to_string();
        let res = tool.execute(&args).await;
        assert!(res.is_err(), "超大内容应拒绝");
        let msg = res.unwrap_err().to_string();
        assert!(msg.contains("过大"), "应提示过大: {msg}");
        cleanup(&dir);
    }
}
