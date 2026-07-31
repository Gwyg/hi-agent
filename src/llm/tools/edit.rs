use async_openai::types::chat::{ChatCompletionTool, ChatCompletionTools, FunctionObject};
use async_trait::async_trait;
use serde::Deserialize;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use super::Tool;
use super::sandbox::ensure_within_sandbox;

/// 文件大小上限(防大文件占内存;超大文件建议 write 重写或 bash sed)
const MAX_FILE_SIZE: usize = 10 * 1024 * 1024;
/// 同步 IO 超时
const EDIT_TIMEOUT: Duration = Duration::from_secs(120);

/// 对文件做精确局部编辑(替换文本片段)。有副作用,中危。
/// old_string 须精确匹配;expected_count 校验实际数量(防盲区误替换),默认 1(唯一匹配)
pub struct EditTool;

#[derive(Deserialize)]
struct Args {
    path: String,
    old_string: String,
    new_string: String,
    /// 预期匹配数量。None=默认1(唯一匹配);Some(n)=批量替换 n 处(需先 read 全文确认)
    #[serde(default)]
    expected_count: Option<usize>,
}

impl EditTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn definition(&self) -> ChatCompletionTools {
        ChatCompletionTools::Function(ChatCompletionTool {
            function: FunctionObject {
                name: "edit".to_string(),
                description: Some(
                    "对文件做精确局部编辑(替换文本片段)。有副作用,中危。\n\
                    适用:改文件局部代码/配置;重构;修 bug。\n\
                    不适用:新建文件用 write;完全重写文件用 write(overwrite=true);删整段用 edit(old_string=段,new_string=空)。\n\
                    old_string 须精确匹配文件内容(含空格换行,区分大小写)。\n\
                    expected_count 校验匹配数量,防盲区误替换:不传=默认1(唯一匹配,最常见);批量替换传预期数量。预期与实际不符报错,按提示读文件确认或精确 old_string。\n\
                    受沙箱白名单限制(项目根 + 启动时配置的额外路径);拒越界、.git/;软链按真实目标校验。"
                        .to_string(),
                ),
                parameters: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "文件路径:相对项目根、绝对路径、或 ~/开头(展开家目录)"
                        },
                        "old_string": {
                            "type": "string",
                            "description": "要替换的精确文本(须在文件中匹配,含空格换行)"
                        },
                        "new_string": {
                            "type": "string",
                            "description": "替换后的文本"
                        },
                        "expected_count": {
                            "type": "integer",
                            "description": "预期匹配数量。不传=默认1(唯一匹配)。批量替换传预期数量。预期与实际不符报错,按提示读文件确认或精确 old_string"
                        }
                    },
                    "required": ["path", "old_string", "new_string"]
                })),
                strict: None,
            },
        })
    }

    async fn execute(&self, args: &str) -> anyhow::Result<String> {
        let args: Args = serde_json::from_str(args)
            .map_err(|e| anyhow::anyhow!("edit 参数解析失败: {e}"))?;

        // 防无意义编辑(模型误用)
        if args.old_string == args.new_string {
            return Err(anyhow::anyhow!("old_string 与 new_string 相同,无意义"));
        }

        let path_str = args.path;
        let timeout_path = path_str.clone();
        let old = args.old_string;
        let new = args.new_string;
        let expected_count = args.expected_count;

        let result = tokio::time::timeout(
            EDIT_TIMEOUT,
            tokio::task::spawn_blocking(move || edit_sync(&path_str, &old, &new, expected_count)),
        )
        .await
        .map_err(|_| anyhow::anyhow!("编辑超时(>{:?},路径 {})", EDIT_TIMEOUT, timeout_path))?
        .map_err(|e| anyhow::anyhow!("编辑任务失败: {e:#}"))??;

        Ok(result)
    }
}

