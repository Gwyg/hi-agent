use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// 全局项目根(启动时 set,会话内不变;界面切项目时可 set 改)
static PROJECT_ROOT: RwLock<Option<PathBuf>> = RwLock::new(None);

/// 全局额外白名单(启动时 set,会话内不变)
/// 默认覆盖:临时文件、agent 配置、工具配置、缓存、数据
static EXTRA_ALLOWED: RwLock<Vec<PathBuf>> = RwLock::new(Vec::new());

/// 合并后的敏感后缀清单(内置 + 配置追加,启动时 set,会话内不变)
/// 内置基线始终在前,配置只能追加不能移除内置项(安全原则:收紧可,放宽不可)
static SENSITIVE_SUFFIXES: RwLock<Vec<String>> = RwLock::new(Vec::new());

/// 合并后的敏感路径前缀清单(内置 + 配置追加,启动时已展开 ~,会话内不变)
/// 内置基线始终在前,配置只能追加不能移除内置项
static SENSITIVE_PATHS: RwLock<Vec<String>> = RwLock::new(Vec::new());

/// 内置敏感后缀基线(ends_with 匹配,小写;启动时合并到 SENSITIVE_SUFFIXES)
const BUILTIN_SUFFIXES: &[&str] = &[
    ".env",          // 环境变量(API keys, DB URLs)
    ".envrc",        // direnv
    ".pem",          // 证书/私钥
    ".key",          // 私钥
    ".p12",          // 证书
    ".pfx",          // 证书
    ".npmrc",        // npm token
    ".pypirc",       // PyPI token
    ".netrc",        // HTTP 凭证
    ".bash_history", // shell 历史
    ".zsh_history",  // shell 历史
    "id_rsa",        // SSH 私钥(ends_with: id_rsa,不匹配 id_rsa.pub 因为公钥不敏感)
    "id_ed25519",    // SSH 私钥(同上)
    "secrets.yml",   // 通用密钥文件
    "secrets.yaml",
    "secrets.json",
];

/// 内置敏感路径前缀基线(starts_with 匹配,启动时展开 ~)
/// 只匹配家目录下的凭证目录,不影响项目内或其他位置的同名目录
/// 参考 Codex 的 OS 沙箱精确路径 denyRead
const BUILTIN_PATH_PREFIXES: &[&str] = &[
    "~/.ssh",           // SSH 密钥
    "~/.aws",           // AWS 凭证
    "~/.kube",          // K8s 配置
    "~/.gnupg",         // GPG 密钥
    "~/.config/gcloud", // GCP 凭证
];

/// 初始化/切换项目根
/// - CLI:main 启动时调一次(env 优先,current_dir 兜底)
/// - 界面:用户切项目时调
/// canonicalize 确保路径真实(解析软链/../)
pub fn set_project_root(root: PathBuf) -> anyhow::Result<()> {
    let canonical = root.canonicalize()?;
    let mut guard = PROJECT_ROOT
        .write()
        .map_err(|e| anyhow::anyhow!("project_root 锁中毒: {e}"))?;
    *guard = Some(canonical);
    Ok(())
}

/// 获取项目根(返 owned clone,供路径解析)
pub fn project_root() -> anyhow::Result<PathBuf> {
    PROJECT_ROOT
        .read()
        .map_err(|e| anyhow::anyhow!("project_root 锁中毒: {e}"))?
        .clone()
        .ok_or_else(|| anyhow::anyhow!("project_root 未初始化,启动时需 set_project_root"))
}

/// 设置额外白名单(启动时调)
/// 内部展开 ~(调用方可传 ~/aaa 未展开路径),canonicalize 解析软链;不存在保留原路径
pub fn set_extra_allowed(paths: Vec<PathBuf>) -> anyhow::Result<()> {
    let mut canonical_paths = Vec::with_capacity(paths.len());
    for p in paths {
        // 先展开 ~(防御性:调用方可能传 ~/aaa 未展开)
        let expanded = expand_tilde(&p.to_string_lossy());
        // 存在则 canonicalize(解析软链,匹配 real 路径);不存在保留展开后路径,不丢白名单条目
        let canonical = expanded.canonicalize().unwrap_or(expanded);
        canonical_paths.push(canonical);
    }
    let mut guard = EXTRA_ALLOWED
        .write()
        .map_err(|e| anyhow::anyhow!("extra_allowed 锁中毒: {e}"))?;
    *guard = canonical_paths;
    Ok(())
}

