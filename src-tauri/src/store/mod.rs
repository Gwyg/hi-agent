//! 存储层：SQLite 单库，程序生成的一切结构化数据都住在这里。
//!
//! # 结构
//!
//! ```text
//! store/
//! ├─ README.md    本层的方案与现状（schema、规矩、已处理的口子、接入时要做的事）
//! ├─ engine/      引擎：连接 / 迁移 / 事务 / 参数化语句（私有，外部不可见）
//! ├─ sql/         0001_init.sql 与迁移纪律
//! ├─ workspace.rs 仓储：工作区
//! ├─ session.rs   仓储：会话（含按工作区清理旧会话）
//! └─ message.rs   仓储：消息（唯一认识 `Msg` 的地方，含按水位清理旧消息）
//! ```
//!
//! **引擎管「能不能安全地写」，仓储管「这类数据怎么读写」，业务管「什么时候为什么要写」。**
//!
//! # 三条纪律
//!
//! 1. `store` 之外不出现 SQL。上层看到的是仓储函数，看不到表名、列名、语句。
//!    注意这一条是**约定**而不是编译期约束：`tx` 就是 `rusqlite::Transaction`，业务层真去
//!    `execute` 是拦不住的；拦住它的是「不知道表名和语句长什么样」——那部分只写在仓储里。
//! 2. 仓储里没有业务规则。压缩多少条、标题怎么生成、何时新建会话，全在 `ai/` 与 `services/`。
//! 3. 连接只有一个，方法都是同步短调用，**不跨 `await` 持锁**。
//!
//! # 为什么单库
//!
//! 程序数据的访问模式不只有「追加 + 顺序读」：会话要改名、要按时间排序、要被删除、
//! 将来还要搜索与统计。文件格式要为这些逐个发明机制（墓碑、重写、索引、对账），
//! 那才是真正的维护负担。SQLite 把这些都变成一条语句，代价只是一个依赖和一个二进制文件。
//!
//! 配置仍走 `app.toml`（人要能手改），诊断日志仍是纯文本——那是另外两类东西，不与这里重叠。

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use crate::util::{app_dir, canonical_path, expand_tilde, resolve_lexical, strip_verbatim};

mod engine;
pub use engine::Info;
// 仓储可用的执行原语：三个函数。绑定值用 `rusqlite::params!`（零分配、`Option<T>` 天然绑 NULL），
// 行用 `rusqlite::Row`，本层不导出任何自有类型。
pub(in crate::store) use engine::exec::{execute, query_all, query_one};

pub mod message;
pub mod session;
pub mod workspace;

/// 存储层句柄。整个进程只需要一个（由 `lib.rs` 在启动时装进 Tauri state）。
///
/// 内部是「一个连接 + 一把锁」：桌面单进程、写入本来就串行，连接池在这里只是复杂度。
/// 将来若真出现多窗口并发写，把锁换成按线程连接即可，schema 与仓储都不用动。
pub struct Store {
    conn: Mutex<engine::Conn>,
}

impl Store {
    /// 按默认路径打开（`~/.hi-agent/data/hi-agent.db`）。
    pub fn open() -> Result<Self> {
        Self::open_at(Self::path())
    }

