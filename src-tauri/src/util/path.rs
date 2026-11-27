//! 路径原语:展开 `~`、归一化成比对键、解析绝对路径(词法 / 真实)、去 Windows `\\?\` 前缀。
//!
//! 职责是**路径的文本表示 → 路径的文本表示**:进来字符串,出去字符串,这个模块不打开任何文件。
//! OS 的句柄形态(`Path` / `PathBuf`)只在函数内部出现,用来借用 std 的 `Path` API 做折叠与
//! 解析,出口一律转回 `String`——什么时候把字符串变成真正的路径去读写,是调用方执行阶段的事。
//!
//! 两步走:`resolve_lexical` 定绝对位置(要基准、纯词法),`canonical_path` 换真身(跟软链、
//! 只认绝对路径)。除后者要读一次元数据外都不碰磁盘,基准一律由调用方传入。
//!
//! 返回 `Result` 的只有三处**兑现不了承诺**的:`expand_tilde`(无家目录)、`resolve_lexical`
//! (无基准)、`canonical_path`(无落点)。纯字符映射的两个函数对任何输入都有唯一输出,承诺恒
//! 成立,加 `Result` 只会让每个调用点多写一个永不触发的分支。

use std::ffi::OsStr;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

/// 路径解析失败:缺的是信息(家目录 / 基准),不是 IO 出了错。IO 失败仍走 `io::Error`。
#[derive(Debug)]
pub enum PathError {
    /// 含 `~` 但环境给不出家目录
    NoHomeDir(String),
    /// 相对路径但没有基准
    NoBase(String),
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoHomeDir(raw) => write!(f, "无法展开 `~`: 家目录不可用 ({raw})"),
            Self::NoBase(raw) => write!(f, "相对路径缺少工作目录基准 ({raw})"),
        }
    }
}

impl std::error::Error for PathError {}

/// 展开 `~` 为家目录。只认单独一个 `~` 组件(`~` / `~/x` / `~\x`),`~user/x` 原样返回:
/// 那要查系统用户库。
///
/// 按**组件**判断而非字符串前缀:`~//x` 与 `~/x` 等价(连续分隔符在组件层面不存在)。若用
/// `strip_prefix("~/")` 只吃掉一层斜杠,剩下的 `/x` 会被 `join` 当成新根,家目录整个丢掉。
///
/// `Path::is_absolute()` 不认 `~`,不展开就会被当成相对路径拼进工作区。**含 `~` 却拿不到
/// 家目录时返回 `Err`**——原样返回的话,调用方无法把它和「本来就不含 `~` 的路径」区分开。
pub fn expand_tilde(raw: &str) -> Result<String, PathError> {
    let path = Path::new(raw);
    let Ok(rest) = path.strip_prefix("~") else {
        return Ok(raw.to_string());
    };
    let Some(home) = dirs::home_dir() else {
        return Err(PathError::NoHomeDir(raw.to_string()));
    };
    Ok(if rest.as_os_str().is_empty() {
        home.display().to_string()
    } else {
        home.join(rest).display().to_string()
    })
}

/// 路径 → 比对键:去 Windows `\\?\` 前缀 → Win 上 `\` 统一成 `/` → Win/mac 小写 → 去尾分隔符。
///
/// 不认识 `~`,要展开先过 `expand_tilde`。产物**不能拿去打开文件**:分隔符与盘符大小写都改了
/// (`E:\X` → `e:/x`),要落盘先用 `canonical_path` 取真身。
///
/// 尾部 `/` 必须去掉:`/a/b/` 与 `/a/b` 是同一目录,留着会让 `prefix_match` 的目录边界判断
/// 失效(`strip_prefix` 剩下 `x.txt`,既不为空也不以 `/` 开头)。`temp_dir()` 在 Windows 上就
/// 带尾反斜杠,`~/.ssh/` 这种手写条目更是常见。但根要保住一个斜杠——全 trim 掉会让 `//`
/// 变成空串,而空串作前缀**恒真**(`strip_prefix("")` 必成功),一条 `"//"` 条目就能让白名单全开。
pub fn normalize_path(raw: &str) -> String {
    // 顺序不能换:先剥 `\\?\` 再换分隔符,否则前缀被改写成 `//?/` 就认不出来了
    let s = strip_verbatim(raw);
    // `\` 只在 Windows 是分隔符。Unix 上它是合法文件名字符,无条件替换会让 `/a\b` 与 `/a/b`
    // 归一成同一个键——映射不再单射,白名单会连带放行不在其内的那个文件。
    #[cfg(windows)]
    let s = s.replace('\\', "/");
    #[cfg(any(windows, target_os = "macos"))]
    let s = s.to_lowercase();
    match s.trim_end_matches('/') {
        "" => "/".to_string(),
        trimmed => trimmed.to_string(),
    }
}