/// 默认额外白名单:只含临时目录
/// 其他路径(配置/缓存/数据目录)路径约定跨平台争议大(Apple vs XDG vs Windows),
/// 不内置,需写项目外配置等用 bash 显式操作
/// main 启动时调,传给 set_extra_allowed
pub fn default_extra_paths() -> Vec<PathBuf> {
    vec![std::env::temp_dir()]
}

/// 设置敏感后缀清单:内置 + 配置追加(启动时调)
/// 内置基线始终在前,配置只能追加不能移除内置项
/// 全部用 ends_with 匹配,归一化后存入 SENSITIVE_SUFFIXES
pub fn set_sensitive_suffixes(extra: &[String]) -> anyhow::Result<()> {
    let mut all: Vec<String> = BUILTIN_SUFFIXES
        .iter()
        .map(|s| normalize_for_match(s))
        .collect();
    all.extend(extra.iter().map(|s| normalize_for_match(s)));
    let mut guard = SENSITIVE_SUFFIXES
        .write()
        .map_err(|e| anyhow::anyhow!("sensitive_suffixes 锁中毒: {e}"))?;
    *guard = all;
    Ok(())
}

/// 设置敏感路径前缀清单:内置 + 配置追加(启动时调)
/// 启动时预展开 ~ 成绝对路径,运行时只 starts_with,无 replacen alloc
/// 内置基线始终在前,配置只能追加不能移除内置项
pub fn set_sensitive_paths(extra: &[String]) -> anyhow::Result<()> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("无法获取家目录,无法展开敏感路径前缀"))?;
    let home_str = normalize_for_match(&home.to_string_lossy());
    let mut all: Vec<String> = BUILTIN_PATH_PREFIXES
        .iter()
        .map(|p| p.replacen('~', &home_str, 1))
        .collect();
    all.extend(
        extra
            .iter()
            .map(|p| normalize_for_match(p).replacen('~', &home_str, 1)),
    );
    let mut guard = SENSITIVE_PATHS
        .write()
        .map_err(|e| anyhow::anyhow!("sensitive_paths 锁中毒: {e}"))?;
    *guard = all;
    Ok(())
}

/// 路径/模式归一化(用于敏感文件匹配)
/// - 统一 \ → /(跨平台分隔符)
/// - Windows/macOS:小写化(文件系统不区分大小写)
/// - Linux:保留原样(文件系统区分大小写,.ENV ≠ .env)
fn normalize_for_match(s: &str) -> String {
    let s = s.replace('\\', "/");
    #[cfg(any(windows, target_os = "macos"))]
    let s = s.to_lowercase();
    s
}

/// 展开 `~` 为家目录(跨平台,用 dirs::home_dir)
/// - "~/foo" → {home}/foo
/// - "~" → {home}
/// - "/etc" → /etc(不变,无 ~)
///
/// Path::is_absolute() 不认 ~,需显式展开,否则 ~/foo 被当相对路径拼到项目内
fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

/// 路径解析:展开 ~ → 相对路径拼项目根,绝对路径直通
/// - "~/.config/foo" → {home}/.config/foo
/// - "src/foo.rs" → {project_root}/src/foo.rs
/// - "/etc/passwd" → /etc/passwd
pub fn resolve_path(path: &str) -> anyhow::Result<PathBuf> {
    let path = expand_tilde(path);
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(project_root()?.join(path))
    }
}

/// 跨平台路径前缀比较
/// - Windows/macOS:文件系统默认不区分大小写,小写比较
/// - Linux:区分大小写,原生 starts_with
fn path_starts_with(path: &Path, prefix: &Path) -> bool {
    #[cfg(any(windows, target_os = "macos"))]
    {
        path.to_string_lossy()
            .to_lowercase()
            .starts_with(&prefix.to_string_lossy().to_lowercase())
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        path.starts_with(prefix)
    }
}

