//! 会话仓储：一行 = 一段对话。
//!
//! 本模块只回答「这一行怎么读写」，不做任何判断：标题取什么、什么时候新建、
//! 保留几条、要不要压缩——都是 `services` 与 `ai/` 的事。
//!
//! `summary` 与 `summary_upto_seq` 一起读写，永远不单独改：水位没了摘要就不知道从哪续，
//! 摘要在、水位不对则会把已压缩的消息重新装回上下文。所以只给一个 `set_summary`。
//!
//! 不暴露 `created_at` / `updated_at`：排序在 SQL 里做，调用方不需要拿它们做判断。
//! 哪天侧边栏要按「今天 / 昨天」分组，再加一列，别现在预留着没人用。

use anyhow::{bail, Result};
use rusqlite::{params, Row, Transaction};

use super::{execute, query_all, query_one};
use crate::util::{new_id, now_ms};

/// 一行会话。
#[derive(Clone, Debug)]
pub struct Session {
    pub id: String,
    pub workspace_id: String,
    /// 空串 = 还没起标题（正常流程不会出现：建行时就写了首条输入的截断）。
    pub title: String,
    /// 最新摘要，空串 = 从未压缩。
    pub summary: String,
    /// 本会话 `seq` 小于等于此值的消息已被摘要覆盖。
    pub summary_upto_seq: i64,
}

const COLUMNS: &str = "id, workspace_id, title, summary, summary_upto_seq";

fn to_session(row: &Row<'_>) -> Result<Session> {
    Ok(Session {
        id: row.get("id")?,
        workspace_id: row.get("workspace_id")?,
        title: row.get("title")?,
        summary: row.get("summary")?,
        summary_upto_seq: row.get("summary_upto_seq")?,
    })
}

/// 新建一行。`id` 由仓储生成（UUID v7）。
///
/// 与首条消息的插入放在**同一个事务**里：否则崩在两者之间会留下一个没有任何消息的
/// 空会话，占着「每工作区 5 条」的名额，列表里还显示成一条空的。
pub fn insert(tx: &Transaction<'_>, workspace_id: &str, title: &str) -> Result<Session> {
    let id = new_id();
    let now = now_ms();
    execute(
        tx,
        "INSERT INTO session
           (id, workspace_id, title, summary, summary_upto_seq, created_at, updated_at)
         VALUES (?1, ?2, ?3, '', 0, ?4, ?4)",
        params![id, workspace_id, title, now],
    )?;
    Ok(Session {
        id,
        workspace_id: workspace_id.to_string(),
        title: title.to_string(),
        summary: String::new(),
        summary_upto_seq: 0,
    })
}

pub fn load(tx: &Transaction<'_>, id: &str) -> Result<Option<Session>> {
    query_one(
        tx,
        &format!("SELECT {COLUMNS} FROM session WHERE id = ?1"),
        params![id],
        to_session,
    )
}

/// 某工作区下的会话，最近的在前。`limit` 由调用方给（业务上是「每工作区保留 5 条」）。
///
/// 第二排序键 `id` 让顺序确定下来：同一毫秒更新的两行 `updated_at` 相同，
/// 只按时间排时 SQLite 不保证稳定顺序，列表会在两次查询间跳动。
pub fn list_by_workspace(
    tx: &Transaction<'_>,
    workspace_id: &str,
    limit: i64,
) -> Result<Vec<Session>> {
    query_all(
        tx,
        &format!(
            "SELECT {COLUMNS} FROM session WHERE workspace_id = ?1 \
             ORDER BY updated_at DESC, id DESC LIMIT ?2"
        ),
        params![workspace_id, limit],
        to_session,
    )
}

/// 记一次「有新消息」。只动 `updated_at`——列表顺序跟着真实写入走。
pub fn touch(tx: &Transaction<'_>, id: &str) -> Result<()> {
    execute(
        tx,
        "UPDATE session SET updated_at = ?1 WHERE id = ?2",
        params![now_ms(), id],
    )?;
    Ok(())
}

