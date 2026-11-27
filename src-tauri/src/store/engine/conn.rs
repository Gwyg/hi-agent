//! 连接：把「一个库文件」变成可以安全读写的 [`Conn`]。
//!
//! 打开时依次做四件事：设参数（WAL / synchronous / busy_timeout）、迁移到最新版本、
//! 必要时自检、记日志。任何一步失败都直接报错——**绝不自动删除或重建库文件**。

use anyhow::{bail, Context, Result};
use rusqlite::{Connection, Transaction};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::migrate;

use crate::util::files;

/// 数据库连接。同一进程只应存在一个（由 [`crate::store::Store`] 持有并加锁）。
///
/// 用 `unchecked_transaction` 而非 `transaction`：互斥锁已保证独占，不必再靠 `&mut self`
/// 表达一次；这才能让 [`Conn::tx`] 保持 `&self`，与「句柄放在 `Mutex` 里」的使用方式一致。
pub(in crate::store) struct Conn {
    inner: Connection,
    path: PathBuf,
}

/// 库的诊断快照。纯只读，供日志与将来的「关于 / 诊断」界面使用。
#[derive(Debug)]
pub struct Info {
    pub path: PathBuf,
    pub size_bytes: u64,
    /// 当前 schema 版本（`PRAGMA user_version`）
    pub schema_version: u32,
    pub journal_mode: String,
    pub sqlite_version: String,
    /// 用户表（不含 `sqlite_` 内建表），按名字排序
    pub tables: Vec<String>,
}

impl Conn {
    /// 用程序内建的迁移列表打开（正式的入口）。
    pub(in crate::store) fn open(path: &Path) -> Result<Self> {
        Self::open_with(path, migrate::MIGRATIONS)
    }

    /// 用指定迁移列表打开。测试用它注入自己的迁移，不必先往正式 schema 里塞业务表。
    pub(in crate::store) fn open_with(path: &Path, migrations: &[(u32, &str)]) -> Result<Self> {
        if let Some(parent) = path.parent() {
            files::ensure_dir(parent)
                .with_context(|| format!("创建数据目录失败: {}", parent.display()))?;
        }

        let mut inner = Connection::open(path).with_context(|| {
            format!(
                "打开数据库失败: {}（若该文件不是 SQLite 库或已损坏，请先备份再手动处理；程序不会自动删除数据）",
                path.display()
            )
        })?;

        // 判据要在 `configure` 之前取：`configure` 会把日志模式设成 WAL，那一步在干净库上
        // 也会建出一个 0 字节的 `-wal`。先读，问的才是「我碰它之前它干净吗」。
        let suspicious = wal_has_pending_frames(path);

        let wal =
            configure(&inner).with_context(|| format!("配置数据库连接失败: {}", path.display()))?;
        // 迁移失败的错误链必须自带路径：启动失败时用户只看到弹窗那一行字，
        // 而「哪个文件出问题」是唯一能让他自己排查的信息（自定义数据目录之后尤其如此）。
        let version = migrate::apply(&mut inner, migrations)
            .with_context(|| format!("迁移数据库失败: {}", path.display()))?;

        let conn = Self {
            inner,
            path: path.to_path_buf(),
        };
        // 判据靠 `-wal` 文件；非 WAL 模式下它根本不会存在，等于失去了「上次是否正常退出」
        // 的信号。这种库宁可多扫一遍全库，也不留盲区。
        let check = suspicious || !wal;
        if check && suspicious {
            tracing::warn!(
                path = %conn.path.display(),
                "上次未正常退出（-wal 里有未合并的帧），执行完整性检查"
            );
        }
        if check {
            conn.check_integrity()?;
        }
        tracing::info!(
            path = %conn.path.display(),
            version,
            wal,
            checked = check,
            "存储层就绪"
        );
        Ok(conn)
    }

