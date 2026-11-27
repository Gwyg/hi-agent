//! 工作区用例：启动时决定用哪个目录，运行时切换。
//!
//! 两条线各管一件事，不要混：
//! - **真值**在 SQLite `workspace` 表（`path` 唯一）：有哪些工作区、叫什么名字、最近打开的顺序；
//! - **上次停在哪**在 `app.toml [resume]`：只是个指针，可丢失、可手删，删了按「最近打开」重推。
//!
//! 名字的策略只有一条：**入库那一刻取目录名，之后不再自动跟随**。
//! 否则用户改过的名字会被目录重命名悄悄覆盖，而「用没改过」这件事无从判断。
//!
//! 边界根（工具层能读写的范围）由本模块算好后注入 `path_scope`：`ai/` 在更下层，
//! 不能反过来问 `services/` 要目录。

use anyhow::{anyhow, Context, Result};
use std::path::Path;
use std::sync::RwLock;
use tracing::warn;

use crate::config::{path_scope, resume};
use crate::store::workspace as repo;
use crate::store::Store;
use crate::util::{app_dir, canonical_path, dir_name, resolve_lexical, strip_verbatim};

pub use crate::store::workspace::Workspace;

/// 当前工作区。启动时由 [`init`] 写入，之后只在 [`switch`] 里改。
///
/// 与 `path_scope` 里的根是同一件事的两种表示：那边是给工具层看的边界根，
/// 这边是给界面看的 id / 名字。两者只在 [`activate`] 一处同时更新，不会分叉。
static CURRENT: RwLock<Option<Workspace>> = RwLock::new(None);

/// 当前工作区。启动完成前是 `None`。
pub fn current() -> Option<Workspace> {
    CURRENT.read().ok()?.clone()
}

/// 启动：确定本次用哪个目录 → 注入边界根 → 补写断点。
///
/// 候选按可靠性降序，任一步失手就退下一步，**只有默认目录建不出来才拦启动**：
/// 1. `[resume].workspace`（上次停的地方）；
/// 2. 库里最近打开的那一个（断点被删 / 被手改坏时的自愈路径）；
/// 3. `~/.hi-agent/workspace`（兜底，不存在就建）。
///
/// 前两步失败都只是「那个目录没了」，不该让程序打不开——用户还能自己选一个。
pub fn init(store: &Store) -> Result<()> {
    let resumed = resume::get().workspace;
    let ws = startup(store, resumed.as_deref())?;
    activate(&ws)?;

    // 断点只在「和实际不一致」时补写：值本来就对的时候写一遍只是白改一次文件，
    // 而且会把用户手写的注释排版来回重排。
    if resumed.as_deref() != Some(ws.path.as_str()) {
        if let Err(e) = resume::set_workspace(&ws.path) {
            warn!("断点写回失败（本次已生效，下次启动按最近打开推断）: {e}");
        }
    }
    Ok(())
}

/// 切换工作区：定位 → 入库/取回 → 注入 → 写断点。
///
/// 与 [`init`] 的区别是**错误必须报出去**：用户点了一个目录，说不清就是失败。
/// 断点写回则是例外——切换已经在界面上生效了，为一个配置文件写不进去而回滚，
/// 用户只会看到「点了没反应」。
pub fn switch(store: &Store, raw: &str) -> Result<Workspace> {
    let path = existing_dir(raw)?;
    let ws = open_or_create(store, &path)?;
    activate(&ws)?;
    if let Err(e) = resume::set_workspace(&ws.path) {
        warn!("切换已生效，但断点写回失败（下次启动按最近打开推断）: {e}");
    }
    Ok(ws)
}

/// 列出工作区，最近打开的在前，供界面分批（懒加载）取。
///
/// `limit` 在这里夹紧：前端传的只是「想拉多少」，不是可以要十万行的接口。
/// 目录可能已经被删或盘没挂载，照样列出来——用户可能只是暂时没插盘；
/// 能不能用由选中时的 [`switch`] 回答，不该在列表阶段替用户判断。
pub fn list(store: &Store, offset: u32, limit: u32) -> Result<Vec<Workspace>> {
    let limit = limit.clamp(1, 50) as i64;
    store.tx(|tx| repo::list(tx, limit, offset as i64))
}

// ---- 内部 ----

/// 按可靠性依次尝试三个候选，见 [`init`]。
fn startup(store: &Store, resumed: Option<&str>) -> Result<Workspace> {
    if let Some(raw) = resumed {
        match existing_dir(raw) {
            Ok(path) => return open_or_create(store, &path),
            // 目录被删 / 盘没挂 / 手改成相对路径：都不是致命错误，换下一个候选
            Err(e) => warn!("断点里的工作目录不可用，改按最近打开推断: {e}"),
        }
    }

    if let Some(ws) = store.tx(repo::most_recent)? {
        if Path::new(&ws.path).is_dir() {
            return open_or_create(store, &ws.path);
        }
        // 目录不在了但行还留着：跳过它，不删——用户可能只是没插移动硬盘。
        // 真删是「管理多个工作区」那个界面的动作，不该由启动流程替用户决定。
        warn!(path = %ws.path, "最近打开的工作目录已不存在，改用默认目录");
    }

    open_or_create(store, &default_dir()?)
}

