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

/// 单文件上限:先 stat 再读,不把整个文件吞进内存才发现太大
const MAX_FILE_SIZE: usize = 10 * 1024 * 1024;
/// 同步 IO 超时(到点只是不再等,线程本身停不下来,见 `execute`)
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

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "edit",
            format!(
                "对文件做精确局部编辑(替换文本片段)。有副作用,中危。只改已有文件,不新建。\n\
                 适用:改局部代码 / 配置 / 文档;重构;修 bug;删掉一段(要删的文本作 old_string,new_string 传空)。\n\
                 不适用:新建文件用 write;整份重写用 write(文件不大时更省事);看内容用 read——不 read 就写 old_string,\
                 多半匹配不上;大面积变动(几十处)也用 write。\n\
                 old_string 必须与文件内容逐字节相同(空格、换行、大小写都算;文件是 CRLF 就要带 \\r),否则报错。\n\
                 默认要求全文只匹配 1 处:多处命中说明对内容的认知有偏差,此时不猜、直接报错,先 read 确认;\
                 要批量改就传 expected_count,含义是「必须恰好这么多处」,不符同样整次放弃。\n\
                 old_string 不能为空,也不能与 new_string 相同。任何一步校验不过都不会改动文件(原子写回,失败不留半截)。\n\
                 限制:目标必须存在且是普通文件(目录 / 管道 / 设备拒);只支持 UTF-8 文本;单文件上限 {mb}MB,\
                 更大改用 bash;项目根与额外允许目录内直接改,越界 / .git/ 每次要用户确认;\
                 密钥 / 凭据类文件(.env、.pem、id_rsa、~/.ssh/ 等)不由本工具修改,\
                 用户确实需要时改用 bash 命令(会向用户确认)。\n\
                 软链改到真实目标,以返回结果里的路径为准。",
                mb = MAX_FILE_SIZE / (1024 * 1024)
            ),
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "文件路径:相对项目根、绝对路径、或 ~/开头(展开家目录)"
                    },
                    "old_string": {
                        "type": "string",
                        "description": "要替换的精确文本,须与文件内容逐字节相同(含空格、换行、大小写);不能为空"
                    },
                    "new_string": {
                        "type": "string",
                        "description": "替换后的文本;传空串表示删除 old_string"
                    },
                    "expected_count": {
                        "type": "integer",
                        "description": "预期匹配数量,必须与实际数量完全相等,不符则整次放弃。不传=默认1(要求唯一匹配)"
                    }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        )
    }

    async fn assess(&self, args: &str) -> Action {
        // 判定只用到 path,但没必要为「取一个字段」再造个结构体:参数解析不出来就说明
        // 这次调用不成立,直接拒掉——和 execute 报的是同一件事,不会混淆
        let path = match serde_json::from_str::<Args>(args) {
            Ok(a) => a.path,
            Err(e) => return Action::Deny(format!("edit 参数解析失败: {e}")),
        };
        // 敏感路径 / 后缀直接拒,不给确认机会:这类文件装的是密钥凭据,改坏即不可恢复。
        // 排在白名单之前——项目根内的 `.env` 同样命中白名单,不能因此放行。
        // 文案明确「本工具不允许」并给出 bash 这条出路:否则模型会把工具的局部限制
        // 复述成「密钥文件一律不能由助手改」,平白拦住用户本可完成的修改。
        if is_sensitive_path(&path) || has_sensitive_suffix(&path) {
            return Action::Deny(format!(
                "edit 工具不允许改敏感文件: {path} —— 密钥 / 凭据类文件不由本工具改写；\
                 用户确实需要时，可通过 bash 命令完成（会向用户确认）"
            ));
        }
        // 白名单内 = 用户已授权,直接改;白名单外每次必问,不记忆
        if is_allowed_path(&path) {
            Action::Allow
        } else {
            Action::AskAlways
        }
    }

    async fn execute(&self, args: &str) -> anyhow::Result<String> {
        let args: Args =
            serde_json::from_str(args).map_err(|e| anyhow::anyhow!("edit 参数解析失败: {e}"))?;

        // 防无意义编辑(模型误用)
        if args.old_string == args.new_string {
            return Err(anyhow::anyhow!("old_string 与 new_string 相同,无意义"));
        }

        // 同步 IO 交给工作线程,超时兜底。到点只是丢掉 JoinHandle:一旦进入阻塞调用就没法
        // 中止,所以「停」不靠这里的 timeout,靠 edit_sync 保证每个阻塞点都有界(见那里的注释)
        let timeout_path = args.path.clone();
        tokio::time::timeout(
            EDIT_TIMEOUT,
            tokio::task::spawn_blocking(move || {
                edit_sync(
                    &args.path,
                    &args.old_string,
                    &args.new_string,
                    args.expected_count,
                )
            }),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "编辑超过 {:?} 未返回: {timeout_path} —— 阻塞 IO 无法中止,这次编辑可能仍在进行;\
                 重试前先 read 确认当前内容,不要直接重试",
                EDIT_TIMEOUT
            )
        })?
        .map_err(|e| anyhow::anyhow!("编辑任务失败: {e}"))?
    }
}