    /// 事务闭包：`f` 正常返回才提交，`?` 早退或 panic 一律回滚（`Transaction` 的 Drop 负责）。
    ///
    /// 事务对象直接交给 `f`（`rusqlite::Transaction`）。仓储不自己开事务——入口只有这一个，
    /// 且必须先经过 [`crate::store::Store::tx`] 那把锁。
    pub(in crate::store) fn tx<T>(
        &self,
        f: impl FnOnce(&Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let tx = self.inner.unchecked_transaction().context("开启事务失败")?;
        let out = f(&tx)?;
        tx.commit().context("提交事务失败")?;
        Ok(out)
    }

    pub(in crate::store) fn info(&self) -> Result<Info> {
        // 读不到大小就记 0：这只是诊断信息，不值得为它让调用方多处理一种错误
        let size_bytes = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        let journal_mode: String = self
            .inner
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .context("读取 journal_mode 失败")?;
        let tables = self
            .inner
            .prepare(
                "SELECT name FROM sqlite_master \
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .and_then(|mut stmt| {
                stmt.query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<String>>>()
            })
            .context("读取表清单失败")?;
        Ok(Info {
            path: self.path.clone(),
            size_bytes,
            // 版本号怎么读只留一处实现（`migrate` 也要用它判断该跑哪几步）
            schema_version: migrate::read_version(&self.inner)?,
            journal_mode,
            sqlite_version: rusqlite::version().to_string(),
            tables,
        })
    }

    /// `PRAGMA quick_check`：比 `integrity_check` 快得多，足够拦下「文件被截断 / 页损坏」。
    pub(in crate::store) fn check_integrity(&self) -> Result<()> {
        let verdict: String = self
            .inner
            .query_row("PRAGMA quick_check", [], |r| r.get(0))
            .context("完整性检查未执行")?;
        if !verdict.eq_ignore_ascii_case("ok") {
            bail!("数据库完整性检查未通过: {verdict}");
        }
        Ok(())
    }

    /// 把 WAL 里的内容合并回主库并截断（退出前调用，避免 `-wal` 无限增长）。
    ///
    /// 暂无调用者：`Store::checkpoint` 是唯一的转发口，退出流程还没接。
    #[allow(dead_code)]
    pub(in crate::store) fn checkpoint(&self) -> Result<()> {
        let mode: String = self
            .inner
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .context("读取 journal_mode 失败")?;
        if !mode.eq_ignore_ascii_case("wal") {
            // 回退到 rollback journal 的库没有 `-wal` 可合并。继续执行它会返回 -1，
            // 被下面的判断读成「仍有读者」，于是每次退出都留一条假警告。
            return Ok(());
        }
        let busy: i64 = self
            .inner
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))
            .context("WAL checkpoint 失败")?;
        if busy != 0 {
            // 有其它连接在读时不是错误，只是没截断干净
            tracing::warn!(busy, "WAL checkpoint 未完全完成（仍有读者）");
        }
        Ok(())
    }
}

/// 上次是否留下未合并的 WAL 帧 —— 也就是「没走完退出流程」。
///
/// 正常退出会 `checkpoint(TRUNCATE)` 把 `-wal` 截到 0 字节（文件仍留着），所以非 0 长度
/// 就说明上次写到一半没了。只有这类库值得花一次全库扫描去验结构：干净退出的库不扫
/// 不是「不校验」——SQLite 读页时本就校验页头与溢出链，这里多给的只是**提前**发现。
/// 误判（有读者导致没截干净）只会多查一次，方向安全。
fn wal_has_pending_frames(db_path: &Path) -> bool {
    // SQLite 的附属文件是**追加** `-wal`，不是换扩展名：`with_extension("wal")`
    // 会把 `hi-agent.db` 变成 `hi-agent.wal`，那不是它认的文件名。
    let mut wal = db_path.as_os_str().to_owned();
    wal.push("-wal");
    std::fs::metadata(PathBuf::from(wal)).is_ok_and(|m| m.len() > 0)
}

