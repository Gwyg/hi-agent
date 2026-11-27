//! 路径范围配置:工作目录根 + 额外白名单 + 敏感清单。
//!
//! 本模块只回答「配置值是什么」;「值意味着什么」归校验引擎(`llm::tools::support::path_scope`)。
//! - 读:默认允许,敏感黑名单兜底;写:默认拒绝,工作目录根 + 额外白名单。不对称是代价不对称。
//! - 三张清单都有代码内置基线,app.toml 里的同名键只能**追加**(内置的删不掉,且内置那份不落盘,
//!   否则升级推不到用户)。
//! - **工作目录根不从这里读**:工作区是 SQLite 里的一行(`workspace` 表),由启动流程
//!   (`services::workspace::init`)算出来再 [`set_root`] 注入。本模块只持有那个字符串,
//!   供 `ai/` 的工具层当边界根——三层单向依赖,`ai/` 不能反过来问 `services/`。
//! - 因此清单 `init` 注入一次 → `OnceLock`,以 `&'static [T]` 借出;根会被运行时改写 → `RwLock`。
//! - 对外一改四读:`set_root` / `workspace_path` / `extra_allowed` / `sensitive_suffixes` /
//!   `sensitive_paths`。**对外一律字符串**:路径的文本表示才是这个模块的交换形态,谁要打开文件谁自己转。

use serde::Deserialize;
use std::sync::{OnceLock, RwLock};
use tracing::warn;

use crate::util::{app_dir, canonical_path, expand_tilde, normalize_path};

/// app.toml `[path_scope]` 段(磁盘形状)。
///
/// 三张清单用户手写、程序只读——所以本类型连 `Serialize` 都没有:
/// 不是靠 `skip_serializing` 排除,而是想写都写不出去。
///
/// 清单与代码内置基线是**并集**:若用覆盖语义,用户只写一条 `.env` 就把 `~/.ssh` 那批挤掉了。
#[derive(Deserialize, Default)]
pub struct PathScopeConfig {
    /// 已废弃:工作区改由 `workspace` 表管理,启动断点记在 `[resume].workspace`。
    ///
    /// 保留字段只为了**认出来并删掉**:不删的话用户改了它却发现不生效,是最难查的一类问题。
    /// `init` 里读到非空值就告警一次并清键,之后这个字段恒为 `None`。
    #[serde(default)]
    pub path: Option<String>,
    /// 额外白名单:项目根之外仍允许读写的目录(支持 `~`)
    #[serde(default)]
    pub extra_allowed: Vec<String>,
    /// 敏感后缀清单(ends_with 匹配,支持 `~`)
    #[serde(default)]
    pub sensitive_suffixes: Vec<String>,
    /// 敏感路径前缀清单(starts_with 匹配,支持 `~`)
    #[serde(default)]
    pub sensitive_paths: Vec<String>,
}

// ---- 运行时状态:按生命周期分开存 ----

/// 工作目录根:由启动流程注入,运行时可被 `set_root` 改写。
///
/// 存**字符串**(已 canonicalize、去过 `\\?\`)而不是 `PathBuf`:它要喂给 `resolve_lexical`
/// 当基准、要和清单条目做前缀比较——两处都按文本用,存路径反而每次都要转。
static ROOT: RwLock<Option<String>> = RwLock::new(None);

/// 额外允许目录(启动时注入一次,之后只读)。
static EXTRA: OnceLock<Vec<String>> = OnceLock::new();

/// 敏感后缀清单(启动时注入一次,之后只读)。
static SUFFIXES: OnceLock<Vec<String>> = OnceLock::new();

/// 敏感路径前缀清单(启动时注入一次,之后只读)。
static PATHS: OnceLock<Vec<String>> = OnceLock::new();

// ---- 内置基线:代码内置、不落盘,用户只能追加 ----
//
// 三条都是 `fn() -> Vec<String>`:本就同类,形态一致;且 `temp_dir()` 运行时才有值,
// 借不出 `'static`。只在 `init` 时各调一次,分配可忽略。
//
// 方向相反,最该记牢:**敏感清单多一条更安全,允许清单多一条更危险**(每项 = 多一个可写目录)。

