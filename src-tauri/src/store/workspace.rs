//! 工作区仓储：一行 = 一个交给 AI 读写的目录。
//!
//! 本模块只回答「这一行怎么读写」，不做任何判断：目录存不存在、名字该叫什么、
//! 这次是不是「切换」——都是 `services::workspace` 的事。这里连 `Path` 都不碰，
//! 收进来的 `path` 已经是统一过的绝对路径（唯一索引建立在它上面）。
//!
//! 只暴露业务用得到的列。`created_at` / `updated_at` 是审计字段，`last_opened_at`
//! 只在 SQL 里参与排序，都不出现在返回值里——多带一列，调用方就会开始依赖它。

use anyhow::Result;
use rusqlite::{params, Row, Transaction};

use super::{execute, query_all, query_one};
use crate::util::{new_id, now_ms};

/// 一行工作区。
#[derive(Clone, Debug)]
pub struct Workspace {
    pub id: String,
    /// 已统一的绝对路径，同时是唯一键。
    pub path: String,
    /// 显示名。入库时取目录名，之后不再自动跟随目录改名（否则分不清用户改没改过）。
    pub name: String,
}

const COLUMNS: &str = "id, path, name";

/// 类型映射的唯一一处：列 → 字段。取值按列名而不是列序，
/// `SELECT` 里换顺序不会静默读串（列名对不上会当场报错）。
fn to_workspace(row: &Row<'_>) -> Result<Workspace> {
    Ok(Workspace {
        id: row.get("id")?,
        path: row.get("path")?,
        name: row.get("name")?,
    })
}

/// 按路径找。路径是唯一键，`ux_workspace_path` 保证最多一行。
pub fn find_by_path(tx: &Transaction<'_>, path: &str) -> Result<Option<Workspace>> {
    query_one(
        tx,
        &format!("SELECT {COLUMNS} FROM workspace WHERE path = ?1"),
        params![path],
        to_workspace,
    )
}

/// 按 id 取一行。改名与删除都要先确认存在，才谈得上「改成什么」「删了哪个」。
pub fn load(tx: &Transaction<'_>, id: &str) -> Result<Option<Workspace>> {
    query_one(
        tx,
        &format!("SELECT {COLUMNS} FROM workspace WHERE id = ?1"),
        params![id],
        to_workspace,
    )
}

/// 插入一行并返回它。`id` 由仓储生成（UUID v7，按字符串排≈按时间排），
/// 三个时间戳都取当前时刻：新建即「刚打开过」。
pub fn insert(tx: &Transaction<'_>, path: &str, name: &str) -> Result<Workspace> {
    let id = new_id();
    let now = now_ms();
    execute(
        tx,
        "INSERT INTO workspace (id, path, name, created_at, updated_at, last_opened_at)
         VALUES (?1, ?2, ?3, ?4, ?4, ?4)",
        params![id, path, name, now],
    )?;
    Ok(Workspace {
        id,
        path: path.to_string(),
        name: name.to_string(),
    })
}

/// 记一次「打开」。只动 `last_opened_at`，不碰 `updated_at`——
/// 后者是元数据（改名）的时间，把两者混在一起会让「最近改名」和「最近打开」分不清。
pub fn touch(tx: &Transaction<'_>, id: &str) -> Result<()> {
    execute(
        tx,
        "UPDATE workspace SET last_opened_at = ?1 WHERE id = ?2",
        params![now_ms(), id],
    )?;
    Ok(())
}

/// 改名。只动 `name` 与 `updated_at`：`path` 是唯一键，改名不是「换目录」。
///
/// 与 `touch` 分开是刻意的：`last_opened_at` 决定「最近打开」的顺序，
/// `updated_at` 记元数据变更的时间，混在一起会让「刚改过名」被当成「刚打开过」。
pub fn rename(tx: &Transaction<'_>, id: &str, name: &str) -> Result<()> {
    execute(
        tx,
        "UPDATE workspace SET name = ?1, updated_at = ?2 WHERE id = ?3",
        params![name, now_ms(), id],
    )?;
    Ok(())
}

/// 删除一个工作区，**连带它的会话与消息**。返回删掉的会话数。
///
/// 只删库里的记录，**不碰目录本身**：那是用户的代码，不是我们的数据。
/// 用户在这里做的事是「不再把这条目录列在列表里」，不是「删掉这个项目」。
///
/// 三层都自己删：schema 没有外键，`session.workspace_id` 与 `message.session_id`
/// 都只是约定，跳一层就会留下一批读不到的孤儿行。
pub fn delete(tx: &Transaction<'_>, id: &str) -> Result<usize> {
    let sessions: i64 = query_one(
        tx,
        "SELECT count(*) AS n FROM session WHERE workspace_id = ?1",
        params![id],
        |row| Ok(row.get("n")?),
    )?
    .unwrap_or(0);

    execute(
        tx,
        "DELETE FROM message WHERE session_id IN \
           (SELECT id FROM session WHERE workspace_id = ?1)",
        params![id],
    )?;
    execute(
        tx,
        "DELETE FROM session WHERE workspace_id = ?1",
        params![id],
    )?;
    execute(tx, "DELETE FROM workspace WHERE id = ?1", params![id])?;
    Ok(sessions as usize)
}