/// 校验路径在沙箱内(项目根 + 额外白名单),返回 canonicalize 后的真实路径。
/// 拒绝:越界、.git/ 子目录
///
/// 内部调 resolve_path(展开 ~、相对路径拼项目根),调用方传原始字符串即可
/// 分流:文件存在时 canonicalize 文件(跟文件软链);不存在时 canonicalize 父目录 + 拼文件名
pub fn ensure_within_sandbox(path_str: &str) -> anyhow::Result<PathBuf> {
    let path = resolve_path(path_str)?;
    let root = project_root()?;

    let real = if path.exists() {
        // 文件存在:canonicalize 跟软链(含文件本身是软链的情况)
        path.canonicalize()?
    } else {
        // 文件不存在:canonicalize 父目录(跟父目录软链/消解..)+ 拼文件名
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("路径无父目录: {}", path.display()))?;
        let real_parent = if parent.as_os_str().is_empty() {
            root.clone() // 无目录前缀(如 "foo.rs"),父目录默认项目根
        } else {
            parent.canonicalize().map_err(|_| anyhow::anyhow!(
                "无法写入 {}: 父目录 {} 不存在或路径越界(检查路径;若需新建目录用 bash mkdir -p)",
                path.display(),
                parent.display()
            ))?
        };
        let file_name = path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("路径无文件名: {}", path.display()))?;
        real_parent.join(file_name)
    };

    // 统一校验:沙箱内(check_allowed 内部已含 .git 保护)
    check_allowed(&real, &root)?;
    Ok(real)
}

/// 判断路径是否指向敏感文件(需警惕的文件,即使沙箱允许)
/// 用于 assess 预检:敏感文件读写需每次确认
///
/// 遍历两个合并后的清单(启动时已合并内置 + 配置追加,前缀已展开 ~):
///   1. 后缀(ends_with)
///   2. 路径前缀(starts_with)
/// 锁中毒降级为 false(fail-open,与原实现一致)
pub fn is_sensitive_path(path: &str) -> bool {
    // 归一化:统一分隔符 + Windows/macOS 小写化(Linux 保留原样,区分大小写)
    let p = normalize_for_match(path);
    // 1. 后缀(ends_with,锁中毒降级为 false)
    if SENSITIVE_SUFFIXES
        .read()
        .map(|suffixes| suffixes.iter().any(|s| p.ends_with(s)))
        .unwrap_or(false)
    {
        return true;
    }
    // 2. 路径前缀(starts_with,启动时已展开 ~,锁中毒降级为 false)
    SENSITIVE_PATHS
        .read()
        .map(|prefixes| prefixes.iter().any(|s| p.starts_with(s)))
        .unwrap_or(false)
}

/// 检查路径在项目根或额外白名单内
fn check_allowed(real: &Path, root: &Path) -> anyhow::Result<()> {
    // 1. 项目根白名单
    if path_starts_with(real, root) {
        check_not_git(real, root)?;
        return Ok(());
    }

    // 2. 额外白名单(启动时 set 的全局列表)
    let extra = EXTRA_ALLOWED
        .read()
        .map_err(|e| anyhow::anyhow!("extra_allowed 锁中毒: {e}"))?;
    for allowed in extra.iter() {
        if path_starts_with(real, allowed) {
            check_not_git(real, allowed)?;
            return Ok(());
        }
    }

    Err(anyhow::anyhow!(
        "路径越界: {} 不在项目根或额外允许列表内",
        real.display()
    ))
}