/// 内置敏感后缀:依据「泄露即事故」——凭据、私钥、密钥库。
///
/// 已知局限:`.env` 命中不了 `.env.local`,补它需要第三种匹配方式(文件名模式),暂不引入。
fn builtin_sensitive_suffixes() -> Vec<String> {
    [
        ".env",
        ".pem",
        ".key",
        ".p12",
        ".pfx",
        ".keystore",
        ".jks",
        ".kdbx",
        ".netrc",
        ".npmrc",
        ".pypirc",
        ".git-credentials",
        "id_rsa",
        "id_dsa",
        "id_ecdsa",
        "id_ed25519",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

/// 内置敏感路径前缀:按目录边界匹配。`~` 留给注入阶段统一展开,这里只写声明形态。
fn builtin_sensitive_paths() -> Vec<String> {
    let mut paths = [
        "~/.ssh",
        "~/.aws",
        "~/.gnupg",
        "~/.kube",
        "~/.docker",
        "~/.config/gcloud",
    ]
    .map(str::to_string)
    .to_vec();
    // 明文 api_key 落在这个文件里。精确到文件而非 `~/.hi-agent` 整目录:默认工作区就在其下,
    // 整目录拉黑会连工作区一起误伤。位置用 `app_dir()` 算而不是写死 `~/.hi-agent`——
    // 那才是写配置的同一份真源,家目录取不到时它会退到当前目录。
    paths.push(app_dir().join("app.toml").display().to_string());
    paths
}

/// 内置允许目录:只放系统临时目录(「谁都能随意写」的语义),其余由用户在 `extra_allowed` 里显式追加。
fn builtin_extra_allowed() -> Vec<String> {
    vec![std::env::temp_dir().display().to_string()]
}

/// 内置 ∪ 用户:内置在前、逐条变换、按序去重(`transform` 返回 `None` = 该条不收录)。
///
/// 去重必须在**变换之后**做:`~/.ssh` 与 `/home/u/.ssh` 只有归一化/展开后才相等。
/// 保序不用 `HashSet`:条目只有几十条,顺序本身可读(内置在前、用户在后)。
fn merge<T, F>(
    builtin: impl IntoIterator<Item = String>,
    user: impl IntoIterator<Item = String>,
    transform: F,
) -> Vec<T>
where
    T: PartialEq,
    F: Fn(&str) -> Option<T>,
{
    let mut out: Vec<T> = Vec::new();
    for raw in builtin.into_iter().chain(user) {
        let Some(item) = transform(&raw) else {
            continue;
        };
        if !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

/// 路径条目 → 比对键:展开 `~` → 取真实位置 → 归一化。两张路径清单共用它。
///
/// 条目都是绝对路径或 `~` 开头,不支持相对路径——相对名字按进程 cwd 解析会落到 exe 目录,
/// 变成一条与用户意图无关的键。取不到真实位置就丢掉这一条(`merge` 的 `None`)。
fn entry_key(raw: &str) -> Option<String> {
    let expanded = expand_tilde(raw)
        .map_err(|e| warn!("清单条目无法展开 `~`,已忽略({raw}): {e}"))
        .ok()?;
    let real = canonical_path(&expanded)
        .map_err(|e| warn!("清单条目无法定位真实路径,已忽略({raw}): {e}"))
        .ok()?;
    Some(normalize_path(&real))
}

/// 启动注入(三张清单 `OnceLock::set`)。
///
/// **不碰 `ROOT`**:工作目录根要先查库才能确定(工作区是 `workspace` 表里的一行),
/// 本模块看不到库,所以由 `services::workspace::init` 算好后调 [`set_root`] 注入。
///
/// 也**不碰磁盘**:测试会反复调它来重置清单,写盘的事留给 `config::init` 的唯一一次启动。
///
/// 清单不支持热更新:重复调用时清单保持首次的值并告警。失败仅告警,不阻断启动。
pub(crate) fn init(section: &PathScopeConfig) {
    // 三张清单都存**比对键**。两张路径清单同形:展开 `~` → 绝对化 → 归一化,顺序固定,
    // 与判定侧 `resolve_key` 完全一致——清单侧和判定侧对同一条路径必须算出同一个字符串。
    // 后缀是纯名字(`.env`),不碰磁盘;但仍要走一次归一化,把 Win/mac 的大小写折平,
    // 否则比较侧(拿的是小写路径)对 `.PEM` 这种写法永远不命中。
    let extra = merge(
        builtin_extra_allowed(),
        section.extra_allowed.iter().cloned(),
        entry_key,
    );
    if EXTRA.set(extra).is_err() {
        warn!("额外允许目录已初始化,忽略重复注入(清单不支持热更新)");
    }

    let suffixes = merge(
        builtin_sensitive_suffixes(),
        section.sensitive_suffixes.iter().cloned(),
        |raw| Some(normalize_path(raw)),
    );
    if SUFFIXES.set(suffixes).is_err() {
        warn!("敏感后缀清单已初始化,忽略重复注入(清单不支持热更新)");
    }

    let paths = merge(
        builtin_sensitive_paths(),
        section.sensitive_paths.iter().cloned(),
        entry_key,
    );
    if PATHS.set(paths).is_err() {
        warn!("敏感路径前缀清单已初始化,忽略重复注入(清单不支持热更新)");
    }
}

/// 注入工作目录根:调用方(`services::workspace`)保证它是**已统一的绝对目录**。
///
/// 不在这里 canonicalize:那需要磁盘状态,而「这个目录是否可用」是业务判断,
/// 已经在切工作区时做完了。本函数只负责把它放进工具层能读到的地方。
pub fn set_root(path: &str) -> anyhow::Result<()> {
    let mut guard = ROOT
        .write()
        .map_err(|e| anyhow::anyhow!("path scope 锁中毒: {e}"))?;
    *guard = Some(path.to_string());
    Ok(())
}

/// 当前工作目录(绝对路径,已去 `\\?\`)。未初始化返回 `None`,由调用方兜底。
pub fn workspace_path() -> Option<String> {
    ROOT.read().ok()?.clone()
}

/// 额外允许目录清单(内置 ∪ 用户,已 canonicalize 并归一化成比对键)。
///
/// 与两张敏感清单同形,故三个前缀判断共用同一套比较规则。
pub fn extra_allowed() -> &'static [String] {
    EXTRA.get().map(Vec::as_slice).unwrap_or(&[])
}

/// 敏感后缀清单(内置 ∪ 用户,已归一化)。
pub fn sensitive_suffixes() -> &'static [String] {
    SUFFIXES.get().map(Vec::as_slice).unwrap_or(&[])
}

/// 敏感路径前缀清单(内置 ∪ 用户,已展开 `~` 并归一化)。只能当比对键,不能当路径打开。
pub fn sensitive_paths() -> &'static [String] {
    PATHS.get().map(Vec::as_slice).unwrap_or(&[])
}

#[cfg(test)]
mod tests {
    use super::{
        builtin_extra_allowed, builtin_sensitive_paths, builtin_sensitive_suffixes, entry_key,
        merge,
    };

    /// 内置敏感基线必须在:开箱不能是「无保护」状态
    #[test]
    fn builtin_sensitive_baseline_is_present() {
        let suffixes = builtin_sensitive_suffixes();
        assert!(suffixes.iter().any(|s| s == ".env"));
        assert!(suffixes.iter().any(|s| s == "id_rsa"));
        let paths = builtin_sensitive_paths();
        assert!(paths.iter().any(|p| p.ends_with("/.ssh")));
        // 明文 api_key 所在文件,删掉这条等于把密钥敞给读工具
        assert!(paths.iter().any(|p| p.ends_with("app.toml")));
    }

    /// 内置**允许**基线必须最小:未配置时只有系统临时目录。
    ///
    /// 与上一条方向相反,别合并:有人顺手加 `~/.cargo`、`/usr` 进来,这条会红。
    #[test]
    fn builtin_allow_list_is_minimal() {
        assert_eq!(
            builtin_extra_allowed(),
            vec![std::env::temp_dir().display().to_string()],
            "内置允许清单只应有系统临时目录,新增项会放宽 write/edit 的可写范围"
        );
    }

    /// 合并语义:内置在前、用户追加;同一条目的不同写法归一化后只留一条
    #[test]
    fn merge_appends_user_rules_after_builtin() {
        let merged = merge(
            vec!["~/.ssh".to_string()],
            vec![format!(
                "{}/.ssh",
                dirs::home_dir().expect("home dir").display()
            )],
            entry_key,
        );
        assert_eq!(merged.len(), 1);
        assert!(merged[0].ends_with("/.ssh"));
    }
}