/// 最近打开的那一个。只用于「断点丢了/失效了」时的推断，不参与启动主路径。
pub fn most_recent(tx: &Transaction<'_>) -> Result<Option<Workspace>> {
    query_one(
        tx,
        &format!("SELECT {COLUMNS} FROM workspace ORDER BY last_opened_at DESC LIMIT 1"),
        params![],
        to_workspace,
    )
}

/// 按最近打开时间倒序分页列出。`limit` / `offset` 由调用方算好，这里不夹紧、也不判
/// 「还有没有下一页」——那是分页策略，属于调用方。
///
/// 排序补上 `id` 作第二键：同一毫秒插入的两行 `last_opened_at` 相同，只按时间排时
/// SQLite 不保证稳定顺序，`OFFSET` 翻页就会重复或漏项（同一页里两次查询顺序可能不同）。
/// `id` 不参与「谁更新」的判断，只是让顺序确定下来。
pub fn list(tx: &Transaction<'_>, limit: i64, offset: i64) -> Result<Vec<Workspace>> {
    query_all(
        tx,
        &format!(
            "SELECT {COLUMNS} FROM workspace \
             ORDER BY last_opened_at DESC, id DESC LIMIT ?1 OFFSET ?2"
        ),
        params![limit, offset],
        to_workspace,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    /// 临时库：每个用例一个文件，用完删掉（含 WAL 的两个边文件）。
    struct TempStore {
        store: Store,
        path: std::path::PathBuf,
    }

    impl TempStore {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!("hi-agent-ws-{tag}-{}.db", new_id()));
            let store = Store::open_at(&path).expect("打开临时库");
            Self { store, path }
        }
    }

    impl Drop for TempStore {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", self.path.display()));
            }
        }
    }

    /// 插入后能按路径找回来；同一路径再插会撞唯一索引（由业务层先 find 再决定插不插）
    #[test]
    fn insert_then_find_by_path() {
        let tmp = TempStore::new("find");
        let ws = tmp
            .store
            .tx(|tx| insert(tx, "/proj/demo", "demo"))
            .expect("插入");
        assert_eq!(ws.name, "demo");

        let found = tmp
            .store
            .tx(|tx| find_by_path(tx, "/proj/demo"))
            .expect("查询")
            .expect("应当存在");
        assert_eq!(found.id, ws.id);
        assert_eq!(found.path, "/proj/demo");

        assert!(
            tmp.store
                .tx(|tx| find_by_path(tx, "/proj/other"))
                .expect("查询")
                .is_none(),
            "不存在的路径应返回 None"
        );
    }

    /// `most_recent` 按 `last_opened_at` 取，`touch` 能把某一行顶上来
    #[test]
    fn most_recent_follows_touch() {
        let tmp = TempStore::new("recent");
        let first = tmp
            .store
            .tx(|tx| insert(tx, "/proj/a", "a"))
            .expect("插入 a");
        let second = tmp
            .store
            .tx(|tx| insert(tx, "/proj/b", "b"))
            .expect("插入 b");

        // 两次插入可能落在同一毫秒，`touch` 的时间戳必须严格更大才能决定排序；
        // 等 2ms 是测试里唯一可靠的手段（不 sleep 就得把时间注进仓储签名，为测试改设计更糟）
        std::thread::sleep(std::time::Duration::from_millis(2));
        tmp.store.tx(|tx| touch(tx, &first.id)).expect("touch a");
        let recent = tmp
            .store
            .tx(most_recent)
            .expect("查询")
            .expect("应当有记录");
        assert_eq!(recent.id, first.id);

        std::thread::sleep(std::time::Duration::from_millis(2));
        tmp.store.tx(|tx| touch(tx, &second.id)).expect("touch b");
        let recent = tmp
            .store
            .tx(most_recent)
            .expect("查询")
            .expect("应当有记录");
        assert_eq!(recent.id, second.id);
    }

    /// 改名只动 name，path 不变（path 是唯一键，也是工具层的边界根）。
    #[test]
    fn rename_keeps_path() {
        let tmp = TempStore::new("rename");
        let ws = tmp
            .store
            .tx(|tx| insert(tx, "/proj/demo", "demo"))
            .expect("插入");

        tmp.store
            .tx(|tx| rename(tx, &ws.id, "我的项目"))
            .expect("改名");
        let found = tmp
            .store
            .tx(|tx| load(tx, &ws.id))
            .expect("查询")
            .expect("存在");
        assert_eq!(found.name, "我的项目");
        assert_eq!(found.path, "/proj/demo", "改名不该动路径");
        assert!(
            tmp.store
                .tx(|tx| find_by_path(tx, "/proj/demo"))
                .expect("查询")
                .is_some(),
            "按路径仍然找得到同一个工作区"
        );
    }

    /// 删工作区连带它的会话与消息，别的工作区一行不少。
    #[test]
    fn delete_cascades_to_sessions_and_messages() {
        use crate::ai::llm::types::Msg;
        use crate::store::{message, session};

        let tmp = TempStore::new("del");
        let doomed = tmp
            .store
            .tx(|tx| insert(tx, "/proj/a", "a"))
            .expect("插入 a");
        let kept = tmp
            .store
            .tx(|tx| insert(tx, "/proj/b", "b"))
            .expect("插入 b");

        let doomed_s = tmp
            .store
            .tx(|tx| session::insert(tx, &doomed.id, "sa"))
            .expect("插入会话");
        let kept_s = tmp
            .store
            .tx(|tx| session::insert(tx, &kept.id, "sb"))
            .expect("插入会话");
        for id in [&doomed_s.id, &kept_s.id] {
            tmp.store
                .tx(|tx| message::insert(tx, id, &Msg::user("hi")))
                .expect("插入消息");
        }

        let removed = tmp.store.tx(|tx| delete(tx, &doomed.id)).expect("删除");
        assert_eq!(removed, 1, "返回删掉的会话数");

        assert!(
            tmp.store
                .tx(|tx| load(tx, &doomed.id))
                .expect("查询")
                .is_none(),
            "工作区行该没了"
        );
        assert!(
            tmp.store
                .tx(|tx| session::load(tx, &doomed_s.id))
                .expect("查询")
                .is_none(),
            "它的会话也该没了"
        );
        assert_eq!(
            count_messages(&tmp.store, &doomed_s.id),
            0,
            "会话下的消息一起删"
        );

        assert!(
            tmp.store
                .tx(|tx| load(tx, &kept.id))
                .expect("查询")
                .is_some(),
            "别的工作区不受影响"
        );
        assert!(tmp
            .store
            .tx(|tx| session::load(tx, &kept_s.id))
            .expect("查询")
            .is_some());
        assert_eq!(count_messages(&tmp.store, &kept_s.id), 1);
    }

    fn count_messages(store: &Store, session_id: &str) -> i64 {
        store
            .tx(|tx| {
                query_one(
                    tx,
                    "SELECT count(*) AS n FROM message WHERE session_id = ?1",
                    params![session_id],
                    |row| Ok(row.get("n")?),
                )
            })
            .expect("查询")
            .expect("count 有值")
    }

    /// `list`：最近打开的在前，`offset` 能翻页，`touch` 过的会被顶到第一页
    #[test]
    fn list_orders_by_recent_and_paginates() {
        let tmp = TempStore::new("list");
        let a = tmp
            .store
            .tx(|tx| insert(tx, "/proj/a", "a"))
            .expect("插入 a");
        // 拉开毫秒：同一毫秒插入时 `last_opened_at` 相同，顺序由 id 决定，测不出「谁更新」
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = tmp
            .store
            .tx(|tx| insert(tx, "/proj/b", "b"))
            .expect("插入 b");
        std::thread::sleep(std::time::Duration::from_millis(2));
        let c = tmp
            .store
            .tx(|tx| insert(tx, "/proj/c", "c"))
            .expect("插入 c");

        let first_page = tmp.store.tx(|tx| list(tx, 2, 0)).expect("第一页");
        let names: Vec<_> = first_page.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, ["c", "b"], "最近打开的在前");

        let second_page = tmp.store.tx(|tx| list(tx, 2, 2)).expect("第二页");
        let names: Vec<_> = second_page.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, ["a"], "offset 跳过前两条");

        // 翻页不重叠：两次查询合起来正好是全部三行，没有重复也没有漏
        let mut ids: Vec<_> = first_page.iter().map(|w| w.id.clone()).collect();
        ids.extend(second_page.iter().map(|w| w.id.clone()));
        ids.sort();
        let mut expect = vec![a.id.clone(), b.id.clone(), c.id.clone()];
        expect.sort();
        assert_eq!(ids, expect);

        std::thread::sleep(std::time::Duration::from_millis(2));
        tmp.store.tx(|tx| touch(tx, &a.id)).expect("touch a");
        let first_page = tmp.store.tx(|tx| list(tx, 2, 0)).expect("重新取第一页");
        assert_eq!(first_page[0].id, a.id, "touch 过的顶到最前");
    }
}