    /// 按指定路径打开，供测试与将来的「自定义数据目录」使用。
    ///
    /// 路径会先归一化（`~` 展开 → 绝对化 → 跟软链 → 去 `\\?\`），见 [`normalize_db_path`]。
    pub fn open_at(path: impl AsRef<Path>) -> Result<Self> {
        let path = normalize_db_path(path.as_ref())?;
        let conn = engine::Conn::open(&path)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// 测试专用：注入一份迁移列表，避免为验证机制而先往正式 schema 里塞业务表。
    #[cfg(test)]
    pub(in crate::store) fn open_with(
        path: impl AsRef<Path>,
        migrations: &[(u32, &str)],
    ) -> Result<Self> {
        let conn = engine::Conn::open_with(path.as_ref(), migrations)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// 默认库路径的**拼装**位置，将来接 `[storage] data_dir` 只改这里。
    ///
    /// 返回的是「意图路径」，不碰磁盘、不做归一化（所以可能还是 `~` 下的字面写法）；
    /// 归一化在 [`normalize_db_path`] 里做一次，**真实落地路径以 [`Info::path`] 为准**。
    pub fn path() -> PathBuf {
        app_dir().join("data").join("hi-agent.db")
    }

    /// 事务闭包。业务层靠它把多个仓储放进同一个事务（落一条消息 + 更新会话）。
    ///
    /// 闭包拿到的是 `rusqlite::Transaction` 本身——仓储函数签名直接用这个类型。
    pub(crate) fn tx<T>(
        &self,
        f: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        self.conn().tx(f)
    }

    pub fn info(&self) -> Result<Info> {
        self.conn().info()
    }

    /// 启动自检：`PRAGMA quick_check` 走一遍，库损坏时当场报出来。
    ///
    /// 暂时没有调用者 —— 启动流程还没决定「检查不过时是拒绝启动还是只告警」，
    /// 定下来再接（`open_and_report` 里的自愈逻辑要先让位）。
    #[allow(dead_code)]
    pub fn check_integrity(&self) -> Result<()> {
        self.conn().check_integrity()
    }

    /// 退出前调用，把 WAL 合并回主库。
    ///
    /// 暂无调用者：退出流程（`RunEvent::Exit`）还没接。留着它是因为这段实现
    /// 已经想清楚了（回退 journal 时跳过、有读者时只 warn），删了下次还要重新推一遍。
    #[allow(dead_code)]
    pub fn checkpoint(&self) -> Result<()> {
        self.conn().checkpoint()
    }

    /// 取连接。
    ///
    /// 锁中毒意味着此前某个线程在持锁时 panic 了（事务会由 Drop 回滚，SQLite 自身仍然自洽），
    /// 所以这里**恢复**而不是让整个存储层永久不可用：桌面应用的取舍是「还能用」优先，
    /// 但一定留一条 warn 日志，别让问题静静消失。
    fn conn(&self) -> MutexGuard<'_, engine::Conn> {
        self.conn.lock().unwrap_or_else(|poisoned| {
            tracing::warn!("存储层锁已中毒（此前有线程 panic），继续复用连接");
            poisoned.into_inner()
        })
    }
}

/// 库文件路径归一化：`~` 展开 → 绝对化 → 跟软链 → 去 `\\?\`。
///
/// 为什么不直接把入参交给 `Connection::open`：
/// - `~/x` 在 SQLite 眼里是**名叫 `~` 的目录**，会在某处悄悄建出一个真正的 `~` 文件夹；
/// - 相对路径会被进程 cwd 解析，而 Tauri 打包后 cwd 是 exe 所在目录，库就落到安装目录里
///   （`util::path` 的既定取舍：没有基准就报错，不猜）。
///
/// 文件本身允许不存在（首次启动）——`canonical_path` 会向上取最近存在的祖先再拼回缺失段，
/// 父目录由 `Conn::open_with` 里的 `ensure_dir` 创建。
fn normalize_db_path(raw: &Path) -> Result<PathBuf> {
    let text = raw
        .to_str()
        .with_context(|| format!("数据库路径含非 UTF-8 字符: {}", raw.display()))?;
    let expanded = expand_tilde(text).map_err(|e| anyhow::anyhow!("{e}"))?;
    let resolved = resolve_lexical(&expanded, None)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| format!("数据库路径必须是绝对路径（或 `~/` 开头）: {text}"))?;
    let real = canonical_path(&resolved).with_context(|| format!("解析数据库路径失败: {text}"))?;
    Ok(PathBuf::from(strip_verbatim(&real)))
}

/// 打开默认库并把「路径 + 版本 + 表清单」打进日志。`lib.rs` 的启动流程用它。
///
/// **只有 `Store::open` 失败才拦启动**。读取诊断信息失败只 warn：那几条信息是用来
/// 排查问题的，不该反过来成为「打不开程序」的原因。
pub fn open_and_report() -> Result<Store> {
    let store = Store::open()?;
    match store.info() {
        Ok(info) => tracing::info!(
            path = %info.path.display(),
            size = info.size_bytes,
            version = info.schema_version,
            journal = %info.journal_mode,
            sqlite = %info.sqlite_version,
            tables = info.tables.len(),
            "存储层已打开"
        ),
        Err(e) => tracing::warn!(error = format!("{e:#}"), "读取库信息失败，跳过启动日志"),
    }
    Ok(store)
}
