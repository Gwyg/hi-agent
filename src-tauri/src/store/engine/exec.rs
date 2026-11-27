//! 执行原语：三个函数，没有自有类型。
//!
//! 事务对象、绑定值、行都直接用 `rusqlite` 的（`Transaction` / `params!` / `Row`）。
//! 本模块只做两件真正重复的事：
//! 1. 把「prepare → query → 逐行 next」这段样板收成一个调用；
//! 2. 给每一步的错误套上 SQL 原文（`anyhow` 上下文），仓储只管 `?`。
//!
//! 为什么不自己包一层 `Row`：包几个取值方法，就等于把仓储能取的类型限制成几个
//! （`text` / `int` / `_opt` 四种）；而 `rusqlite` 的 `get::<_, T>` 覆盖 bool / f64 /
//! u32 / BLOB 全都免费，类型还能由目标字段反推——`id: row.get(0)?` 不必写 turbofish。
//! 出错信息也够用：`InvalidColumnType(列号, 期望类型, 实际类型)`。
//!
//! 三个函数的第一个参数都是事务对象：
//! - [`execute`]：除 SELECT 以外的语句，返回受影响行数；
//! - [`query_one`]：查至多一行，`map` 把行转成领域类型；
//! - [`query_all`]：查多行。
//!
//! 事务的开与合不在这里——那是 `Conn::tx` 的事。本模块不认识连接，只在已有事务上跑语句。

use anyhow::{Context, Result};
use rusqlite::{Params, Row, Transaction};

/// 执行一条写语句，返回受影响行数。
///
/// SQL 用 `?1` / `?2` 占位，值用 `params![…]` 按顺序绑定（**永不拼接字符串**，
/// 这是防注入的唯一手段，也是「值里有引号不会炸」的唯一保证）。
///
/// 拿不到 `RETURNING` 的结果：要自增主键一律用 [`query_one`] 跑
/// `INSERT … RETURNING seq`，不要 `execute` 之后读 `last_insert_rowid()`
/// ——那个值是**连接级**的，同一事务里插了第二条就对不上。
pub(in crate::store) fn execute(
    tx: &Transaction<'_>,
    sql: &str,
    params: impl Params,
) -> Result<usize> {
    tx.execute(sql, params)
        .with_context(|| format!("执行语句失败: {sql}"))
}

/// 查询多行，逐行交给 `map` 转换成调用方要的类型。
///
/// `map` 返回 `anyhow::Result`（而不是 rusqlite 的 Result），所以仓储里
/// 类型不符、字段缺失可以直接 `?` 抛出并带上自己的上下文。
pub(in crate::store) fn query_all<T>(
    tx: &Transaction<'_>,
    sql: &str,
    params: impl Params,
    mut map: impl FnMut(&Row<'_>) -> Result<T>,
) -> Result<Vec<T>> {
    let mut stmt = tx
        .prepare(sql)
        .with_context(|| format!("准备语句失败: {sql}"))?;
    let mut rows = stmt
        .query(params)
        .with_context(|| format!("查询失败: {sql}"))?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().with_context(|| format!("读取行失败: {sql}"))? {
        out.push(map(row)?);
    }
    Ok(out)
}

/// 查询至多一行。多于一行不报错（由调用方的 `LIMIT 1` 或聚合语义保证），
/// 这里只取第一行——不做「偷偷收集全部再判断」的额外开销。
pub(in crate::store) fn query_one<T>(
    tx: &Transaction<'_>,
    sql: &str,
    params: impl Params,
    mut map: impl FnMut(&Row<'_>) -> Result<T>,
) -> Result<Option<T>> {
    let mut stmt = tx
        .prepare(sql)
        .with_context(|| format!("准备语句失败: {sql}"))?;
    let mut rows = stmt
        .query(params)
        .with_context(|| format!("查询失败: {sql}"))?;
    match rows.next().with_context(|| format!("读取行失败: {sql}"))? {
        Some(row) => Ok(Some(map(row)?)),
        None => Ok(None),
    }
}
