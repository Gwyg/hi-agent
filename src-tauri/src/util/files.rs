//! 项目数据文件的 IO 原语：不依赖任何业务领域（无 config / agent / llm 知识），
//! 只提供「项目数据根目录」与「安全整份写」两类最朴素、可复用的原语。
//!
//! 职责边界：本模块是**实现层**的 plumbing——各业务模块（配置 / 授权记忆 / 窗口几何）
//! 的「加载与写入」逻辑由各自模块持有（如 `config::AppConfig::load` 与 `config::persist`），本模块仅提供
//! 它们共用的底层原语（项目数据目录 + 原子整份写），
//! **不写任何领域相关的文件名、不做序列化**。
//!
//! 写入只有 [`atomic_write`] 一种形态：整份替换，一次动作就是一次完整快照，崩溃不会留下半截文件。
//! 只增不改的结构化数据不在这里手写追加 —— 它们进 `store` 的 SQLite 单库（事务 / 索引 / 迁移
//! 交给数据库）。曾为 JSONL 方案准备的 `append_line` / `read_lines` 已随之作废删除。
//!
//! 「不做序列化」指**不把业务结构体映射成文本**——那要知道字段含义，归各配置域的 `Deserialize`。
//! 对已解析语法树的编辑（建段、修坏段、保住注释）属格式层，见 `util::toml`；两者合起来才是
//! 「安全地改一个配置文件」，但谁都不知道配置文件里有哪些字段。

use std::io::Write;
use std::path::{Path, PathBuf};

/// 项目数据根目录：`~/.hi-agent/`。
///
/// config / logs / workspace / 未来对话历史均位于其下。
/// 集中在此定义，避免 `config`、`logging` 等各处硬编码 `~/.hi-agent` 造成重复与漂移。
pub fn app_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".hi-agent")
}

/// 确保目录存在（含父级）；已存在则无操作。
pub fn ensure_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)
}

/// 读文件为字符串；缺失或读取失败返回空串（调用方据此走默认逻辑）。
pub fn read_to_string_or_default(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// 原子写：临时文件 → `fsync` → `rename`，防中途崩溃留下半截文件损坏数据。
/// `path` 所在目录会被确保存在。
///
/// 三点保证：进程被杀不会留半截（rename 原子替换）；断电也不会（`sync_all` 才让数据真正落盘，
/// 只 rename 不刷盘时目录项可能指向尚未写入的数据）；目标已存在时权限不变——rename 换掉了
/// inode，不显式沿用原 mode 就会退回默认值（可执行位会这样丢）。目标不存在则跟随 umask。
/// 权限策略（比如「密钥文件必须是 0600」）不归这里，由调用方在自己那层决定。
pub fn atomic_write(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }
    // 同目录隐藏临时名 `.{文件名}.{pid}.tmp`。不用 `with_extension`——它替换扩展名
    // (`a.tar.gz` → `a.tar.tmp`)且落成普通文件名,可能撞上用户自己的同名文件;隐藏解决可见性,
    // pid 解决多实例互踩
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));

    let res = write_via_tmp(path, &tmp, contents);
    if res.is_err() {
        // 临时文件里就是完整内容(可能含密钥),失败时不能留在磁盘上
        std::fs::remove_file(&tmp).ok();
    }
    res
}

/// 写满临时文件再顶替目标；`?` 只退出到这里，调用方才有机会清掉失败留下的临时文件。
fn write_via_tmp(path: &Path, tmp: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::File::create(tmp)?;
    // 仅 Unix:Windows 的 `Permissions` 只表达只读位,把临时文件设成只读会让
    // 随后的 rename 覆盖失败
    #[cfg(unix)]
    if let Ok(meta) = std::fs::metadata(path) {
        file.set_permissions(meta.permissions())?;
    }
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file); // Windows 上 rename 前必须关闭句柄
    std::fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::atomic_write;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 目录里的条目名(排序):用来断言「只剩目标文件」——比起盯某个特定临时名,
    /// 这样不依赖临时文件的命名规则,规则变了测试也不用跟着改。
    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn replaces_content_and_leaves_no_tmp() {
        let dir = tmpdir("hi_agent_atomic_write");
        let file = dir.join("app.toml");

        atomic_write(&file, b"a = 1").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "a = 1");

        atomic_write(&file, b"a = 2").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "a = 2");
        assert_eq!(entries(&dir), vec!["app.toml"], "不应残留临时文件");

        fs::remove_dir_all(&dir).ok();
    }

    /// 临时名要保住完整文件名:`a.tar.gz` 不能变成 `a.tar.tmp` 这种半截名字
    #[test]
    fn keeps_whole_file_name_in_tmp() {
        let dir = tmpdir("hi_agent_atomic_write_name");
        let file = dir.join("a.tar.gz");

        atomic_write(&file, b"x").unwrap();
        assert_eq!(entries(&dir), vec!["a.tar.gz"]);

        fs::remove_dir_all(&dir).ok();
    }

    /// 覆盖时沿用原 mode:rename 换掉了 inode,不显式处理会把可执行位抹掉
    #[cfg(unix)]
    #[test]
    fn preserves_mode_on_overwrite() {
        let dir = tmpdir("hi_agent_atomic_write_mode");
        let file = dir.join("run.sh");

        atomic_write(&file, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();

        atomic_write(&file, b"#!/bin/sh\necho hi\n").unwrap();
        assert_eq!(mode_of(&file), 0o755, "覆盖应保住可执行位");

        fs::remove_dir_all(&dir).ok();
    }

    /// 目标是个目录时 rename 必然失败:此时临时文件要被清掉,不能把内容留在磁盘上
    #[test]
    fn cleans_tmp_when_rename_fails() {
        let dir = tmpdir("hi_agent_atomic_write_fail");
        let target = dir.join("app.toml");
        fs::create_dir_all(&target).unwrap();

        assert!(atomic_write(&target, b"x").is_err(), "写入目录应失败");
        // 目标目录本身还在,但不应多出临时文件
        assert_eq!(entries(&dir), vec!["app.toml"], "失败后不应残留临时文件");

        fs::remove_dir_all(&dir).ok();
    }

    /// 父目录不存在时自动创建:首次生成配置时 `~/.hi-agent/` 还不存在
    #[test]
    fn creates_parent_dir_on_write() {
        let dir = tmpdir("hi_agent_atomic_write_mkdir");
        let file = dir.join("nested/deep/app.toml");

        atomic_write(&file, b"a = 1").unwrap();

        assert_eq!(fs::read_to_string(&file).unwrap(), "a = 1");
        fs::remove_dir_all(&dir).ok();
    }
}