/// 同步编辑核心:沙箱校验 → 读文件 → 数量校验 + 替换 → 原子写回
fn edit_sync(path_str: &str, old: &str, new: &str, expected_count: Option<usize>) -> anyhow::Result<String> {
    // 1. 沙箱校验(复用 sandbox 模块)
    let real_path = ensure_within_sandbox(path_str)?;

    // 2. 拒目录(edit 只改文件)
    if real_path.is_dir() {
        return Err(anyhow::anyhow!(
            "目标是目录不是文件: {}(edit 只改文件)",
            real_path.display()
        ));
    }

    // 3. 文件必须存在(edit 改已有文件,不新建)
    if !real_path.exists() {
        return Err(anyhow::anyhow!(
            "文件不存在: {}(用 write 创建新文件)",
            real_path.display()
        ));
    }

    // 4. 读文件
    let content = std::fs::read_to_string(&real_path)
        .map_err(|e| anyhow::anyhow!("读取失败 {}: {e}", real_path.display()))?;
    if content.len() > MAX_FILE_SIZE {
        return Err(anyhow::anyhow!(
            "文件过大: {} 字节,上限 {} 字节(用 write 重写或 bash sed)",
            content.len(),
            MAX_FILE_SIZE
        ));
    }

    // 5. 匹配 + 数量校验(防盲区误替换)
    let actual_count = content.matches(old).count();
    if actual_count == 0 {
        return Err(anyhow::anyhow!(
            "找不到 old_string(检查是否精确匹配:空格、换行、大小写)"
        ));
    }
    let expected = expected_count.unwrap_or(1);
    if actual_count != expected {
        return Err(anyhow::anyhow!(
            "预期 {} 处匹配,实际 {} 处,拒绝(防盲区误替换;读更多的文本确认实际数量,或提供更精确 old_string)",
            expected,
            actual_count
        ));
    }

    // 数量匹配,替换 expected 处
    let new_content = content.replacen(old, new, expected);

    // 6. 保留原权限
    #[cfg(unix)]
    let old_perms = std::fs::metadata(&real_path).ok().map(|m| m.permissions());
    #[cfg(not(unix))]
    let old_perms: Option<std::fs::Permissions> = None;

    // 7. 原子写回(临时文件 + fsync + rename,防崩溃半残)
    let file_name = real_path.file_name().ok_or_else(|| {
        anyhow::anyhow!("路径无文件名: {}", real_path.display())
    })?;
    let tmp_name = format!(".{}.tmp", file_name.to_string_lossy());
    let tmp_path = real_path.with_file_name(tmp_name);
    atomic_write(&real_path, &tmp_path, new_content.as_bytes())?;

    // 8. 恢复权限
    #[cfg(unix)]
    if let Some(perms) = old_perms {
        std::fs::set_permissions(&real_path, perms).ok();
    }

    let old_bytes = content.len();
    let new_bytes = new_content.len();
    Ok(format!(
        "已编辑 {}({} 处替换,{} → {} 字节)",
        real_path.display(),
        expected,
        old_bytes,
        new_bytes
    ))
}

