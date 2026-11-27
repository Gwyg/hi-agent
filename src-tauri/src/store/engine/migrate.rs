//! 迁移：版本号 + 有序步骤。
//!
//! 三条纪律（写在这里，也写在 `sql/README.md` 里，因为半年后真正需要被提醒的是人）：
//! 1. **已发布的步骤永不修改**，改 schema 一律新增一步；
//! 2. **一步一个事务**，版本号与 DDL 在同一事务里提交——不会出现「表建好了但版本没升」；
//! 3. **版本号全局唯一、从 1 开始、严格连续**，不按业务域分号（表之间有删除顺序依赖，
//!    顺序必须确定；跳号意味着中间那步被谁删了，老库与新库就此分叉）。
//!
//! 版本号存在 SQLite 自带的 `PRAGMA user_version` 里，不额外建版本表：它就在文件头，
//! 读写零成本，也不会被业务代码误删。

use anyhow::{bail, Context, Result};
use rusqlite::Connection;

/// 全局迁移列表。**这是唯一登记迁移的地方**，与 `sql/` 下的文件名一一对应。
///
/// 只追加：已登记的行永不改动（改了老库不会重跑，只有新库得到新结构，两份库就分叉了）。
/// 新增一步 = 加一个 `sql/000N_*.sql` + 在这里加一行。
pub(in crate::store) const MIGRATIONS: &[(u32, &str)] =
    &[(1, include_str!("../sql/0001_init.sql"))];

/// 把库升到 `migrations` 的最新版本，返回升完后的版本号。
///
/// 一步一个事务：某步失败则整步回滚、库停在原版本，下次启动从同一处重试。
pub(in crate::store) fn apply(conn: &mut Connection, migrations: &[(u32, &str)]) -> Result<u32> {
    ensure_ordered(migrations)?;

    let current = read_version(conn)?;
    let latest = migrations.last().map(|(v, _)| *v).unwrap_or(0);

    if current > latest {
        bail!(
            "数据库版本 v{current} 高于程序支持的 v{latest}：该库由更新版本的程序创建，\
             请升级程序后再打开（本程序不会改写它）"
        );
    }

    let pending: Vec<&(u32, &str)> = migrations.iter().filter(|(v, _)| *v > current).collect();
    if pending.is_empty() {
        return Ok(current);
    }

    for (version, sql) in pending {
        let tx = conn
            .transaction()
            .with_context(|| format!("开启迁移事务失败（v{version}）"))?;
        tx.execute_batch(sql)
            .with_context(|| format!("迁移到 v{version} 失败，已回滚"))?;
        tx.pragma_update(None, "user_version", *version as i64)
            .with_context(|| format!("写入版本号 v{version} 失败，已回滚"))?;
        tx.commit()
            .with_context(|| format!("提交迁移 v{version} 失败，已回滚"))?;
        tracing::info!(version, "已应用迁移");
    }

    Ok(latest)
}

/// 读 `user_version`。负数按 0 处理（只有手工改库才会出现）。
///
/// 对 `store` 内可见而不只是本模块：诊断快照（`conn::Info`）也要报同一个版本号，
/// 两处各写一遍解析，早晚会有一处忘了 `max(0)`。
pub(in crate::store) fn read_version(conn: &Connection) -> Result<u32> {
    conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
        .map(|v| v.max(0) as u32)
        .context("读取 user_version 失败")
}