/// 取回或新建一行，两种情况都算「刚打开过」。
///
/// 一个事务：查到就 touch，查不到就插。仓储层没有 upsert，
/// 因为「先查再决定」这一步本身就是业务语义（同名路径是同一个工作区，不是两个）。
fn open_or_create(store: &Store, path: &str) -> Result<Workspace> {
    store.tx(|tx| match repo::find_by_path(tx, path)? {
        Some(ws) => {
            repo::touch(tx, &ws.id)?;
            Ok(ws)
        }
        None => repo::insert(tx, path, &dir_name(path)),
    })
}

/// 用户给的路径 → 可直接使用的绝对目录。
///
/// 不存在 / 不是目录一律报错：工作区是「已经在那儿的目录」，不是一个待创建的名字。
/// （`canonical_path` 对不存在的路径也能给出落点，所以这里必须自己再判一次 `is_dir`。）
fn existing_dir(raw: &str) -> Result<String> {
    let resolved = resolve_lexical(raw, None)
        .with_context(|| format!("无法定位工作目录: {raw}（需要绝对路径或 `~/` 开头）"))?;
    let real = canonical_path(&resolved).with_context(|| format!("无法定位工作目录: {raw}"))?;
    let real = strip_verbatim(&real);
    if !Path::new(&real).is_dir() {
        return Err(anyhow!("不是目录: {real}"));
    }
    Ok(real)
}

/// 兜底目录：`~/.hi-agent/workspace`，不存在就建。
fn default_dir() -> Result<String> {
    let dir = app_dir().join("workspace");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("创建默认工作目录失败: {}", dir.display()))?;
    let real = canonical_path(&dir.display().to_string())?;
    Ok(strip_verbatim(&real))
}

/// 同时更新「工具层的边界根」和「界面看到的当前工作区」。
///
/// 顺序固定：先边界根。反过来的话，工具层会在一小段时间里拿着旧根去判定路径。
fn activate(ws: &Workspace) -> Result<()> {
    path_scope::set_root(&ws.path)?;
    let mut guard = CURRENT
        .write()
        .map_err(|e| anyhow!("工作区状态锁中毒: {e}"))?;
    *guard = Some(ws.clone());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 判定目录可用性：相对路径、不存在的路径、文件都要拒绝；真实目录要通过。
    ///
    /// 相对路径必须报错而不是猜基准——猜的话会被解析到 exe 所在目录（Tauri 打包后是安装目录）。
    #[test]
    fn existing_dir_rejects_unusable_inputs() {
        assert!(existing_dir("a/b").is_err(), "相对路径应报错");

        let missing = std::env::temp_dir().join("__hi_agent_no_such_dir__");
        assert!(
            existing_dir(&missing.display().to_string()).is_err(),
            "不存在的目录应报错"
        );

        let tmp = std::env::temp_dir();
        let real = existing_dir(&tmp.display().to_string()).expect("临时目录可用");
        assert!(Path::new(&real).is_dir());
        assert!(!real.starts_with(r"\\?\"), "落库的路径不应带扩展前缀");
    }

    /// 启动推导：断点里的目录可用时就用它，名字取目录名，同一路径不会建出第二行。
    ///
    /// 这里测的是纯决策（只碰临时库与临时目录），不碰 `app.toml`——
    /// 断点写回在 `init` 里，那一步不该由单测触发。
    #[test]
    fn startup_uses_resumed_dir_and_names_it_after_folder() {
        let dir = std::env::temp_dir().join(format!("hi-agent-ws-start-{}", crate::util::new_id()));
        std::fs::create_dir_all(&dir).expect("建临时工作区");
        let db =
            std::env::temp_dir().join(format!("hi-agent-ws-start-{}.db", crate::util::new_id()));
        let store = Store::open_at(&db).expect("打开临时库");

        let raw = dir.display().to_string();
        let ws = startup(&store, Some(&raw)).expect("断点有效时应直接用它");
        assert_eq!(
            ws.path,
            strip_verbatim(&dir.canonicalize().expect("cwd 可用").display().to_string()),
            "入库的应是统一后的绝对路径"
        );
        assert_eq!(
            ws.name,
            dir.file_name().expect("临时目录有名字").to_string_lossy(),
            "默认名取目录名"
        );

        let again = startup(&store, Some(&raw)).expect("再次确定");
        assert_eq!(again.id, ws.id, "同一路径应是同一行，不是新建");

        drop(store);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", db.display()));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