/// 用户改名。**不动 `updated_at`**。
///
/// 那一列表示「最后一次有消息的时间」，列表按它倒序排。改名是元数据编辑：
/// 若也把行顶到最前，用户给一条旧对话起个名字，它会突然跳到第一行——改名前它
/// 在哪，改完还在哪，才符合预期。（工作区那边没这个问题：排序看 `last_opened_at`，
/// `updated_at` 只是元数据时间，改名动它不影响顺序。）
///
/// 标题长度与空值由用例层判（那是展示口径），这里原样写入。
pub fn rename(tx: &Transaction<'_>, id: &str, title: &str) -> Result<()> {
    execute(
        tx,
        "UPDATE session SET title = ?1 WHERE id = ?2",
        params![title, id],
    )?;
    Ok(())
}

/// 删除一个会话，**连带它的消息**。
///
/// 与 `trim_by_workspace` 同一个理由：schema 没有外键，`message.session_id` 只是约定，
/// 漏删就留下一批永远读不到的消息（占空间，也让「每会话条数」的判断失真）。
///
/// 不判断该不该删：是不是当前会话、断点要不要清，都是用例层的事。
pub fn delete(tx: &Transaction<'_>, id: &str) -> Result<()> {
    execute(tx, "DELETE FROM message WHERE session_id = ?1", params![id])?;
    execute(tx, "DELETE FROM session WHERE id = ?1", params![id])?;
    Ok(())
}

/// 回写压缩结果。**摘要与水位必须同一条语句**，否则崩在中间会出现
/// 「有摘要但水位是旧的」——恢复时已压缩的消息会重新进上下文。
///
/// 不动 `updated_at`：压缩发生在对话中途，那列表示「最后一条消息的时间」，
/// 两者混在一起会让列表顺序出现用户看不懂的跳动。
pub fn set_summary(tx: &Transaction<'_>, id: &str, summary: &str, upto_seq: i64) -> Result<()> {
    execute(
        tx,
        "UPDATE session SET summary = ?1, summary_upto_seq = ?2 WHERE id = ?3",
        params![summary, upto_seq, id],
    )?;
    Ok(())
}

