use async_trait::async_trait;
use serde::Deserialize;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::time::Duration;

use super::support::path_scope::{
    has_sensitive_suffix, is_allowed_path, is_sensitive_path, workspace_root,
};
use super::{Action, Tool};
use crate::ai::llm::types::ToolDef;
use crate::util::{atomic_write, canonical_path, resolve_lexical};

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

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "write",
            format!(
                "创建或完全覆盖写文件。有副作用,高风险。\n\
                 适用:新建文件;整份重写已存在的文件(overwrite=true);生成配置 / 脚本 / 文档。\n\
                 不适用:只改文件局部用 edit(不必先读全文,未涉及的内容不会丢);看文件内容用 read;\
                 建目录 / 改权限 / 移动删除用 bash。\n\
                 新建直接写;已存在必须 overwrite=true,否则报错——防止一次误写抹掉原内容。\n\
                 目标必须是普通文件(目录 / 设备 / 管道一律拒);父目录不存在会自动创建。\n\
                 单次内容上限 {mb}MB,超出报错:更大的内容先 write 骨架,再用 edit 分段补。\n\
                 范围:项目根与配置的额外允许目录内直接写;越界、.git/ 每次都要用户确认;\
                 密钥 / 凭据类文件(.env、.pem、id_rsa、~/.ssh/ 等)不由本工具写入,\
                 用户确实需要时改用 bash 命令(会向用户确认)。\n\
                 软链写到真实目标,以返回结果里的路径为准。",
                mb = MAX_CONTENT_SIZE / (1024 * 1024)
            ),
            serde_json::json!({
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
            }),
        )
    }

    async fn assess(&self, args: &str) -> Action {
        let Ok(args) = serde_json::from_str::<Args>(args) else {
            return Action::Deny("write 参数解析失败".into());
        };
        // 敏感路径 / 后缀直接拒,不给确认机会:这类文件装的是密钥凭据,覆盖即不可恢复。
        // 排在白名单之前——项目根内的 `.env` 同样命中白名单,不能因此放行。
        // 文案明确「本工具不允许」并给出 bash 这条出路:否则模型会把工具的局部限制
        // 复述成「密钥文件一律不能由助手改」,平白拦住用户本可完成的写入。
        if is_sensitive_path(&args.path) || has_sensitive_suffix(&args.path) {
            return Action::Deny(format!(
                "write 工具不允许写敏感文件: {} —— 密钥 / 凭据类文件不由本工具改写；\
                 用户确实需要时，可通过 bash 命令完成（会向用户确认）",
                args.path
            ));
        }
        // 白名单内 = 用户已授权,直接写;白名单外每次必问,不记忆
        if is_allowed_path(&args.path) {
            Action::Allow
        } else {
            Action::AskAlways
        }
    }

    async fn execute(&self, args: &str) -> anyhow::Result<String> {
        let args: Args =
            serde_json::from_str(args).map_err(|e| anyhow::anyhow!("write 参数解析失败: {e}"))?;

        if args.content.len() > MAX_CONTENT_SIZE {
            let mb = MAX_CONTENT_SIZE / (1024 * 1024);
            return Err(anyhow::anyhow!(
                "内容过大: {:.1}MB,上限 {mb}MB —— 单次写入不要超过 {mb}MB;大文件改用 bash 分段追加,\
                 或先 write 骨架再用 edit 补内容",
                args.content.len() as f64 / (1024.0 * 1024.0)
            ));
        }

        // 同步 IO 交给工作线程,超时兜底
        let timeout_path = args.path.clone();
        tokio::time::timeout(
            WRITE_TIMEOUT,
            tokio::task::spawn_blocking(move || {
                write_file(&args.path, &args.content, args.overwrite)
            }),
        )
        .await
        .map_err(|_| {
            // 到点只是丢掉 JoinHandle,已开始的阻塞写不会停:可能仍在进行,不能断言失败
            anyhow::anyhow!(
                "写入超过 {:?} 未返回: {timeout_path} —— 阻塞 IO 无法中止,这次写入可能仍在进行;\
                 重试前先 read 确认当前内容,不要直接重试",
                WRITE_TIMEOUT
            )
        })?
        .map_err(|e| anyhow::anyhow!("写入任务失败: {e}"))?
    }
}