/// 路径的最后一段(`/a/b` → `b`),用作工作区的默认显示名。
///
/// 根没有最后一段(`/`、`C:\`),此时原样返回——总得有个能显示的字符串,
/// 而「根」本身已经是它最好的名字。纯文本映射,不查存在性;尾分隔符由组件的语义吃掉。
pub fn dir_name(raw: &str) -> String {
    Path::new(raw)
        .file_name()
        .and_then(|s| s.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| raw.to_string())
}

/// 真实路径:跟软链、去 `.`,目标可以整条不存在(向上取最近一个存在的祖先,再拼回缺失段)。
///
/// 落盘前只能用它:软链不解析时 `link/x` 在字符串上看着仍在工作区内,判定与写入会指向两处。
/// 只收绝对路径——相对路径该用哪个基准归调用方管,拿进程 cwd 兜底会落到 exe 目录。
/// 返回 canonicalize 的字符串原值(Win 上带 `\\?\`),长路径能力不丢。
pub fn canonical_path(raw: &str) -> std::io::Result<String> {
    let path = Path::new(raw);
    let bad = |what: &str| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{what}: {raw}"))
    };
    if !path.is_absolute() {
        return Err(bad("需要绝对路径"));
    }
    // 只用 canonicalize 一个来源判存在性:`exists()` 会把权限不足、路径过长一并吞成 false,
    // 后面就会一路退到根、报「盘符未挂载」,把真正的原因盖掉。
    match path.canonicalize() {
        Ok(real) => return Ok(real.display().to_string()),
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    // 自底向上收集缺失段,直到撞上第一个存在的祖先
    let mut missing: Vec<&OsStr> = Vec::new();
    let mut cur = path;
    loop {
        // 退到根仍无可用祖先:盘符未挂载 / 共享不可达
        let Some(name) = cur.file_name() else {
            return Err(bad("根路径不可用(盘符未挂载或共享不可达)"));
        };
        missing.push(name);
        // `parent()` 是纯语法切分,路径不存在也照样有父目录;到根才 None(那时上一轮已返回)
        let Some(parent) = cur.parent() else {
            return Err(bad("根路径不可用(盘符未挂载或共享不可达)"));
        };
        match parent.canonicalize() {
            Ok(real_parent) => {
                let mut real = real_parent;
                for name in missing.into_iter().rev() {
                    real.push(name);
                }
                return Ok(real.display().to_string());
            }
            Err(e) if e.kind() == ErrorKind::NotFound => cur = parent,
            Err(e) => return Err(e),
        }
    }
}

/// 按基准绝对化,词法折叠 `.` 与 `..`。不跟软链、不查存在性,目标不存在也能解析。
///
/// `root` 为 `None`(手里没有基准)时,绝对路径照常解析;相对路径**报错**——不报的话它会被
/// 原样交出去,调用方拿去 `fs::read` 就按**进程 cwd** 解析(Tauri 打包后是 exe 目录),既能干
/// 又不报错,只是干错地方。折叠不能省——`../../.ssh` 不折就撞不上黑名单。
pub fn resolve_lexical(raw: &str, root: Option<&str>) -> Result<String, PathError> {
    let expanded = expand_tilde(raw)?;
    let path = Path::new(&expanded);
    let abs = match (path.is_absolute(), root) {
        (true, _) => path.to_path_buf(),
        (false, Some(root)) => Path::new(root).join(path),
        (false, None) => return Err(PathError::NoBase(raw.to_string())),
    };
    let mut out = PathBuf::new();
    for comp in abs.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    Ok(out.display().to_string())
}