/// 清掉该工作区里最旧的会话（连带其消息），只保留最近 `keep` 个，返回删除的会话数。
///
/// `keep` 是**最终保留的总数**：被保护的当前会话占其中一个名额，不会因为「它在用」
/// 就多挤掉一个别人 —— 上限得是用户看得见的那个数。
///
/// `protect` 是当前会话的 id：它可能恰好排在最旧（用户开着一条很久没说话的对话），
/// 但正在用，不能被维护任务删掉。
///
/// 删父行必须**连带**删子行：schema 没有外键，`message.session_id` 只是约定，
/// 漏删就留下一批永远读不到的消息（占着空间，还会让「每会话条数」的判断失真）。
pub fn trim_by_workspace(
    tx: &Transaction<'_>,
    workspace_id: &str,
    keep: i64,
    protect: Option<&str>,
) -> Result<usize> {
    if keep < 0 {
        bail!("keep 不能为负，收到 {keep}");
    }
    // 候选里已排除被保护的那个，所以「保留几个候选」要少算它一个。
    // `LIMIT -1 OFFSET n` = 跳过最近的 n 个、取剩下的全部（SQLite 里 -1 表示不限）。
    let offset = match protect {
        Some(_) => (keep - 1).max(0),
        None => keep,
    };
    let victims: Vec<String> = query_all(
        tx,
        "SELECT id FROM session WHERE workspace_id = ?1 AND (?2 IS NULL OR id <> ?2) \
         ORDER BY updated_at DESC, id DESC LIMIT -1 OFFSET ?3",
        params![workspace_id, protect, offset],
        |row| Ok(row.get("id")?),
    )?;

    for id in &victims {
        execute(tx, "DELETE FROM message WHERE session_id = ?1", params![id])?;
        execute(tx, "DELETE FROM session WHERE id = ?1", params![id])?;
    }
    Ok(victims.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::workspace;
    use crate::store::Store;

    struct TempStore {
        store: Store,
        path: std::path::PathBuf,
    }

    impl TempStore {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!("hi-agent-sess-{tag}-{}.db", new_id()));
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

    /// 先建一个工作区，返回它的 id（会话必须挂在某个工作区下）
    fn seed_workspace(store: &Store, path: &str) -> String {
        store
            .tx(|tx| workspace::insert(tx, path, "demo"))
            .expect("插入工作区")
            .id
    }

    #[test]
    fn insert_then_load() {
        let tmp = TempStore::new("load");
        let ws = seed_workspace(&tmp.store, "/proj/a");
        let s = tmp
            .store
            .tx(|tx| insert(tx, &ws, "改一下这个 bug"))
            .expect("插入会话");
        assert_eq!(s.summary, "");
        assert_eq!(s.summary_upto_seq, 0);

        let found = tmp
            .store
            .tx(|tx| load(tx, &s.id))
            .expect("查询")
            .expect("存在");
        assert_eq!(found.title, "改一下这个 bug");
        assert_eq!(found.workspace_id, ws);

        assert!(
            tmp.store.tx(|tx| load(tx, "nope")).expect("查询").is_none(),
            "不存在的 id 返回 None"
        );
    }

    /// 列表按 `updated_at` 倒序、只列本工作区、`touch` 能顶上来
    #[test]
    fn list_is_per_workspace_and_follows_touch() {
        let tmp = TempStore::new("list");
        let ws_a = seed_workspace(&tmp.store, "/proj/a");
        let ws_b = seed_workspace(&tmp.store, "/proj/b");

        let a1 = tmp.store.tx(|tx| insert(tx, &ws_a, "a1")).expect("a1");
        std::thread::sleep(std::time::Duration::from_millis(2));
        tmp.store.tx(|tx| insert(tx, &ws_a, "a2")).expect("a2");
        std::thread::sleep(std::time::Duration::from_millis(2));
        tmp.store.tx(|tx| insert(tx, &ws_b, "b1")).expect("b1");

        let list = tmp
            .store
            .tx(|tx| list_by_workspace(tx, &ws_a, 5))
            .expect("列表");
        assert_eq!(
            list.iter().map(|s| s.title.as_str()).collect::<Vec<_>>(),
            ["a2", "a1"],
            "最近的在前，且不混入别的工作区"
        );

        std::thread::sleep(std::time::Duration::from_millis(2));
        tmp.store.tx(|tx| touch(tx, &a1.id)).expect("touch a1");
        let list = tmp
            .store
            .tx(|tx| list_by_workspace(tx, &ws_a, 5))
            .expect("列表");
        assert_eq!(list[0].id, a1.id, "touch 过的顶到最前");
    }

    /// 摘要与水位必须一起变；`rename` 改标题
    #[test]
    fn set_summary_and_rename() {
        let tmp = TempStore::new("misc");
        let ws = seed_workspace(&tmp.store, "/proj/a");
        let s = tmp.store.tx(|tx| insert(tx, &ws, "初稿")).expect("插入");

        tmp.store
            .tx(|tx| set_summary(tx, &s.id, "之前聊了环境配置", 42))
            .expect("回写摘要");
        let found = tmp
            .store
            .tx(|tx| load(tx, &s.id))
            .expect("查询")
            .expect("存在");
        assert_eq!(found.summary, "之前聊了环境配置");
        assert_eq!(found.summary_upto_seq, 42);

        tmp.store
            .tx(|tx| rename(tx, &s.id, "环境配置"))
            .expect("改名");
        let found = tmp
            .store
            .tx(|tx| load(tx, &s.id))
            .expect("查询")
            .expect("存在");
        assert_eq!(found.title, "环境配置");
    }

    /// 删除会话必须连带删它的消息，且不波及同工作区的其他会话。
    #[test]
    fn delete_removes_session_and_its_messages_only() {
        use crate::ai::llm::types::Msg;
        use crate::store::message;

        let tmp = TempStore::new("del");
        let ws = seed_workspace(&tmp.store, "/proj/a");
        let doomed = tmp.store.tx(|tx| insert(tx, &ws, "要删的")).expect("插入");
        let kept = tmp.store.tx(|tx| insert(tx, &ws, "留着的")).expect("插入");

        for id in [&doomed.id, &kept.id] {
            tmp.store
                .tx(|tx| message::insert(tx, id, &Msg::user("hi")))
                .expect("插入消息");
            tmp.store
                .tx(|tx| message::insert(tx, id, &Msg::assistant("hello")))
                .expect("插入消息");
        }

        tmp.store.tx(|tx| delete(tx, &doomed.id)).expect("删除会话");

        assert!(
            tmp.store
                .tx(|tx| load(tx, &doomed.id))
                .expect("查询")
                .is_none(),
            "会话行该没了"
        );
        assert_eq!(
            count_messages(&tmp.store, &doomed.id),
            0,
            "删会话必须连带删它的消息"
        );
        assert_eq!(
            count_messages(&tmp.store, &kept.id),
            2,
            "别的会话的消息不能被牵连"
        );
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

    /// 超出上限时删最旧的会话，连带其消息；`protect` 的那个永不删；不跨工作区。
    #[test]
    fn trim_by_workspace_keeps_newest_and_protects_current() {
        use crate::ai::llm::types::Msg;
        use crate::store::message;

        let tmp = TempStore::new("trim");
        let ws_a = seed_workspace(&tmp.store, "/proj/a");
        let ws_b = seed_workspace(&tmp.store, "/proj/b");

        let mut ids = Vec::new();
        for i in 0..4 {
            let s = tmp
                .store
                .tx(|tx| insert(tx, &ws_a, &format!("s{i}")))
                .expect("插入会话");
            // 每个会话各落一条消息，用来验证「删会话连带删消息」
            tmp.store
                .tx(|tx| message::insert(tx, &s.id, &Msg::user("hi")))
                .expect("插入消息");
            ids.push(s.id);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        // 别的会话不该被这个工作区的清理波及
        let other = tmp
            .store
            .tx(|tx| insert(tx, &ws_b, "b0"))
            .expect("插入会话");

        // 保留 2 个、保护第 1 个（它恰好是最旧的）：删 s1、s2
        let removed = tmp
            .store
            .tx(|tx| trim_by_workspace(tx, &ws_a, 2, Some(&ids[0])))
            .expect("清理");
        assert_eq!(removed, 2);

        let left = tmp
            .store
            .tx(|tx| list_by_workspace(tx, &ws_a, 10))
            .expect("列表");
        assert_eq!(
            left.iter().map(|s| s.title.as_str()).collect::<Vec<_>>(),
            ["s3", "s0"],
            "剩最新的 s3 与被保护的 s0"
        );
        assert!(
            tmp.store
                .tx(|tx| load(tx, &ids[1]))
                .expect("查询")
                .is_none(),
            "被删的会话行不该还在"
        );

        let orphan: i64 = tmp
            .store
            .tx(|tx| {
                query_one(
                    tx,
                    "SELECT count(*) AS n FROM message WHERE session_id = ?1",
                    params![ids[1]],
                    |row| Ok(row.get("n")?),
                )
            })
            .expect("查询")
            .expect("count 有值");
        assert_eq!(orphan, 0, "删会话必须连带删它的消息");

        assert!(
            tmp.store
                .tx(|tx| load(tx, &other.id))
                .expect("查询")
                .is_some(),
            "别的工作区不受影响"
        );
    }
}