/// 拒绝 .git/ 子目录(版本库元数据保护)
fn check_not_git(real: &Path, root: &Path) -> anyhow::Result<()> {
    if let Ok(rel) = real.strip_prefix(root) {
        if rel.components().any(|c| c.as_os_str() == ".git") {
            return Err(anyhow::anyhow!(
                "拒绝写 .git/ 目录(版本库元数据保护): {}",
                real.display()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;

    // 全局 PROJECT_ROOT/EXTRA_ALLOWED 跨测试共享,用 Mutex 序列化
    static ROOT_LOCK: Mutex<()> = Mutex::new(());

    fn setup_sandbox_test() -> PathBuf {
        let dir = PathBuf::from("/tmp/hi_agent_sandbox_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(dir.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(dir.join("existing.txt"), "hello").unwrap();
        set_project_root(dir.clone()).unwrap();
        set_extra_allowed(default_extra_paths()).unwrap();
        dir
    }

    #[test]
    fn allows_path_in_root() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_sandbox_test();
        let path = dir.join("existing.txt");
        let result = ensure_within_sandbox(path.to_str().unwrap());
        assert!(result.is_ok(), "项目内文件应允许: {:?}", result);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn allows_new_file_in_root() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_sandbox_test();
        let path = dir.join("new_file.txt");
        let result = ensure_within_sandbox(path.to_str().unwrap());
        assert!(result.is_ok(), "项目内新文件应允许: {:?}", result);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_outside_root() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_sandbox_test();
        // /etc 不在项目根,也不在白名单(只 /tmp)
        let path = PathBuf::from("/etc/passwd");
        let result = ensure_within_sandbox(path.to_str().unwrap());
        assert!(result.is_err(), "项目外路径应拒绝");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("越界"), "错误应提示越界: {msg}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_dot_git() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_sandbox_test();
        let path = dir.join(".git").join("HEAD");
        let result = ensure_within_sandbox(path.to_str().unwrap());
        assert!(result.is_err(), ".git 应拒绝");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains(".git"), "错误应提示 .git: {msg}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_parent_traversal() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_sandbox_test();
        let path = dir.join("..").join("etc").join("passwd");
        let result = ensure_within_sandbox(path.to_str().unwrap());
        assert!(result.is_err(), "../ 逃逸应拒绝");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_nonexistent_parent() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_sandbox_test();
        let path = dir.join("nonexistent_subdir").join("file.txt");
        let result = ensure_within_sandbox(path.to_str().unwrap());
        assert!(result.is_err(), "父目录不存在应拒绝");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("父目录") || msg.contains("越界"),
            "错误应提示父目录或越界: {msg}"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_relative_path() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_sandbox_test();
        let resolved = resolve_path("src/foo.rs").unwrap();
        let expected = dir.canonicalize().unwrap().join("src").join("foo.rs");
        assert_eq!(resolved, expected);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_absolute_path() {
        let resolved = resolve_path("/etc/passwd").unwrap();
        assert_eq!(resolved, PathBuf::from("/etc/passwd"));
    }

    #[test]
    fn allows_default_tmp_path() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_sandbox_test();
        let path = std::env::temp_dir().join("hi_agent_test_file.txt");
        let result = ensure_within_sandbox(path.to_str().unwrap());
        assert!(result.is_ok(), "/tmp 应在默认白名单: {:?}", result);
        fs::remove_file(&path).ok();
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn path_starts_with_case_aware() {
        #[cfg(target_os = "macos")]
        {
            let path = Path::new("/Users/Foo/bar");
            let prefix = Path::new("/users/foo");
            assert!(path_starts_with(path, prefix), "macOS 应不区分大小写");
        }
        #[cfg(windows)]
        {
            let path = Path::new("C:\\Projects\\Foo");
            let prefix = Path::new("c:\\projects\\foo");
            assert!(path_starts_with(path, prefix), "Windows 应不区分大小写");
        }
        #[cfg(target_os = "linux")]
        {
            let path = Path::new("/home/Foo");
            let prefix = Path::new("/home/foo");
            assert!(!path_starts_with(path, prefix), "Linux 应区分大小写");
        }
    }

    #[test]
    fn expand_tilde_home() {
        let expanded = expand_tilde("~");
        let home = dirs::home_dir().unwrap();
        assert_eq!(expanded, home);
    }

    #[test]
    fn expand_tilde_subpath() {
        let expanded = expand_tilde("~/foo/bar");
        let home = dirs::home_dir().unwrap();
        assert_eq!(expanded, home.join("foo").join("bar"));
    }

    #[test]
    fn expand_tilde_no_tilde() {
        let expanded = expand_tilde("/etc/passwd");
        assert_eq!(expanded, PathBuf::from("/etc/passwd"));
    }

    #[test]
    fn resolve_tilde_path() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_sandbox_test();
        // ~/foo 应展开成家目录下的 foo,而非拼到项目内
        let resolved = resolve_path("~/foo").unwrap();
        let home = dirs::home_dir().unwrap();
        assert_eq!(resolved, home.join("foo"), "~ 应展开到家目录");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn set_extra_allowed_keeps_nonexistent() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_sandbox_test();
        // 含不存在的路径,不应被丢弃
        let nonexistent = dir.join("nonexistent_allow_dir");
        set_extra_allowed(vec![nonexistent.clone()]).unwrap();
        // 读取白名单验证条目保留
        let extra = EXTRA_ALLOWED.read().unwrap();
        assert!(
            extra.iter().any(|p| *p == nonexistent),
            "不存在路径应保留在白名单"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn set_extra_allowed_expands_tilde() {
        let _guard = ROOT_LOCK.lock().unwrap();
        let dir = setup_sandbox_test();
        // 传未展开的 ~/foo,set_extra_allowed 应展开成 {home}/foo
        set_extra_allowed(vec![PathBuf::from("~/foo")]).unwrap();
        let home = dirs::home_dir().unwrap();
        let expected = home.join("foo");
        let extra = EXTRA_ALLOWED.read().unwrap();
        let found = extra.iter().any(|p| *p == expected);
        assert!(found, "~/foo 应展开成 {:?}/foo,实际白名单未找到", home);
        drop(extra);
        fs::remove_dir_all(&dir).ok();
    }
}