/// 写入核心:解析真实落点 → 判定目标状态 → 原子写。
/// 授权不在这里(那是 assess 的事),只回答「写到哪、能不能写」。
///
/// 软链照常跟随:assess 的三个判定都经 `resolve_key` 取过真实落点,越界或敏感早被拦下,
/// 这里再拒一次既不增加安全性,又与「判真实路径」的语义分叉。真实落点会由返回值回报。
fn write_file(path_str: &str, content: &str, overwrite: bool) -> anyhow::Result<String> {
    let root = workspace_root()?;
    let resolved = resolve_lexical(path_str, Some(&root))?;
    let path = PathBuf::from(
        canonical_path(&resolved)
            .map_err(|e| anyhow::anyhow!("无法解析写入路径(工作目录可能已失效): {e}"))?,
    );

    // 一次 stat 定目标状态:非普通文件一律拒——rename 会把管道 / socket / 设备替换成普通文件。
    // stat 失败(权限不足、路径过长)不能当成「不存在」,否则 overwrite 防线被绕过
    let existed = match std::fs::metadata(&path) {
        Ok(m) if !m.is_file() => {
            return Err(anyhow::anyhow!(
                "目标不是普通文件(目录 / 管道 / 设备等): {}",
                path.display()
            ))
        }
        Ok(_) if !overwrite => {
            return Err(anyhow::anyhow!(
                "文件已存在: {}(需 overwrite=true 才覆盖)",
                path.display()
            ))
        }
        Ok(_) => true,
        Err(e) if e.kind() == ErrorKind::NotFound => false,
        Err(e) => return Err(anyhow::anyhow!("无法读取目标状态 {}: {e}", path.display())),
    };

    // 原子写:半截文件对「覆盖已有内容」是不可恢复的故障。父目录由它一并确保,
    // 权限也由它沿用原文件的(脚本的可执行位不会因覆盖而丢);新建则跟随默认
    atomic_write(&path, content.as_bytes())
        .map_err(|e| anyhow::anyhow!("写入失败 {}: {e}", path.display()))?;

    Ok(format!(
        "已{} {}({} 字节)",
        if existed { "覆盖" } else { "新建" },
        path.display(),
        content.len()
    ))
}

#[cfg(test)]
mod tests {
    // 用例要独占全局工作目录根,只能拿着锁跑;`#[tokio::test]` 是 current_thread,
    // 且 await 期间没有任何代码再去取这把锁,不存在死锁
    #![allow(clippy::await_holding_lock)]

    use super::*;
    use std::fs;
    use std::path::Path;

    use crate::ai::llm::tools::support::path_scope::TEST_ROOT_LOCK as ROOT_LOCK;
    use crate::config::path_scope::{init, set_root, PathScopeConfig};
    use crate::util::{canonical_path, strip_verbatim};