/// 设置连接参数并**回读校验**，返回是否成功启用了 WAL。
///
/// 每次建立连接都要跑一遍：`busy_timeout` / `synchronous` 只活在连接对象里，关闭即消失。
/// `journal_mode` / `auto_vacuum` 虽是库文件头的持久属性，仍要执行——前者用来发现
/// 「当前文件系统不支持共享内存、WAL 被静默拒绝」，后者的价值全在回读那半段。
///
/// **拿不到 WAL 不算错误**：网络盘、虚拟机共享目录、FUSE 挂载都会降级，此时退到
/// rollback journal + `FULL` 同步，功能正确只是变慢。为「写得更快」把程序拦在门外不值得。
///
/// 回读不是多余：`journal_mode` 的降级**不报错**，`synchronous` / `auto_vacuum` 不生效
/// 也不报错，只在很久以后表现为「偶发丢数据」或「删了文件不变小」。
///
/// **不开 `foreign_keys`**：schema 里没有 `REFERENCES`，一致性由业务事务保证。真要启用
/// 必须在这里开并回读断言——它是连接级开关，DDL 写了外键也不会自动生效。
fn configure(conn: &Connection) -> Result<bool> {
    conn.busy_timeout(Duration::from_millis(5_000))
        .context("设置 busy_timeout 失败")?;

    // auto_vacuum 必须在**任何建表之前**设置：有表后再执行是静默 no-op，想改只能整库 VACUUM。
    // 库已是 INCREMENTAL 时这行同样什么都不做（值没变），所以只有首次建库那次真正生效，
    // 后面每次打开的价值全在下面的回读。
    // 选 INCREMENTAL 而非 FULL：删行只把页标空闲，何时搬页截断交给我们（`PRAGMA
    // incremental_vacuum`，删完会话/工作区后调）；FULL 每次提交都搬页，一轮对话写好几次，不划算。
    conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")
        .context("设置 auto_vacuum 失败")?;

    // journal_mode 会返回生效后的模式，必须 query_row；execute 会报 "returned results"
    let mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
        .context("切换 WAL 失败")?;
    let wal = mode.eq_ignore_ascii_case("wal");

    // 档位必须跟着日志模式走：`NORMAL` 只在 WAL 下安全（断电最多丢最后一次提交），
    // 而在 rollback journal 模式下同样的档位**可能损坏库文件**，那里只能用 `FULL`。
    let sync_level = if wal { "NORMAL" } else { "FULL" };
    conn.pragma_update(None, "synchronous", sync_level)
        .context("设置 synchronous 失败")?;

    if !wal {
        tracing::warn!(
            journal_mode = %mode,
            "无法启用 WAL（文件系统可能不支持共享内存），已回退到 {mode} 模式并保持 FULL 同步；\
             功能不受影响，但写入会变慢、读写互斥。建议把数据目录放在本地磁盘"
        );
    }

    let expected_sync: i64 = if wal { 1 } else { 2 };
    let sync: i64 = conn
        .pragma_query_value(None, "synchronous", |r| r.get(0))
        .context("回读 synchronous 失败")?;
    if sync != expected_sync {
        bail!("synchronous 未生效（期望 {expected_sync}，实际 {sync}）");
    }
    let auto_vacuum: i64 = conn
        .pragma_query_value(None, "auto_vacuum", |r| r.get(0))
        .context("回读 auto_vacuum 失败")?;
    if auto_vacuum != 2 {
        // 两种「未生效」要区别对待：空库上设置必然生效，仍不生效是真故障，bail；
        // 有表（老库）时 SQLite 规范就是静默 no-op，不是错误——为「不自动缩文件」打不开
        // 应用不值得，只 warn 并提示整库 VACUUM。
        let tables: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table'",
                [],
                |r| r.get(0),
            )
            .context("回读表数量失败")?;
        if tables == 0 {
            bail!(
                "auto_vacuum 未生效（期望 2=INCREMENTAL，实际 {auto_vacuum}）：\
                 空库上设置都应生效，说明当前 SQLite 构建不支持自动空间回收"
            );
        }
        tracing::warn!(
            auto_vacuum,
            tables,
            "auto_vacuum 无法切换到 INCREMENTAL（库中已有表时 SQLite 会静默忽略）；\
             删数据后文件不会自动变小，需要时请整库 VACUUM 重建"
        );
    }
    Ok(wal)
}

#[cfg(test)]
mod tests {
    use super::{configure, wal_has_pending_frames, Conn};
    use rusqlite::Connection;

    /// WAL 拿不到时不能拦启动，但同步档位必须退到 `FULL`——`NORMAL` 在 rollback
    /// journal 模式下断电可能损坏库文件。内存库天然无法启用 WAL，正好走这条分支。
    #[test]
    fn falls_back_to_full_sync_when_wal_unavailable() {
        let conn = Connection::open_in_memory().unwrap();
        // 先建表：让 auto_vacuum 的回读走「有表 = 正常 no-op」而不是「空库 = 故障」
        conn.execute_batch("CREATE TABLE t (id INTEGER)").unwrap();

        let wal = configure(&conn).unwrap_or_else(|e| panic!("降级不应报错: {e}"));
        assert!(!wal, "内存库不可能启用 WAL");

        let sync: i64 = conn
            .pragma_query_value(None, "synchronous", |r| r.get(0))
            .unwrap();
        assert_eq!(sync, 2, "非 WAL 模式下必须是 FULL(2)，NORMAL 有损坏风险");
    }

