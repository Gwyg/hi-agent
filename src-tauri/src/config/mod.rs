//! 配置模块：聚合各配置域(llm / path_scope / permissions)的初始化入口与对外访问器。
//!
//! 设计（各域自治 + 统一 TOML 根）:
//! - 每个配置域自己拥有:运行时结构体 + RwLock 单例 + 访问器 + `init()`;
//! - 本模块定义 `app.toml` 结构根 `AppConfig`(聚合 `[path_scope]` / `[llm]` / `[permissions]` /
//!   `[resume]` 四段),并只持有
//!   **配置域自身**的职责——`config_path()`(文件位置)、「解析失败回退默认」策略,以及唯一的
//!   写入原语 `persist()`(加锁串行 + 读 → 交给 util 改 → 原子落盘)。
//!   格式层的活(建段、修坏段、沿用装饰保住注释)在 `util::toml`,文件读写原语在 `util::files`;
//!   本模块是唯一知道「哪个文件、哪些键属于哪个段」的地方。
//! - **写回真源是「本次变更」,不是「内存全量状态」**:调用方声明改哪个表的哪几个键,
//!   未声明的节点一个字节都不动(注释、空行、多行数组排版、未知 key 全保留)。
//!   这是与早先 `AppConfig::save()`(全量序列化 + `merge_doc` 覆盖)的根本区别:那套写法是
//!   「默认写」——新增字段自动进入写回名单,且一次动作等于重写整个文件;
//! - `init()` 是唯一的启动编排入口,`run()` 只调 `config::init()`,不感知任何子系统细节;
//! - 业务代码一律通过 `config::xxx()` 访问器直取,不经由任何中央 `Config` 壳分发。

pub mod llm;
pub mod logging;
pub mod path_scope;
pub mod permissions;
pub mod resume;

use std::sync::Mutex;

use tracing::warn;

use crate::util::{app_dir, atomic_write, edit_text, read_to_string_or_default};

/// 写回值类型(真源在 `util::toml::Value`,此处转发给各配置域,免得它们直接依赖 `toml_edit`)。
pub use crate::util::Value;

/// `app.toml` 路径:`~/.hi-agent/app.toml`(应用数据根见 `util::app_dir`)。
fn config_path() -> std::path::PathBuf {
    app_dir().join("app.toml")
}

/// `app.toml` 结构根:当前 `[path_scope]` + `[llm]` + `[permissions]` 三段。
///
/// 各段的「文件形状」类型由其所属子模块持有(path_scope 段 = `PathScopeConfig`、llm 段 = `LlmConfig`、
/// permissions 段 = `PermissionsConfig`),
/// 二者均经下方 `pub use` 再导出到 config 根,故本结构体字段可直接用非限定名引用,无需 `llm::` /
/// `path_scope::` 前缀。本结构体只做**跨域聚合**与**读取**;写入不经它(见 `persist`)——新增配置段时,
/// 在此加一个字段、并 `pub use` 对应子模块的 section 类型即可。
#[derive(serde::Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub path_scope: PathScopeConfig,
    #[serde(default)]
    pub llm: LlmConfig,
    #[serde(default)]
    pub permissions: PermissionsConfig,
    #[serde(default)]
    pub resume: ResumeConfig,
}

pub use llm::{llm_config, set_llm_config, LlmConfig};
pub use path_scope::{workspace_path, PathScopeConfig};
pub(crate) use permissions::ActionKind;
pub use permissions::{bash_policy, PermissionsConfig};
pub use resume::ResumeConfig;

impl AppConfig {
    /// 加载 `app.toml`;文件缺失 / 解析失败 → 全默认(空 workspace + 空 llm)。
    ///
    /// 只读不写:加载结果用于启动时分发给各配置域,`app` 随即被丢弃,不存在「读出来又被写回去」
    /// 的路径(那是旧 `save()` 的失效模式来源)。
    pub fn load() -> AppConfig {
        let text = read_to_string_or_default(&config_path());
        toml::from_str(&text).unwrap_or_default()
    }
}