    fn setup_write_test() -> PathBuf {
        // 绝对路径:边界根不做相对路径猜测(生产里它由启动流程算好再注入)
        let dir = std::env::temp_dir().join("hi_agent_write_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(dir.join("existing.txt"), "old content").unwrap();
        // 清单注入一次(重复调用只告警),工作目录根单独注入——启动流程也是这么分的:
        // 清单来自配置,根要先查库才知道。两边都不落盘,测试不能动用户的 app.toml。
        init(&PathScopeConfig::default());
        let root = canonical_path(&dir.display().to_string()).expect("解析测试目录");
        set_root(&strip_verbatim(&root)).expect("注入工作目录根");
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
        })
        .to_string();
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
        })
        .to_string();
        let res = tool.execute(&args).await;
        assert!(res.is_err(), "无 overwrite 应拒绝");
        // 原内容应保留
        assert_eq!(
            fs::read_to_string(dir.join("existing.txt")).unwrap(),
            "old content"
        );
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
        })
        .to_string();
        let res = tool.execute(&args).await.unwrap();
        assert!(res.contains("覆盖"), "应提示覆盖: {res}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "new content");
        cleanup(&dir);
    }

    /// 越界在授权阶段就要问,不等到执行
    #[tokio::test]
    async fn assess_asks_outside_scope() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_write_test();
        let tool = WriteTool::new();
        let args = serde_json::json!({
            "path": "/etc/passwd",
            "content": "hacked"
        })
        .to_string();
        assert!(
            matches!(tool.assess(&args).await, Action::AskAlways),
            "项目外应在授权阶段要求确认"
        );
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
        })
        .to_string();
        let res = tool.execute(&args).await;
        assert!(res.is_err(), "目录应拒绝");
        let msg = res.unwrap_err().to_string();
        assert!(msg.contains("目录"), "应提示目录: {msg}");
        cleanup(&dir);
    }

    /// socket / 管道不是「写进去」的对象:rename 会把它们替换成普通文件,必须提前拒
    #[cfg(unix)]
    #[tokio::test]
    async fn reject_socket_target() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_write_test();
        let sock = dir.join("s.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();

        let tool = WriteTool::new();
        let args = serde_json::json!({
            "path": sock,
            "content": "x",
            "overwrite": true
        })
        .to_string();
        let msg = tool.execute(&args).await.unwrap_err().to_string();
        assert!(msg.contains("不是普通文件"), "socket 应被拒: {msg}");
        assert!(sock.exists(), "socket 不应被替换成普通文件");

        cleanup(&dir);
    }

    /// `.git` 段在白名单判定里被排除,同样落在授权阶段
    #[tokio::test]
    async fn assess_asks_dot_git() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_write_test();
        let path = dir.join(".git").join("HEAD");
        let tool = WriteTool::new();
        let args = serde_json::json!({
            "path": path,
            "content": "hacked"
        })
        .to_string();
        assert!(
            matches!(tool.assess(&args).await, Action::AskAlways),
            ".git 应在授权阶段要求确认"
        );
        cleanup(&dir);
    }

    /// 父目录是写文件的前置条件,缺了直接建,不让模型多跑一轮 mkdir
    #[tokio::test]
    async fn creates_missing_parent_dirs() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_write_test();
        let path = dir.join("nonexistent").join("deeper").join("file.txt");
        let tool = WriteTool::new();
        let args = serde_json::json!({
            "path": path,
            "content": "hello"
        })
        .to_string();
        tool.execute(&args).await.unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello");
        cleanup(&dir);
    }

    /// 软链跟随到真实落点:assess 判的就是它,写也只能写它,并把它回报给模型
    #[cfg(unix)]
    #[tokio::test]
    async fn follows_symlink_to_real_target() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_write_test();
        let real = dir.join("real.txt");
        fs::write(&real, "old").unwrap();
        let link = dir.join("link.txt");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let tool = WriteTool::new();
        let args = serde_json::json!({
            "path": link,
            "content": "new",
            "overwrite": true
        })
        .to_string();
        let res = tool.execute(&args).await.unwrap();
        assert_eq!(fs::read_to_string(&real).unwrap(), "new", "应写到软链目标");
        assert!(
            res.contains("real.txt"),
            "返回值应给出真实落点,不能只回软链名: {res}"
        );
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
        })
        .to_string();
        tool.execute(&args).await.unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "权限应保留 0600,实际 {:o}",
            mode & 0o777
        );
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
        })
        .to_string();
        let res = tool.execute(&args).await;
        assert!(res.is_err(), "超大内容应拒绝");
        let msg = res.unwrap_err().to_string();
        assert!(msg.contains("过大"), "应提示过大: {msg}");
        cleanup(&dir);
    }

    /// `.env` 落在项目根内(命中白名单),但属于敏感后缀,必须在授权阶段就拒
    #[tokio::test]
    async fn assess_denies_sensitive_file_inside_scope() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_write_test();
        let tool = WriteTool::new();
        let args = serde_json::json!({
            "path": dir.join(".env"),
            "content": "KEY=1"
        })
        .to_string();
        assert!(
            matches!(tool.assess(&args).await, Action::Deny(_)),
            ".env 应被直接拒绝"
        );
        cleanup(&dir);
    }

    /// 普通文件在范围内仍然直接放行,敏感判定不能误杀
    #[tokio::test]
    async fn assess_allows_plain_file_inside_scope() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_write_test();
        let tool = WriteTool::new();
        let args = serde_json::json!({
            "path": dir.join("plain.txt"),
            "content": "hi"
        })
        .to_string();
        assert!(
            matches!(tool.assess(&args).await, Action::Allow),
            "范围内普通文件应放行"
        );
        cleanup(&dir);
    }
}