/// 原子写:临时文件 → fsync → rename
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
    drop(file);

    std::fs::rename(tmp, target)
        .map_err(|e| {
            std::fs::remove_file(tmp).ok();
            anyhow::anyhow!("rename 失败 {} -> {}: {e}", tmp.display(), target.display())
        })?;

    Ok(data.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Mutex;

    static ROOT_LOCK: Mutex<()> = Mutex::new(());

    fn setup_edit_test() -> PathBuf {
        let dir = PathBuf::from("/tmp/hi_agent_edit_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("foo.txt"), "hello world\nhello rust\n").unwrap();
        super::super::sandbox::set_project_root(dir.clone()).unwrap();
        super::super::sandbox::set_extra_allowed(super::super::sandbox::default_extra_paths()).unwrap();
        dir
    }

    fn cleanup(dir: &Path) {
        fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn edit_unique_match() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_edit_test();
        let path = dir.join("foo.txt");
        let tool = EditTool::new();
        let args = serde_json::json!({
            "path": path,
            "old_string": "world",
            "new_string": "WORLD"
        }).to_string();
        let res = tool.execute(&args).await.unwrap();
        assert!(res.contains("1 处替换"), "{}", res);
        assert_eq!(fs::read_to_string(dir.join("foo.txt")).unwrap(), "hello WORLD\nhello rust\n");
        cleanup(&dir);
    }

    #[tokio::test]
    async fn edit_batch_with_expected_count() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_edit_test();
        let path = dir.join("foo.txt");
        let tool = EditTool::new();
        // "hello" 出现 2 处,传 expected_count=2 批量替换
        let args = serde_json::json!({
            "path": path,
            "old_string": "hello",
            "new_string": "hi",
            "expected_count": 2
        }).to_string();
        let res = tool.execute(&args).await.unwrap();
        assert!(res.contains("2 处替换"), "{}", res);
        assert_eq!(fs::read_to_string(dir.join("foo.txt")).unwrap(), "hi world\nhi rust\n");
        cleanup(&dir);
    }

    #[tokio::test]
    async fn reject_count_mismatch() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_edit_test();
        let path = dir.join("foo.txt");
        let tool = EditTool::new();
        // "hello" 实际 2 处,模型传 expected_count=1(基于部分阅读)→ 应拒
        let args = serde_json::json!({
            "path": path,
            "old_string": "hello",
            "new_string": "hi",
            "expected_count": 1
        }).to_string();
        let res = tool.execute(&args).await;
        assert!(res.is_err(), "数量不符应拒");
        let msg = res.unwrap_err().to_string();
        assert!(msg.contains("预期 1 处"), "{}", msg);
        assert!(msg.contains("实际 2 处"), "{}", msg);
        // 原内容不变
        assert_eq!(fs::read_to_string(dir.join("foo.txt")).unwrap(), "hello world\nhello rust\n");
        cleanup(&dir);
    }

    #[tokio::test]
    async fn reject_multiple_default() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_edit_test();
        let path = dir.join("foo.txt");
        let tool = EditTool::new();
        // 不传 expected_count(默认1),"hello" 实际 2 处 → 报错
        let args = serde_json::json!({
            "path": path,
            "old_string": "hello",
            "new_string": "hi"
        }).to_string();
        let res = tool.execute(&args).await;
        assert!(res.is_err(), "多处匹配默认应拒");
        let msg = res.unwrap_err().to_string();
        assert!(msg.contains("预期 1 处"), "{}", msg);
        cleanup(&dir);
    }

    #[tokio::test]
    async fn reject_no_match() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_edit_test();
        let path = dir.join("foo.txt");
        let tool = EditTool::new();
        let args = serde_json::json!({
            "path": path,
            "old_string": "nonexistent",
            "new_string": "x"
        }).to_string();
        let res = tool.execute(&args).await;
        assert!(res.is_err(), "无匹配应拒");
        let msg = res.unwrap_err().to_string();
        assert!(msg.contains("找不到"), "{}", msg);
        cleanup(&dir);
    }

    #[tokio::test]
    async fn reject_same_string() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_edit_test();
        let path = dir.join("foo.txt");
        let tool = EditTool::new();
        let args = serde_json::json!({
            "path": path,
            "old_string": "hello",
            "new_string": "hello"
        }).to_string();
        let res = tool.execute(&args).await;
        assert!(res.is_err(), "相同字符串应拒");
        cleanup(&dir);
    }

    #[tokio::test]
    async fn reject_nonexistent_file() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_edit_test();
        let path = dir.join("nonexist.txt");
        let tool = EditTool::new();
        let args = serde_json::json!({
            "path": path,
            "old_string": "a",
            "new_string": "b"
        }).to_string();
        let res = tool.execute(&args).await;
        assert!(res.is_err(), "文件不存在应拒");
        let msg = res.unwrap_err().to_string();
        assert!(msg.contains("不存在"), "{}", msg);
        cleanup(&dir);
    }

    #[tokio::test]
    async fn reject_directory() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_edit_test();
        let subdir = dir.join("subdir");
        fs::create_dir_all(&subdir).unwrap();
        let tool = EditTool::new();
        let args = serde_json::json!({
            "path": subdir,
            "old_string": "a",
            "new_string": "b"
        }).to_string();
        let res = tool.execute(&args).await;
        assert!(res.is_err(), "目录应拒");
        let msg = res.unwrap_err().to_string();
        assert!(msg.contains("目录"), "{}", msg);
        cleanup(&dir);
    }

    #[tokio::test]
    async fn delete_via_empty_new() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_edit_test();
        let path = dir.join("foo.txt");
        let tool = EditTool::new();
        let args = serde_json::json!({
            "path": path,
            "old_string": "hello world\n",
            "new_string": ""
        }).to_string();
        let res = tool.execute(&args).await.unwrap();
        assert!(res.contains("1 处替换"), "{}", res);
        assert_eq!(fs::read_to_string(dir.join("foo.txt")).unwrap(), "hello rust\n");
        cleanup(&dir);
    }
}