    /// 新库按 INCREMENTAL 打开：删掉大半数据 + 一次 `incremental_vacuum` 后文件确实变小。
    /// 两件事必须一起验——只断言 pragma == 2 不够（值对但不生效也可能）。
    #[test]
    fn incremental_vacuum_reclaims_deleted_pages() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("vac.db");
        let conn = Conn::open_with(&db, &[]).unwrap();

        let auto_vacuum: i64 = conn
            .inner
            .pragma_query_value(None, "auto_vacuum", |r| r.get(0))
            .unwrap();
        assert_eq!(auto_vacuum, 2, "新库必须是 INCREMENTAL");

        conn.inner
            .execute_batch(
                "CREATE TABLE blob_store (id INTEGER PRIMARY KEY, body TEXT);
                 INSERT INTO blob_store (body)
                   SELECT hex(randomblob(2000)) FROM (WITH RECURSIVE n(i) AS
                     (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 300) SELECT i FROM n);",
            )
            .unwrap();

        let pages_before: i64 = conn
            .inner
            .query_row("PRAGMA page_count", [], |r| r.get(0))
            .unwrap();
        conn.inner.execute("DELETE FROM blob_store", []).unwrap();
        conn.inner
            .execute_batch("PRAGMA incremental_vacuum")
            .unwrap();
        let pages_after: i64 = conn
            .inner
            .query_row("PRAGMA page_count", [], |r| r.get(0))
            .unwrap();

        assert!(
            pages_after < pages_before,
            "删除后应回收页面：前 {pages_before} 页，后 {pages_after} 页"
        );
    }

    /// 有表的老库（`auto_vacuum = 0`）：切不过去是 SQLite 既定行为，**不能因此打不开应用**。
    /// 失败方式很难看：写成 bail 时，任何在别处建过表的库都会让程序启动即失败。
    #[test]
    fn opens_legacy_database_whose_auto_vacuum_cannot_change() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("legacy.db");
        {
            let raw = Connection::open(&db).unwrap();
            raw.execute_batch("CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (1);")
                .unwrap();
        }

        let conn = Conn::open_with(&db, &[]).expect("有表的老库必须仍能打开");
        let auto_vacuum: i64 = conn
            .inner
            .pragma_query_value(None, "auto_vacuum", |r| r.get(0))
            .unwrap();
        assert_eq!(auto_vacuum, 0, "老库不会凭空变成 INCREMENTAL");
        let kept: i64 = conn
            .inner
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kept, 1, "降级只是不缩文件，数据一条都不能少");
    }

    /// 已有表的库不再接受切换：这是「必须在建表前设」的守卫，也是上面断言存在的理由。
    #[test]
    fn auto_vacuum_cannot_change_after_tables_exist() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("late.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE t (id INTEGER);").unwrap();

        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")
            .unwrap();
        let auto_vacuum: i64 = conn
            .pragma_query_value(None, "auto_vacuum", |r| r.get(0))
            .unwrap();
        assert_eq!(auto_vacuum, 0, "有表之后设置是静默 no-op，不会报错");
    }

    /// 判据：`-wal` 有未合并帧 ⇔ 上次没走完退出流程。一次跑完「干净 → 脏 → 归零」三段，
    /// 顺带证明命中只意味着多查一次、库照样能打开。判据为真那条分支由代码保证（不为此开
    /// 测试缝），这里只守最容易悄悄写错的判据本身。
    #[test]
    fn pending_wal_marks_a_database_as_suspicious() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("crash.db");

        // 干净状态：WAL 模式下关掉最后一个连接，SQLite 会 checkpoint 并把 -wal 清掉
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "PRAGMA journal_mode = WAL;
                 CREATE TABLE t (id INTEGER);
                 INSERT INTO t VALUES (1);",
            )
            .unwrap();
        }
        assert!(!wal_has_pending_frames(&db), "正常关闭的库不该被判为可疑");

        // 崩溃现场：事务已提交，但帧还在 -wal 里没合并（连接仍开着，等价于进程被强杀）
        let live = Connection::open(&db).unwrap();
        live.execute_batch("INSERT INTO t VALUES (2);").unwrap();
        assert!(wal_has_pending_frames(&db), "有未合并的帧就必须判为可疑");

        // 命中判据只是「多查一次」，不能让库打不开
        Conn::open_with(&db, &[]).expect("带 -wal 的库必须仍能打开");

        // 正常释放后 wal 被合并，判据归零——这也是「下次启动不会白扫一遍」的前提
        drop(live);
        assert!(!wal_has_pending_frames(&db), "正常关闭后判据应归零");
    }
}