/// 同步编辑核心:定位真实文件 → 判定目标 → 读 → 计数校验 + 替换 → 原子写回。
/// 授权不在这里(那是 assess 的事),执行侧只回答「改哪个文件、改不改成」。
///
/// 跑在 `spawn_blocking` 里,超时后没法中止,所以每个阻塞点都必须有界:
/// 先 `metadata` 拿类型和大小,非普通文件一律不进 `read_to_string`——FIFO 会阻塞到有写者,
/// `/dev/zero` 这类设备永远读不完,两者都能把 blocking 线程永久钉住(线程池耗尽后全盘卡死)。
fn edit_sync(
    path_str: &str,
    old: &str,
    new: &str,
    expected_count: Option<usize>,
) -> anyhow::Result<String> {
    // 空串在 `matches` / `replacen` 里匹配每个字符边界(长度 n 的串有 n+1 处),
    // 一旦放过去就是「全文随机插入」,不是编辑该有的语义
    if old.is_empty() {
        return Err(anyhow::anyhow!(
            "old_string 不能为空(删内容请传要删的文本,整份重写用 write)"
        ));
    }

    // 取真实路径(跟软链、消 `..`):判定看的是它,读写也只能用它,两边不能分叉
    let root = workspace_root()?;
    let resolved = resolve_lexical(path_str, Some(&root))?;
    let path = PathBuf::from(
        canonical_path(&resolved)
            .map_err(|e| anyhow::anyhow!("无法解析编辑路径(工作目录可能已失效): {e}"))?,
    );

    // 一次 stat 定类型 / 存在 / 大小。`exists()` 会把权限不足、路径过长一并吞成 false,
    // 底层原因被盖掉,还会把模型引去「用 write 新建」
    let meta = match std::fs::metadata(&path) {
        Ok(m) if !m.is_file() => {
            return Err(anyhow::anyhow!(
                "不是普通文件(目录 / 管道 / 设备等): {}",
                path.display()
            ))
        }
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return Err(anyhow::anyhow!(
                "文件不存在: {}(用 write 新建)",
                path.display()
            ))
        }
        Err(e) => return Err(anyhow::anyhow!("无法读取文件状态 {}: {e}", path.display())),
    };
    if meta.len() > MAX_FILE_SIZE as u64 {
        // 别建议 write:它同样是 10MB 上限,而且要求模型先把全文吐出来——照做只会再撞一次
        return Err(anyhow::anyhow!(
            "文件过大: {} 字节,上限 {} 字节 —— 改用 bash 就地处理(sed / awk 等),\
             edit 不适合这个量级",
            meta.len(),
            MAX_FILE_SIZE
        ));
    }

    // 读失败的两种成因完全不同,不能混在一条文案里:把「没权限」说成「不是 UTF-8」
    // 会把模型带去转码,而真正要解决的是权限
    let content = std::fs::read_to_string(&path).map_err(|e| {
        if e.kind() == ErrorKind::InvalidData {
            anyhow::anyhow!(
                "读取失败 {}: 不是 UTF-8 文本(二进制 / GBK 等),edit 处理不了,改用 bash",
                path.display()
            )
        } else {
            anyhow::anyhow!("读取失败 {}: {e}", path.display())
        }
    })?;

    // 实际数量与预期不等就不动手:此时替换哪一处都是猜,让模型先 read 再回来
    let actual = content.matches(old).count();
    if actual == 0 {
        return Err(anyhow::anyhow!(
            "找不到 old_string(逐字节比对:空格、换行、大小写;文件用 CRLF 就得带 \\r)"
        ));
    }
    let expected = expected_count.unwrap_or(1);
    if actual != expected {
        return Err(anyhow::anyhow!(
            "预期 {expected} 处匹配,实际 {actual} 处,拒绝(防盲区误替换;先 read 确认实际内容,\
             或提供更精确的 old_string)"
        ));
    }

    // 原子写回:临时文件 + fsync + rename,失败不留半截;覆盖时沿用原文件权限
    let new_content = content.replacen(old, new, actual);
    atomic_write(&path, new_content.as_bytes())
        .map_err(|e| anyhow::anyhow!("写入 {} 失败: {e}", path.display()))?;

    Ok(format!(
        "已编辑 {}({actual} 处替换,{} → {} 字节)",
        path.display(),
        content.len(),
        new_content.len()
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

    use crate::ai::llm::tools::support::path_scope::TEST_ROOT_LOCK;
    use crate::config::path_scope::{init, set_root, PathScopeConfig};
    use crate::util::{canonical_path, strip_verbatim};

    /// 每个用例独占一个目录:文件互不干扰,只让 root 这一处共享(由 TEST_ROOT_LOCK 排队)
    fn setup(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hi_agent_edit_{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.rs"), "let x = 1;\nlet y = 2;\n").unwrap();
        // 只改内存,不落盘——测试不能动用户的 app.toml
        init(&PathScopeConfig::default());
        let root = canonical_path(&dir.display().to_string()).expect("解析测试目录");
        set_root(&strip_verbatim(&root)).expect("注入工作目录根");
        dir
    }

    fn cleanup(dir: &Path) {
        fs::remove_dir_all(dir).ok();
    }

    fn args_of(dir: &Path, old: &str, new: &str, expected: Option<usize>) -> String {
        serde_json::json!({
            "path": dir.join("a.rs"),
            "old_string": old,
            "new_string": new,
            "expected_count": expected,
        })
        .to_string()
    }

    async fn run(
        dir: &Path,
        old: &str,
        new: &str,
        expected: Option<usize>,
    ) -> anyhow::Result<String> {
        EditTool::new()
            .execute(&args_of(dir, old, new, expected))
            .await
    }

    #[tokio::test]
    async fn replaces_unique_match() {
        let _guard = TEST_ROOT_LOCK.lock().unwrap();
        let dir = setup("unique");
        let res = run(&dir, "let x = 1;", "let x = 42;", None).await.unwrap();
        assert!(res.contains("1 处替换"), "{res}");
        assert_eq!(
            fs::read_to_string(dir.join("a.rs")).unwrap(),
            "let x = 42;\nlet y = 2;\n"
        );
        cleanup(&dir);
    }

    /// 默认要求唯一匹配:撞上两处就拒绝,不猜改哪一个
    #[tokio::test]
    async fn rejects_when_match_count_differs() {
        let _guard = TEST_ROOT_LOCK.lock().unwrap();
        let dir = setup("count");
        let msg = run(&dir, "let ", "const ", None)
            .await
            .unwrap_err()
            .to_string();
        assert!(msg.contains("实际 2 处"), "{msg}");
        assert_eq!(
            fs::read_to_string(dir.join("a.rs")).unwrap(),
            "let x = 1;\nlet y = 2;\n",
            "拒绝时不能改动文件"
        );
        cleanup(&dir);
    }

    #[tokio::test]
    async fn batch_replaces_with_expected_count() {
        let _guard = TEST_ROOT_LOCK.lock().unwrap();
        let dir = setup("batch");
        let res = run(&dir, "let ", "const ", Some(2)).await.unwrap();
        assert!(res.contains("2 处替换"), "{res}");
        assert_eq!(
            fs::read_to_string(dir.join("a.rs")).unwrap(),
            "const x = 1;\nconst y = 2;\n"
        );
        cleanup(&dir);
    }

    /// 空串在每个字符边界都能匹配,放过去等于全文乱插
    #[tokio::test]
    async fn rejects_empty_old_string() {
        let _guard = TEST_ROOT_LOCK.lock().unwrap();
        let dir = setup("empty");
        let msg = run(&dir, "", "x", Some(2)).await.unwrap_err().to_string();
        assert!(msg.contains("不能为空"), "{msg}");
        cleanup(&dir);
    }

    /// 内容不变却走一遍 rename:改了 mtime、换了 inode,对版本控制就是「文件被动过」
    #[tokio::test]
    async fn rejects_identical_old_and_new() {
        let _guard = TEST_ROOT_LOCK.lock().unwrap();
        let dir = setup("same");
        let msg = run(&dir, "let x = 1;", "let x = 1;", None)
            .await
            .unwrap_err()
            .to_string();
        assert!(msg.contains("相同"), "{msg}");
        cleanup(&dir);
    }

    #[tokio::test]
    async fn reports_missing_file_for_write() {
        let _guard = TEST_ROOT_LOCK.lock().unwrap();
        let dir = setup("missing");
        let args = serde_json::json!({
            "path": dir.join("nope.rs"),
            "old_string": "a",
            "new_string": "b"
        })
        .to_string();
        let msg = EditTool::new()
            .execute(&args)
            .await
            .unwrap_err()
            .to_string();
        assert!(msg.contains("用 write 新建"), "{msg}");
        cleanup(&dir);
    }

    #[tokio::test]
    async fn rejects_directory() {
        let _guard = TEST_ROOT_LOCK.lock().unwrap();
        let dir = setup("dir");
        let args = serde_json::json!({
            "path": dir,
            "old_string": "a",
            "new_string": "b"
        })
        .to_string();
        let msg = EditTool::new()
            .execute(&args)
            .await
            .unwrap_err()
            .to_string();
        assert!(msg.contains("不是普通文件"), "{msg}");
        cleanup(&dir);
    }

    /// 设备 / 管道不是文本:`open` 会阻塞或无限读,必须在 stat 阶段就挡掉
    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_socket_target() {
        let _guard = TEST_ROOT_LOCK.lock().unwrap();
        let dir = setup("sock");
        let sock = dir.join("s.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let args = serde_json::json!({
            "path": sock,
            "old_string": "a",
            "new_string": "b"
        })
        .to_string();
        let msg = EditTool::new()
            .execute(&args)
            .await
            .unwrap_err()
            .to_string();
        assert!(msg.contains("不是普通文件"), "{msg}");
        cleanup(&dir);
    }

    /// 上限在读之前判:不把超大文件整个吞进内存再说它太大
    #[tokio::test]
    async fn rejects_oversized_before_reading() {
        let _guard = TEST_ROOT_LOCK.lock().unwrap();
        let dir = setup("big");
        fs::write(dir.join("a.rs"), vec![b'a'; MAX_FILE_SIZE + 1]).unwrap();
        let msg = run(&dir, "a", "b", None).await.unwrap_err().to_string();
        assert!(msg.contains("文件过大"), "{msg}");
        cleanup(&dir);
    }

    #[tokio::test]
    async fn rejects_non_utf8_text() {
        let _guard = TEST_ROOT_LOCK.lock().unwrap();
        let dir = setup("binary");
        fs::write(dir.join("a.rs"), [0xff, 0xfe, 0x00]).unwrap();
        let msg = run(&dir, "a", "b", None).await.unwrap_err().to_string();
        assert!(msg.contains("UTF-8"), "{msg}");
        cleanup(&dir);
    }

    /// 软链改到真实目标:判定看它、读写也只能用它,并把真实落点回报给模型
    #[cfg(unix)]
    #[tokio::test]
    async fn follows_symlink_to_real_target() {
        let _guard = TEST_ROOT_LOCK.lock().unwrap();
        let dir = setup("link");
        let link = dir.join("link.rs");
        std::os::unix::fs::symlink(dir.join("a.rs"), &link).unwrap();
        let args = serde_json::json!({
            "path": link,
            "old_string": "let x = 1;",
            "new_string": "let x = 9;"
        })
        .to_string();
        let res = EditTool::new().execute(&args).await.unwrap();
        assert!(res.contains("a.rs"), "应回报真实落点: {res}");
        assert_eq!(
            fs::read_to_string(dir.join("a.rs")).unwrap(),
            "let x = 9;\nlet y = 2;\n"
        );
        cleanup(&dir);
    }

    /// 鉴权吃的是一份完整合法参数(真实调用就是这样),路径才是变量
    async fn assess(path: &Path) -> Action {
        let args = serde_json::json!({
            "path": path,
            "old_string": "a",
            "new_string": "b"
        })
        .to_string();
        EditTool::new().assess(&args).await
    }

    #[tokio::test]
    async fn assess_allows_inside_scope() {
        let _guard = TEST_ROOT_LOCK.lock().unwrap();
        let dir = setup("assess_ok");
        let action = assess(&dir.join("a.rs")).await;
        assert!(matches!(action, Action::Allow), "范围内应放行: {action:?}");
        cleanup(&dir);
    }

    #[tokio::test]
    async fn assess_asks_outside_scope() {
        let _guard = TEST_ROOT_LOCK.lock().unwrap();
        let dir = setup("assess_out");
        let action = assess(Path::new("/etc/passwd")).await;
        assert!(
            matches!(action, Action::AskAlways),
            "越界应每次确认: {action:?}"
        );
        cleanup(&dir);
    }

    /// `.env` 在项目根内(命中白名单),但属于凭据类文件,不给确认机会
    #[tokio::test]
    async fn assess_denies_sensitive_file_inside_scope() {
        let _guard = TEST_ROOT_LOCK.lock().unwrap();
        let dir = setup("assess_secret");
        let action = assess(&dir.join(".env")).await;
        assert!(matches!(action, Action::Deny(_)), "应直接拒绝: {action:?}");
        cleanup(&dir);
    }
}
