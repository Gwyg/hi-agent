//! 工具侧路径判定。三个判定,各回答一个是非题,只回 `true` / `false`:
//!
//! | 方法 | 回答 | 命中含义 |
//! |---|---|---|
//! | `is_allowed_path` | 在允许范围内吗 | `true` = 在白名单内(工作目录根 / 额外允许目录),且不碰 `.git` |
//! | `is_sensitive_path` | 落在敏感目录下吗 | `true` = 命中前缀清单(`~/.ssh` 等) |
//! | `has_sensitive_suffix` | 是危险后缀吗 | `true` = 命中后缀清单(`.env` 等) |
//!
//! 三者共用同一份输入处理(`resolve_key`):绝对化 + 折 `..` → 跟软链取真实位置 → 归一化。
//! 输入只有这一份,算不出比对键就一律倒向更严格的一侧:白名单判 `false`,敏感判 `true`。
//!
//! 这里不解析路径(那是 `util::path`)、不打开文件、不拼错误消息。配置值全在
//! `config::path_scope`(只存值、不做判定)。

use crate::config::path_scope::{
    extra_allowed, sensitive_paths, sensitive_suffixes, workspace_path,
};
use crate::util::{canonical_path, normalize_path, resolve_lexical};

/// 工作目录根(绝对路径字符串,已去 Windows `\\?\` 前缀——与清单条目同形)。
///
/// 值由启动流程(`services::workspace::init`)算好后注入;`None` 表示工作区还没确定,
/// 此时任何路径判定都 fail-closed。
pub fn workspace_root() -> anyhow::Result<String> {
    workspace_path().ok_or_else(|| anyhow::anyhow!("工作目录未初始化(启动时需先确定工作区)"))
}

/// 测试用:工作目录根是进程级全局,各工具测试并行跑会互相覆盖,统一在这把锁上排队。
/// 只是测试设施,不参与任何生产路径(整个条目在非 test 构建里不存在)。
#[cfg(test)]
pub(crate) static TEST_ROOT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 在允许范围内吗:真实位置落在工作目录根或额外允许目录内,且路上没有 `.git` 段。
///
/// 判定必须 fail-closed:解析不出真实位置就判 `false`——此时「在不在范围内」无从确认,
/// 放行等于按字符串猜。
pub fn is_allowed_path(raw: &str) -> bool {
    let Ok(root) = workspace_root() else {
        return false;
    };
    let Some(key) = resolve_key(raw, Some(&root)) else {
        return false;
    };
    within_allowed(&key, &normalize_path(&root))
}

/// 落在敏感目录前缀下吗(`~/.ssh`、`~/.aws`、`app.toml` 等)。
pub fn is_sensitive_path(raw: &str) -> bool {
    let Some(key) = resolve_key(raw, workspace_path().as_deref()) else {
        return true;
    };
    sensitive_paths().iter().any(|p| prefix_match(&key, p))
}

/// 是危险后缀吗(`.env`、`.pem`、`id_rsa` 等)。
pub fn has_sensitive_suffix(raw: &str) -> bool {
    let Some(key) = resolve_key(raw, workspace_path().as_deref()) else {
        return true;
    };
    sensitive_suffixes()
        .iter()
        .any(|s| key.ends_with(s.as_str()))
}

/// 比对键落在允许前缀内,且不含 `.git` 段。两个入参都是比对键,这里只比字符串。
///
/// 允许前缀 = 工作区根 + 额外允许目录。工作区根不在清单里(它可变、单独存在),
/// 所以由调用方算好传进来。
fn within_allowed(key: &str, root_key: &str) -> bool {
    let base = if prefix_match(key, root_key) {
        root_key
    } else if let Some(hit) = extra_allowed().iter().find(|a| prefix_match(key, a)) {
        hit.as_str()
    } else {
        return false;
    };

    // `.git` 只按整个路径段拒:`my.git.dir`、`.gitignore` 都不算。
    // 归一化串固定 `/` 分隔,可直接按段切;`base` 已由 `prefix_match` 保证是前缀。
    !key[base.len()..].split('/').any(|seg| seg == ".git")
}