/// 配置写入串行化。
///
/// 切换工作目录与保存大模型配置是两个可并发的命令,各自都是「读整份文件 → 改一处 → 写回」;
/// 没有这层,后写的一方会用自己读到的旧文档把先写方的改动用旧值盖回去(经典 lost update)。
/// 频率极低(一次用户操作一次写)、临界区只有几毫秒文件 IO,单把 Mutex 足够;
/// 跨进程的咨询锁不需要——桌面应用单实例。
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// 定点写回 `app.toml`:本模块唯一的写入口。
///
/// 读一次 → 交给 `util::toml::edit_text` 只改 `entries` 声明的键 → 原子写回。
/// 格式层的活(建段、修坏段、沿用装饰保住注释)全在 util;本函数只管 config 的语义:
/// 文件在哪、为什么加锁、怎么保证不半截落盘。
///
/// `path` 是从文档根到目标表的路径(空切片 = 顶层,`&["llm", "provider"]` = 三层),
/// `entries` 里 `None` 表示删掉该键。一次调用 = 一个事务:全程只读一次文件、写一次盘。
pub fn persist(path: &[&str], entries: &[(&str, Option<Value>)]) -> std::io::Result<()> {
    // 锁中毒不代表文档不可写(闭包 panic 时文件并未被写坏,原子写保证了这一点),继续执行
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let file = config_path();
    let text = read_to_string_or_default(&file);
    // util 不认识文件,错误信息里没有路径;在这里补上,用户才知道该去修哪个文件
    let out = edit_text(&text, path, entries)
        .map_err(|e| std::io::Error::new(e.kind(), format!("{}: {e}", file.display())))?;
    atomic_write(&file, out.as_bytes())?;

    // api_key 已是密文,但文件里还有工作目录、权限规则、模型选择这些用户配置:
    // 收紧到 0600 成本为零,顺手不让同机其他账户读改。失败只告警——内容已经落盘,
    // 返回错误会让调用方以为没保存成功。新建时这一步也顺便定下权限,后续写入沿用。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)) {
            warn!("无法把 {} 权限收紧到 0600: {e}", file.display());
        }
    }
    Ok(())
}

/// 启动编排入口：`run()` 仅需调用一次。
///
/// 顺序依赖：
/// 1. 先装日志（后续 init 的 warn/info 才能落盘）；
/// 2. 读 `.env`(只对 `[llm].api_key` 有兜底作用:页面保存的配置代表用户明确意图,
///    优先于环境变量;端点写死、model 不读 env);
/// 3. 一次性加载 `app.toml` 全量 → 按段分发给各配置域（path_scope / llm / permissions / resume）；
/// 4. permissions 经 `[permissions]` 段注入用户规则（覆盖内置默认的判定结果；
///    分类词表与致命命令均为代码内置，不进配置）。
///    匹配引擎(`llm::tools::support::bash_safety`)在首次匹配时自行从 config 拉取该规则，
///    故 config 不反向依赖匹配引擎。
pub fn init() -> anyhow::Result<()> {
    logging::init();
    dotenvy::dotenv().ok();
    let app = AppConfig::load();

    // 老版本的 `[path_scope].path` 已经不生效了(工作区改由 `workspace` 表管理)。
    // 留着只会让人以为改了有效,所以认出来、告警、删掉。放在这里而不是 `path_scope::init`:
    // 这里是唯一的真实启动路径,各域的 init 会被测试反复调用,不该碰用户的 app.toml。
    if matches!(app.path_scope.path.as_deref(), Some(p) if !p.trim().is_empty()) {
        warn!("[path_scope].path 已废弃(工作区改由工作区列表管理),忽略并删除该键");
        if let Err(e) = persist(&["path_scope"], &[("path", None::<Value>)]) {
            warn!("删除废弃的 path 键失败(下次启动会再试): {e}");
        }
    }

    path_scope::init(&app.path_scope);
    llm::init(&app.llm);
    permissions::init(&app.permissions);
    resume::init(&app.resume);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::AppConfig;

    /// 磁盘段名必须与结构体字段名一致(`[path_scope]` ↔ `path_scope`)。
    ///
    /// `AppConfig` 没有 `deny_unknown_fields`,段名写错不会报错,只会**静默变成默认值**——
    /// 用户改了配置却发现不生效,最难查的一类问题。这条测试把契约钉在「段名」这一层。
    #[test]
    fn section_names_match_fields() {
        let app: AppConfig = toml::from_str(
            r#"
[path_scope]
path = "D:/proj"
sensitive_suffixes = [".custom"]

[llm]
model = "m1"
"#,
        )
        .expect("段名与字段不一致时这里会失败");
        assert_eq!(app.path_scope.path.as_deref(), Some("D:/proj"));
        assert_eq!(app.path_scope.sensitive_suffixes, vec![".custom"]);
        assert_eq!(app.llm.model.as_deref(), Some("m1"));
    }
}