/// 列表本身写错（乱序、重号、从 0 开始、跳号）是编码事故，要在启动时就炸掉，
/// 而不是等某个用户的库被跳过一个版本才发现。
///
/// 检查顺序有讲究：先判递增，多判一次「连续」。反过来的话 `[2, 1]` 会先报「没从 1 开始」，
/// 把真正的乱序藏起来，看日志的人得自己再推一遍。
fn ensure_ordered(migrations: &[(u32, &str)]) -> Result<()> {
    for pair in migrations.windows(2) {
        if pair[1].0 <= pair[0].0 {
            bail!(
                "迁移列表必须按版本严格递增：v{} 出现在 v{} 之后",
                pair[1].0,
                pair[0].0
            );
        }
    }
    // 空列表合法：表示「这个程序只会 v0」，测试用它注入空 schema。
    // 非空则必须 1,2,3… 连续：跳号说明中间某一步被删了——老库的 user_version 已经跨过它，
    // 新库却会直接跑下一步，两份库从此结构不同，而且永远不会有人发现。
    for (idx, (version, _)) in migrations.iter().enumerate() {
        let expected = idx as u32 + 1;
        if *version != expected {
            bail!(
                "迁移版本号必须从 1 开始且连续：第 {} 步应为 v{expected}，实际是 v{version}\
                 （跳号或删过某一步？已发布的迁移只能新增，不能删改）",
                idx + 1
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::apply;
    use rusqlite::Connection;

    fn temp_path(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        (dir, path)
    }

    fn version(conn: &Connection) -> u32 {
        conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap() as u32
    }

    #[test]
    fn applies_pending_steps_in_order() {
        let (_dir, path) = temp_path("order.db");
        let mut conn = Connection::open(&path).unwrap();
        let migrations = &[
            (1u32, "CREATE TABLE a (id INTEGER PRIMARY KEY);"),
            (2u32, "CREATE TABLE b (id INTEGER PRIMARY KEY);"),
        ];
        assert_eq!(apply(&mut conn, migrations).unwrap(), 2);
        assert_eq!(version(&conn), 2);
        // 幂等：再跑一次不重复执行
        assert_eq!(apply(&mut conn, migrations).unwrap(), 2);
    }

    #[test]
    fn failing_step_rolls_back_and_keeps_old_version() {
        let (_dir, path) = temp_path("rollback.db");
        let mut conn = Connection::open(&path).unwrap();
        apply(&mut conn, &[(1u32, "CREATE TABLE a (id INTEGER);")]).unwrap();

        // 第二步先建表再写错 SQL：整步应回滚，表不留下
        let bad = &[
            (1u32, "CREATE TABLE a (id INTEGER);"),
            (
                2u32,
                "CREATE TABLE b (id INTEGER);\nCREATE TABLE b (id INTEGER);",
            ),
        ];
        // 错误要指明失败的是哪一步：启动弹窗只显示这一行
        let err = apply(&mut conn, bad).unwrap_err();
        assert!(
            err.to_string().contains("v2"),
            "错误应带上失败的版本号: {err}"
        );
        assert_eq!(version(&conn), 1);
        let exists: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='b'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(exists, 0, "失败的迁移步骤不该留下半张表");
    }

    #[test]
    fn refuses_database_newer_than_program() {
        let (_dir, path) = temp_path("newer.db");
        let mut conn = Connection::open(&path).unwrap();
        apply(
            &mut conn,
            &[
                (1u32, "CREATE TABLE a (id INTEGER);"),
                (2u32, "CREATE TABLE b (id INTEGER);"),
            ],
        )
        .unwrap();
        // 程序只剩到 v1：库是 v2，应拒绝而不是降级写
        let err = apply(&mut conn, &[(1u32, "CREATE TABLE a (id INTEGER);")]).unwrap_err();
        assert!(err.to_string().contains("高于程序支持"), "{err}");
        assert_eq!(version(&conn), 2, "拒开时不能改动库");
    }

    #[test]
    fn rejects_unordered_migration_list() {
        let (_dir, path) = temp_path("unordered.db");
        let mut conn = Connection::open(&path).unwrap();
        let err = apply(
            &mut conn,
            &[
                (2u32, "CREATE TABLE b (id INTEGER);"),
                (1u32, "CREATE TABLE a (id INTEGER);"),
            ],
        )
        .unwrap_err();
        assert!(err.to_string().contains("严格递增"), "{err}");
    }

    /// 跳号与「不从 1 开始」是同一类事故：中间那步被删了，新库会跳过它直接跑下一步，
    /// 与老库从此结构不同。必须在启动时炸掉，而不是让某个用户遇到缺列时才发现。
    #[test]
    fn rejects_gapped_or_shifted_migration_numbers() {
        let (_dir, path) = temp_path("gapped.db");
        let mut conn = Connection::open(&path).unwrap();

        // 1 之后直接跳 3
        let err = apply(
            &mut conn,
            &[
                (1u32, "CREATE TABLE a (id INTEGER);"),
                (3u32, "CREATE TABLE c (id INTEGER);"),
            ],
        )
        .unwrap_err();
        assert!(err.to_string().contains("连续"), "{err}");

        // 第一步就不是 1
        let err = apply(&mut conn, &[(2u32, "CREATE TABLE b (id INTEGER);")]).unwrap_err();
        assert!(err.to_string().contains("从 1 开始"), "{err}");

        // 列表写错时库不能被碰过：版本仍为 0，一张表都没建
        assert_eq!(version(&conn), 0);
        let tables: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 0, "校验失败必须在执行任何 DDL 之前");
    }

    /// 真实迁移从空库跑到最新：表与索引齐全、唯一约束生效、重复跑幂等。
    ///
    /// 这是 schema 的「快照」用例：以后新增 0002 时要顺手把期望的表/索引更新进来，
    /// 改错了立刻能看到，而不是等某个用户的库缺了列才发现。
    #[test]
    fn real_migrations_create_the_v1_schema() {
        use rusqlite::params;

        let (_dir, path) = temp_path("real.db");
        let mut conn = Connection::open(&path).unwrap();
        let latest = super::MIGRATIONS.last().unwrap().0;
        assert_eq!(apply(&mut conn, super::MIGRATIONS).unwrap(), latest);
        assert_eq!(version(&conn), latest);

        let names = |kind: &str| -> Vec<String> {
            conn.prepare(&format!(
                "SELECT name FROM sqlite_master
                 WHERE type = '{kind}' AND name NOT LIKE 'sqlite_%' ORDER BY name"
            ))
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap()
        };
        assert_eq!(names("table"), vec!["message", "session", "workspace"]);
        assert_eq!(
            names("index"),
            vec![
                "ix_message_session",
                "ix_session_list",
                "ix_workspace_recent",
                "ux_workspace_path"
            ]
        );

        // 唯一索引真的拦得住：同一个 path 插两次，第二次报错
        let add = |p: &str| -> rusqlite::Result<usize> {
            conn.execute(
                "INSERT INTO workspace (id, path, created_at, updated_at, last_opened_at)
                 VALUES (?1, ?2, 0, 0, 0)",
                params![p, p],
            )
        };
        add("C:/work/a").unwrap();
        assert!(add("C:/work/a").is_err(), "同一 path 不能出现两行");
        add("C:/work/b").unwrap();

        // 刻意不用外键：指向不存在 workspace 的 session 照样插得进去。
        // 这条断言是**决策守卫**——将来有人顺手加上 `REFERENCES`，这里会先红，
        // 逼他读一眼 `0001_init.sql` 头注释里「一致性由业务事务保证」那两条要求。
        assert!(
            conn.execute(
                "INSERT INTO session (id, workspace_id, created_at, updated_at)
                 VALUES ('s-orphan', 'no-such-workspace', 0, 0)",
                [],
            )
            .is_ok(),
            "schema 不设外键，孤儿行必须靠业务事务挡住"
        );

        // 幂等：再跑一次不重复执行，版本与数据都不变
        assert_eq!(apply(&mut conn, super::MIGRATIONS).unwrap(), latest);
        let rows: i64 = conn
            .query_row("SELECT count(*) FROM workspace", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 2);
        let sessions: i64 = conn
            .query_row("SELECT count(*) FROM session", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sessions, 1, "重跑迁移不应影响已有行");
    }

    /// `seq` 是消息的唯一句柄，删掉的号**不能**回收：`session.summary_upto_seq` 依赖它只增不减。
    /// 换成不带 AUTOINCREMENT 的 `INTEGER PRIMARY KEY`，这里会退回到 2。
    #[test]
    fn message_seq_is_never_reused() {
        let (_dir, path) = temp_path("seq.db");
        let mut conn = Connection::open(&path).unwrap();
        apply(&mut conn, super::MIGRATIONS).unwrap();

        let add = || -> i64 {
            conn.execute(
                "INSERT INTO message (session_id, role, content, created_at)
                 VALUES ('s1', 'user', 'x', 0)",
                [],
            )
            .unwrap();
            conn.last_insert_rowid()
        };
        assert_eq!(add(), 1);
        assert_eq!(add(), 2);

        conn.execute("DELETE FROM message WHERE seq = 2", [])
            .unwrap();
        assert_eq!(add(), 3, "删掉的号不能回收");
        assert_eq!(add(), 4);
    }

    /// 决策守卫：schema 里没有外键，删父行**不会**带走子行。
    /// 将来有人顺手加 `REFERENCES ... ON DELETE CASCADE`，这里会先红。
    #[test]
    fn deleting_a_parent_does_not_cascade() {
        let (_dir, path) = temp_path("cascade.db");
        let mut conn = Connection::open(&path).unwrap();
        apply(&mut conn, super::MIGRATIONS).unwrap();

        conn.execute_batch(
            "INSERT INTO workspace (id, path, created_at, updated_at, last_opened_at)
               VALUES ('w1', 'C:/w', 0, 0, 0);
             INSERT INTO session (id, workspace_id, created_at, updated_at)
               VALUES ('s1', 'w1', 0, 0);
             INSERT INTO message (session_id, role, content, created_at)
               VALUES ('s1', 'user', 'x', 0);",
        )
        .unwrap();

        conn.execute("DELETE FROM workspace WHERE id = 'w1'", [])
            .unwrap();
        let count = |table: &str| -> i64 {
            conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(
            count("session"),
            1,
            "删工作区不会带走会话，必须业务事务按序删"
        );
        assert_eq!(count("message"), 1, "也不会带走消息");
    }
}