/// 待查路径 → 比对键。三步:绝对化 + 折 `..`(需 `root` 作基准)→ 跟软链取真实位置 →
/// 归一化。三个判定共用,算不出就返回 `None`,由各自按「更严格」的一侧收场。
///
/// 跟软链是必需的:软链能让字符串上的路径与实际落点分家。`../../Windows/x` 拼完仍以工作区
/// 开头,`link/x`(`link` 指向别处)同理,只有解开软链才看得见真实位置。
///
/// 目标整条路径都不存在也照判:祖先链上最近存在的那个决定真身,尚未创建的段原样拼回。
fn resolve_key(raw: &str, root: Option<&str>) -> Option<String> {
    let resolved = resolve_lexical(raw, root).ok()?;
    let real = canonical_path(&resolved).ok()?;
    Some(normalize_path(&real))
}

/// 前缀匹配,要求落在目录边界(`/work` 命中 `/work/a`,不命中 `/work2`)。
///
/// 输入须已是归一化比对键(`\`→`/`、Win/mac 小写)。不用 `Path::starts_with`:它在
/// Windows/macOS 上区分大小写;更不能裸比字符串——`C:\Temp` 会命中 `C:\Temp2`。
///
/// 三个判断(根、额外白名单、敏感前缀)全走它,规则只有这一份。
fn prefix_match(path: &str, prefix: &str) -> bool {
    match path.strip_prefix(prefix) {
        Some(rest) => rest.is_empty() || rest.starts_with('/'),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{prefix_match, resolve_key, within_allowed};
    use crate::util::normalize_path;

    /// 统一输入处理:目标不存在也能给出真实落点;没有基准的相对路径则明确解析不出来
    #[test]
    fn resolve_key_handles_missing_and_unresolvable() {
        let tmp = std::env::temp_dir();
        let root = tmp.display().to_string();

        // 三级目录全不存在:最近存在的祖先决定真身,缺失段原样拼回
        let key = resolve_key("__hi_a__/__hi_b__/f.txt", Some(&root)).expect("祖先可用");
        assert!(key.ends_with("__hi_a__/__hi_b__/f.txt"), "{key}");

        // 相对路径 + 无基准:算不出绝对位置,交给调用方定策略(白名单 fail-closed)
        assert!(resolve_key("a/b.txt", None).is_none());

        // 绝对路径 + 无基准:照常解析
        assert!(resolve_key(&root, None).is_some());
    }

    /// `.git` 只在整个路径段上拒绝,`my.git.dir` / `.gitignore` 都不算
    #[test]
    fn rejects_git_directory_only_as_segment() {
        assert!(!within_allowed("/proj/.git/config", "/proj"));
        assert!(!within_allowed("/proj/sub/.git/HEAD", "/proj"));
        assert!(within_allowed("/proj/.gitignore", "/proj"));
        assert!(within_allowed("/proj/my.git.dir/x", "/proj"));
    }

    /// 前缀必须落在目录边界:`.ssh` 不该命中同级的 `.ssh2`,工作区 `/work` 不该放行 `/work2`
    #[test]
    fn prefix_requires_path_boundary() {
        assert!(prefix_match("/home/u/.ssh/id_rsa", "/home/u/.ssh"));
        assert!(prefix_match("/home/u/.ssh", "/home/u/.ssh"));
        assert!(!prefix_match("/home/u/.ssh2/key", "/home/u/.ssh"));
        assert!(!prefix_match("/home/u/other", "/home/u/.ssh"));
        // 越界路径不能因为字符串前缀相同而放行
        assert!(!within_allowed("/work2/a", "/work"));
        // 条目带尾分隔符也不能让边界判断失效(`temp_dir()` 在 Windows 上就带尾反斜杠)
        assert!(prefix_match(
            &normalize_path("/tmp/x/hi.txt"),
            &normalize_path("/tmp/x/")
        ));
    }

    /// 归一化后大小写不再发散:Win/mac 视为同一目录,Linux 区分
    #[test]
    fn normalized_prefix_ignores_case_on_win_mac() {
        let key = normalize_path("/WORK/a");
        let prefix = normalize_path("/work");
        assert_eq!(
            prefix_match(&key, &prefix),
            cfg!(any(windows, target_os = "macos"))
        );
    }
}