/// 去掉 canonicalize 在 Windows 上产出的 `\\?\` 前缀,还原成常规写法。
///
/// 盘符形式直接剥;UNC 形式还原成 `\\server\share`,只剥前缀会剩下无效的 `UNC\`。
/// `canonical_path` 刻意保留前缀(才能突破 260 字符上限),所以「存配置 / 显示 / 比对」走这个,
/// 真要拿去打开文件时用它的原值——两条路不能合并,但都是字符串,由调用方决定怎么解释。
pub fn strip_verbatim(raw: &str) -> String {
    match raw.strip_prefix("\\\\?\\UNC\\") {
        Some(rest) => format!("\\\\{rest}"),
        None => raw.strip_prefix("\\\\?\\").unwrap_or(raw).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 期望值按平台形态拼:走一遍 `components()` 重建,和 `resolve_lexical` 的出口形态一致
    /// (Windows 上 `/work/a` 与 `\work\a` 是同一个路径,只有重建才会统一成同一种写法)
    fn disp(p: &str) -> String {
        Path::new(p)
            .components()
            .collect::<PathBuf>()
            .display()
            .to_string()
    }

    /// 比对键不带尾部分隔符:`temp_dir()` 在 Windows 上带尾反斜杠,留着会让前缀匹配失效
    #[test]
    fn normalized_key_drops_trailing_separator() {
        assert_eq!(normalize_path("/a/b/"), "/a/b");
        assert_eq!(normalize_path("/a/b///"), "/a/b");
        // 根要保持一个斜杠:trim 成空串会让它成为「匹配一切」的前缀
        assert_eq!(normalize_path("/"), "/");
        assert_eq!(normalize_path("//"), "/");
        assert_eq!(normalize_path("///"), "/");
        // 真实来源:系统临时目录
        let tmp = normalize_path(&std::env::temp_dir().display().to_string());
        assert!(
            tmp == "/" || !tmp.ends_with('/'),
            "临时目录键不应带尾斜杠: {tmp}"
        );
    }

    /// 归一化:Win 上分隔符统一、大小写折叠;Unix 上 `\` 是合法文件名字符,必须原样保留
    #[test]
    fn normalizes_separators_and_case() {
        // `\` 只在 Windows 是分隔符;Linux/mac 上替换会把它和真正的目录分隔混为一谈
        #[cfg(windows)]
        assert_eq!(normalize_path(r"a\b\c"), "a/b/c");
        #[cfg(not(windows))]
        assert_eq!(normalize_path(r"a\b\c"), r"a\b\c");
        // Win/mac 不区分大小写 → 小写化;Linux 区分 → 原样保留
        #[cfg(any(windows, target_os = "macos"))]
        assert_eq!(normalize_path(".PEM"), ".pem");
        #[cfg(not(any(windows, target_os = "macos")))]
        assert_eq!(normalize_path(".PEM"), ".PEM");
    }

    /// `~` 展开成绝对路径;无 `~` 的原样返回;`~user` 不猜;连续分隔符不丢家目录
    #[test]
    fn expands_tilde_to_absolute() {
        let home = dirs::home_dir().expect("home dir");
        let home_s = home.display().to_string();
        let at = |rel: &str| home.join(rel).display().to_string();
        let tilde = |raw: &str| expand_tilde(raw).expect("家目录可用");
        assert_eq!(tilde("~"), home_s);
        assert_eq!(tilde("~/x"), at("x"));
        // 按组件判断:`~//x` 与 `~/x` 等价。字符串前缀只吃掉一层斜杠,剩下的 `/x` 会被
        // `join` 当成新根,家目录整个丢掉
        assert_eq!(tilde("~//x"), at("x"));
        assert_eq!(tilde("~/"), home_s);
        assert_eq!(tilde("/etc/passwd"), "/etc/passwd");
        assert_eq!(tilde("src/main.rs"), "src/main.rs");
        // `~foo` 是「用户 foo 的家目录」,本实现不支持,不能误当成 `~` 展开
        assert_eq!(tilde("~foo/x"), "~foo/x");
        #[cfg(windows)]
        assert_eq!(tilde(r"~\x"), at("x"));
    }

    /// 基准解析:`.` / `..` 原样进、折后出;绝对路径与 `~` 展开后都不拼基准
    #[test]
    fn resolves_lexically_against_root() {
        let root = Some("/work");
        let at = |raw: &str| resolve_lexical(raw, root).expect("基准可用");
        assert_eq!(at("a/./b"), disp("/work/a/b"));
        assert_eq!(at("a/b/../c"), disp("/work/a/c"));
        // 越界的 `..` 停在根,不会把绝对路径退化成相对路径
        assert_eq!(at("../../x"), disp("/x"));
        // 绝对路径不拼基准(折叠后尾分隔符会消失,按 components 比较)
        let abs = std::env::temp_dir();
        let resolved = resolve_lexical(&abs.display().to_string(), root).expect("绝对路径");
        assert_eq!(Path::new(&resolved), abs.as_path());
        // `~` 展开后已是绝对路径,同样不拼基准
        let home = dirs::home_dir().expect("home dir");
        assert_eq!(at("~/x"), home.join("x").display().to_string());
    }

    /// 没有基准:绝对路径照常解析,相对路径**报错**——原样返回会被调用方按 cwd 解析
    #[test]
    fn resolves_without_base_rejects_relative() {
        assert!(matches!(
            resolve_lexical("../a", None),
            Err(PathError::NoBase(_))
        ));
        assert!(matches!(
            resolve_lexical("./a", None),
            Err(PathError::NoBase(_))
        ));
        let abs = std::env::temp_dir();
        let resolved = resolve_lexical(&abs.display().to_string(), None).expect("绝对路径免基准");
        assert_eq!(Path::new(&resolved), abs.as_path());
    }

    /// 真实路径:存在的文件给出规范化结果;不存在的文件(`..` 折过的)也能给出可写路径
    #[test]
    fn canonicalizes_existing_and_missing() {
        let tmp = std::env::temp_dir();
        let root = tmp.display().to_string();
        // 不存在的文件名:父目录 canonicalize + 拼文件名
        let unresolved =
            resolve_lexical("__hi_agent_no_such__/../__hi_agent_new__.txt", Some(&root))
                .expect("基准可用");
        let missing = canonical_path(&unresolved).expect("祖先可用");
        let missing = Path::new(&missing);
        assert!(missing.is_absolute());
        assert_eq!(missing.file_name().unwrap(), "__hi_agent_new__.txt");
        // 已存在的目录:跟 canonicalize
        let existing = resolve_lexical(&root, Some(&root)).expect("基准可用");
        assert_eq!(
            canonical_path(&existing).unwrap(),
            tmp.canonicalize().unwrap().display().to_string()
        );
    }

    /// 祖先链上连着几级都没创建,也要给出落点:最近一个存在的祖先决定真身,其余原样拼回
    #[test]
    fn canonicalizes_path_with_missing_ancestors() {
        // 期望值以 canonicalize 后的基准为准(`/tmp` 在 macOS 上本身就是软链)
        let base = std::env::temp_dir().canonicalize().expect("temp dir");
        let tmp = std::env::temp_dir();
        let root = tmp.display().to_string();
        let at = |rel: &str| {
            canonical_path(&resolve_lexical(rel, Some(&root)).expect("基准可用")).expect("祖先可用")
        };

        // 三级目录全不存在
        assert_eq!(
            at("__hi_a__/__hi_b__/__hi_c__/f.txt"),
            base.join("__hi_a__/__hi_b__/__hi_c__/f.txt")
                .display()
                .to_string()
        );
        // 缺失祖先与 `..` 叠加:先折再拼,不存在的段被 `..` 吃掉,不会带出基准
        assert_eq!(
            at("__hi_a__/x/../../__hi_d__/f.txt"),
            base.join("__hi_d__/f.txt").display().to_string()
        );
    }

    /// 相对路径不猜基准:宁可报错,也不拿进程 cwd 去解析(Tauri 打包后 cwd 是 exe 目录)
    #[test]
    fn canonical_rejects_relative_path() {
        assert!(canonical_path("a/b.txt").is_err());
    }

    /// 条目规范化:含 `~` 的得先展开再归一化;其余只做分隔符/大小写处理
    #[test]
    fn normalizes_entries_expanding_tilde() {
        let home = dirs::home_dir().expect("home dir");
        #[allow(unused_mut)]
        let mut expected = home.join(".ssh").display().to_string().replace('\\', "/");
        #[cfg(any(windows, target_os = "macos"))]
        {
            expected = expected.to_lowercase();
        }
        assert_eq!(
            normalize_path(&expand_tilde("~/.ssh").expect("家目录可用")),
            expected
        );
        assert_eq!(normalize_path(".env"), ".env");
        // 已经是绝对路径的待查路径同样走它,展开是恒等
        assert_eq!(normalize_path("/etc/passwd"), "/etc/passwd");
    }

    /// 显示名取最后一段:尾分隔符不算一段;根没有最后一段,原样返回
    #[test]
    fn dir_name_takes_last_segment() {
        assert_eq!(dir_name("/a/b"), "b");
        assert_eq!(dir_name("/a/b/"), "b");
        assert_eq!(dir_name("/a/b///"), "b");
        // 根:没有 file_name,回落成路径本身(用它自己当名字)
        assert_eq!(dir_name("/"), "/");
        // 相对路径同样只取最后一段,不展开
        assert_eq!(dir_name("a/b"), "b");
    }

    /// Windows 扩展长度前缀:盘符剥掉、UNC 还原成双反斜杠,无前缀原样返回
    #[test]
    fn strips_windows_verbatim_prefix() {
        // 盘符形式
        assert_eq!(strip_verbatim(r"\\?\C:\Users\x"), r"C:\Users\x");
        // 无前缀 / Unix 路径:原样返回
        assert_eq!(strip_verbatim(r"C:\Users\x"), r"C:\Users\x");
        assert_eq!(strip_verbatim("/home/x"), "/home/x");
        // UNC 形式:必须还原成双反斜杠开头,而非残留 "UNC\"
        assert_eq!(strip_verbatim(r"\\?\UNC\server\share"), r"\\server\share");
    }
}
