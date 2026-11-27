//! 消息仓储：一行 = 一条消息，`seq` 既是排序依据也是唯一句柄。
//!
//! 本模块是整个存储层里唯一认识 `Msg` 的地方 —— `Msg ↔ 行` 的映射写在这里，
//! 领域层因此完全不知道表、列、JSON 形状的存在。上层的 `ai/` 照旧只处理 `Msg`。
//!
//! # 形状
//!
//! - `content` 独立成列：三种角色都有正文，渲染与将来的 FTS 都要它；
//! - 角色专属字段进 `payload` JSON：assistant 存 `thinking` 与 `tool_calls`，
//!   tool 存 `call_id`，user 为 NULL；
//! - `system` **不落库**：它每轮由模板现渲染（工作目录换了就得重建），存下来只会过期。
//!
//! `arguments` 原样存协议原文，不 parse、不重排 —— 历史要原样回传服务端，
//! 转成 `Value` 再序列化会丢顺序、吞重复键、归一空白。

use anyhow::{bail, Context, Result};
use rusqlite::{params, Row, Transaction};
use serde::{Deserialize, Serialize};

use super::{execute, query_all, query_one};
use crate::ai::llm::types::{Content, Msg, ToolCall, UserMsg};
use crate::util::now_ms;

/// assistant 的 payload 形状。`skip_serializing_if` 让「没有思考也没有工具调用」时
/// 整个对象退化成 `{}`，再由 [`split`] 折成 NULL —— 纯文本回复不该在库里留一行 JSON。
#[derive(Default, Serialize, Deserialize)]
struct AssistantPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thinking: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<StoredCall>,
}

/// `ToolCall` 的磁盘形状。`Thinking.signature` 刻意不存（那是 Anthropic 的凭据）。
#[derive(Serialize, Deserialize)]
struct StoredCall {
    id: String,
    name: String,
    arguments: String,
}

impl From<&ToolCall> for StoredCall {
    fn from(c: &ToolCall) -> Self {
        Self {
            id: c.id.clone(),
            name: c.name.clone(),
            arguments: c.arguments.clone(),
        }
    }
}

impl From<StoredCall> for ToolCall {
    fn from(c: StoredCall) -> Self {
        Self {
            id: c.id,
            name: c.name,
            arguments: c.arguments,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct ToolPayload {
    call_id: String,
}

/// 落一条消息，返回它的 `seq`。
///
/// 用 `INSERT ... RETURNING seq` 而不是 `last_insert_rowid()`：后者是连接级的，
/// 同一事务里连着插两条时，第二次拿到的仍然是最后一条，但中间只要混入任何别的
/// 插入就会对不上。`seq` 是调用方唯一需要的返回值 —— 它是这条消息之后的句柄。
pub fn insert(tx: &Transaction<'_>, session_id: &str, msg: &Msg) -> Result<i64> {
    let (role, content, payload) = split(msg)?;
    query_one(
        tx,
        "INSERT INTO message (session_id, role, content, payload, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5) RETURNING seq",
        params![session_id, role, content, payload, now_ms()],
        |row| Ok(row.get("seq")?),
    )?
    .context("INSERT ... RETURNING seq 未返回行")
}

/// `Msg` → 三列。`system` 直接报错：它不落库，走到这里说明调用方搞错了。
fn split(msg: &Msg) -> Result<(&'static str, String, Option<String>)> {
    match msg {
        // 目前 `Content` 只有文本一个变体，拼起来不丢信息；将来加图片时这里必须改成
        // 结构化存储，否则图片会被静默丢掉 —— 那个 match 分支会强制我们回来改。
        Msg::User(u) => Ok(("user", user_text(u), None)),
        Msg::Assistant(a) => {
            let p = AssistantPayload {
                thinking: a.thinking.as_ref().map(|t| t.text.clone()),
                tool_calls: a.tool_calls.iter().map(StoredCall::from).collect(),
            };
            let payload = if p.thinking.is_none() && p.tool_calls.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&p).context("序列化 assistant payload 失败")?)
            };
            Ok(("assistant", a.content.clone(), payload))
        }
        Msg::Tool(t) => {
            let payload = serde_json::to_string(&ToolPayload {
                call_id: t.call_id.clone(),
            })
            .context("序列化 tool payload 失败")?;
            Ok(("tool", t.content.clone(), Some(payload)))
        }
        Msg::System(_) => bail!("system 消息不落库（每轮由模板现渲染）"),
    }
}

fn user_text(m: &UserMsg) -> String {
    m.content
        .iter()
        .map(|c| match c {
            Content::Text { text } => text.as_str(),
        })
        .collect::<Vec<_>>()
        .join("")
}

/// 一行消息 + 它的 `seq`。展示侧翻页要 seq，模型上下文不要。
pub struct SeqMsg {
    pub seq: i64,
    pub msg: Msg,
}

/// 一页可回看的消息。
pub struct Page {
    pub rows: Vec<SeqMsg>,
    /// 本页第一行之前还有没有更早的消息（前端据此决定「加载更早」按钮的显示）。
    pub has_more: bool,
}

/// 一行 → `SeqMsg`。
fn to_seq_msg(row: &Row<'_>) -> Result<SeqMsg> {
    Ok(SeqMsg {
        seq: row.get("seq")?,
        msg: to_msg(row)?,
    })
}

/// 一行 → `Msg`。
fn to_msg(row: &Row<'_>) -> Result<Msg> {
    let role: String = row.get("role")?;
    let content: String = row.get("content")?;
    let payload: Option<String> = row.get("payload")?;
    match role.as_str() {
        "user" => Ok(Msg::user(content)),
        "assistant" => {
            let p: AssistantPayload = match payload {
                Some(s) => serde_json::from_str(&s)
                    .with_context(|| format!("assistant payload 不是合法 JSON: {s}"))?,
                None => AssistantPayload::default(),
            };
            Ok(Msg::assistant_with(
                content,
                p.tool_calls.into_iter().map(ToolCall::from).collect(),
                p.thinking,
                None,
            ))
        }
        "tool" => {
            let p: ToolPayload = serde_json::from_str(
                payload
                    .as_deref()
                    .context("tool 消息缺少 payload（call_id 无处可取）")?,
            )
            .context("tool payload 不是合法 JSON")?;
            Ok(Msg::tool(p.call_id, content))
        }
        other => bail!("未知的消息角色: {other}"),
    }
}

/// 恢复上下文：该会话 `seq > after_seq` 的消息，按插入序。
///
/// 返回前会补齐「被中断的工具调用」：进程在工具轮中途被杀时，库里会留下一条带
/// `tool_calls` 的 assistant，但对应的 tool 结果没写。原样回传会让服务端 400
/// （每个 `tool_call` 都必须有配对结果），所以这里补一条占位 tool 消息。
/// 只补在内存里、不回写库 —— 事实是「当时确实没有结果」，不该伪造成真实输出。
pub fn load(tx: &Transaction<'_>, session_id: &str, after_seq: i64) -> Result<Vec<Msg>> {
    let rows = query_all(
        tx,
        "SELECT seq, role, content, payload FROM message \
         WHERE session_id = ?1 AND seq > ?2 ORDER BY seq ASC",
        params![session_id, after_seq],
        to_seq_msg,
    )?;
    Ok(fill_missing_tool_results(rows)
        .into_iter()
        .map(|r| r.msg)
        .collect())
}

/// 取最近 `turns` 个**完整对话轮**，供界面回看。
///
/// 为什么按轮而不按条：一轮 = 一条 user 到下一个 user 之前（含中间全部工具调用）。
/// 按条数截，工具调用密集的一轮会被劈成两半 —— 上一页末尾只剩几张工具卡，
/// 下一页开头找不到它们对应的提问。所以下界永远落在 user 消息上。
///
/// `before_seq` 是翻页游标（上一页最老那行的 seq，不含）；`None` = 从最新开始。
/// 本页第一行必然是一条 user 消息（除非已经翻到会话开头）。
///
/// 组内仍会补齐被中断的工具调用：不补的话历史里那张卡片没有结果，
/// 前端按「result 为空 = 执行中」判断会永远转圈。
pub fn load_turns(
    tx: &Transaction<'_>,
    session_id: &str,
    before_seq: Option<i64>,
    turns: usize,
) -> Result<Page> {
    if turns == 0 {
        bail!("turns 必须大于 0");
    }

    // 从游标往前数第 `turns` 个 user 行 = 本页下界（含）。数不到说明已经能翻到会话开头。
    let lower: Option<i64> = query_one(
        tx,
        "SELECT seq FROM message \
         WHERE session_id = ?1 AND role = 'user' AND (?2 IS NULL OR seq < ?2) \
         ORDER BY seq DESC LIMIT 1 OFFSET ?3",
        params![session_id, before_seq, (turns - 1) as i64],
        |row| Ok(row.get("seq")?),
    )?;

    let rows = query_all(
        tx,
        "SELECT seq, role, content, payload FROM message \
         WHERE session_id = ?1 AND (?2 IS NULL OR seq < ?2) AND (?3 IS NULL OR seq >= ?3) \
         ORDER BY seq ASC",
        params![session_id, before_seq, lower],
        to_seq_msg,
    )?;

    // 必须在补占位之前算：占位行的 seq 是 0，会把「本页第一行」带偏。
    let has_more = match rows.first() {
        Some(first) => query_one(
            tx,
            "SELECT EXISTS(SELECT 1 FROM message WHERE session_id = ?1 AND seq < ?2) AS e",
            params![session_id, first.seq],
            |row| Ok(row.get::<_, i64>("e")? != 0),
        )?
        .unwrap_or(false),
        None => false,
    };

    Ok(Page {
        rows: fill_missing_tool_results(rows),
        has_more,
    })
}

const INTERRUPTED: &str = "（该工具调用未返回结果：程序在对话中途退出）";

/// 给每个没有结果的 `call_id` 补一条占位 tool 消息。
///
/// 时机：走到下一条 assistant 或下一条 user，说明上一轮的 tool 结果不会再来了。
/// 补在「当前这条之前」，保持 `assistant → tool... → 下一条` 的原始顺序。
fn fill_missing_tool_results(rows: Vec<SeqMsg>) -> Vec<SeqMsg> {
    let mut out = Vec::with_capacity(rows.len());
    let mut pending: Vec<String> = Vec::new();

    fn flush(pending: &mut Vec<String>, out: &mut Vec<SeqMsg>) {
        for id in pending.drain(..) {
            // 占位消息不对应任何库内行，`seq` 记 0：它不为翻页提供位置，
            // 也永远不会成为一页的第一行（那一定是真实的 user 行）。
            out.push(SeqMsg {
                seq: 0,
                msg: Msg::tool(id, INTERRUPTED),
            });
        }
    }

    for row in rows {
        match &row.msg {
            Msg::Assistant(a) => {
                flush(&mut pending, &mut out);
                pending = a.tool_calls.iter().map(|c| c.id.clone()).collect();
            }
            Msg::Tool(t) => pending.retain(|id| id != &t.call_id),
            Msg::User(_) => flush(&mut pending, &mut out),
            Msg::System(_) => {}
        }
        out.push(row);
    }
    flush(&mut pending, &mut out);
    out
}

/// 从 `after_seq` 往后数第 `n` 条（`n` 从 1 开始）的 `seq`，不足 `n` 条返回 `None`。
///
/// 压缩回写水位用它：内存只知道「这次压掉了几条」，绝对位置要问库。
/// 起点是**当前水位**而不是会话第一条 —— 恢复过的会话内存条数不等于库内总行数，
/// 从第一条起数会把水位算低，已压缩的消息下次恢复会重新进上下文。
pub fn nth_seq_after(
    tx: &Transaction<'_>,
    session_id: &str,
    after_seq: i64,
    n: i64,
) -> Result<Option<i64>> {
    if n <= 0 {
        bail!("n 必须从 1 开始，收到 {n}");
    }
    query_one(
        tx,
        "SELECT seq FROM message \
         WHERE session_id = ?1 AND seq > ?2 ORDER BY seq ASC LIMIT 1 OFFSET ?3",
        params![session_id, after_seq, n - 1],
        |row| Ok(row.get("seq")?),
    )
}

/// 清掉水位覆盖范围内过旧的消息，只保留最近 `keep` 条，返回删除条数。
///
/// **只删 `seq <= upto_seq`**：水位之上是模型当前还要用的上下文，删了恢复时的切点
/// 就错了（[`nth_seq_after`] 要求 `seq > upto` 那段完整无缺口）。
///
/// 所以 `upto_seq = 0`（从未压缩）时一条都不会删 —— 那种会话的历史是用户仅有的，
/// 不能为了「条数上限」把唯一能回看的内容丢掉；而它也确实不受体积威胁（纯文本增长极慢）。
/// 「每会话保留多少条」是业务参数，本层只按给定的水位和条数执行。
pub fn trim(tx: &Transaction<'_>, session_id: &str, upto_seq: i64, keep: i64) -> Result<usize> {
    if keep < 0 {
        bail!("keep 不能为负，收到 {keep}");
    }
    let total: i64 = query_one(
        tx,
        "SELECT count(*) AS n FROM message WHERE session_id = ?1 AND seq <= ?2",
        params![session_id, upto_seq],
        |row| Ok(row.get("n")?),
    )?
    .context("count 未返回行")?;

    let excess = total - keep;
    if excess <= 0 {
        return Ok(0);
    }
    execute(
        tx,
        "DELETE FROM message WHERE seq IN ( \
           SELECT seq FROM message WHERE session_id = ?1 AND seq <= ?2 \
           ORDER BY seq ASC LIMIT ?3 \
         )",
        params![session_id, upto_seq, excess],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{session, workspace, Store};

    struct TempStore {
        store: Store,
        path: std::path::PathBuf,
    }

    impl TempStore {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!("hi-agent-msg-{tag}-{}.db", new_id()));
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

    use crate::util::new_id;

    /// 建工作区 + 会话，返回会话 id
    fn seed(store: &Store, tag: &str) -> String {
        let ws = store
            .tx(|tx| workspace::insert(tx, &format!("/proj/{tag}"), tag))
            .expect("插入工作区");
        store
            .tx(|tx| session::insert(tx, &ws.id, "测试会话"))
            .expect("插入会话")
            .id
    }

    /// 三种角色进库再读回，内容与形状都不变形
    #[test]
    fn roundtrip_three_roles() {
        let tmp = TempStore::new("roundtrip");
        let sid = seed(&tmp.store, "a");

        let msgs = vec![
            Msg::user("帮我看看 read.rs"),
            Msg::assistant_with(
                "",
                vec![ToolCall::new(
                    "call_1",
                    "read_file",
                    r#"{"path":"src/read.rs"}"#,
                )],
                Some("先读文件".to_string()),
                None,
            ),
            Msg::tool("call_1", "fn main() {}"),
            Msg::assistant("这段代码没问题"),
        ];

        for m in &msgs {
            tmp.store.tx(|tx| insert(tx, &sid, m)).expect("插入消息");
        }

        let back = tmp.store.tx(|tx| load(tx, &sid, 0)).expect("读取");
        assert_eq!(back, msgs, "读回的消息必须与写入的逐字段相等");

        // arguments 是原文，不能被重新序列化（键序、空白都要原样）
        match &back[1] {
            Msg::Assistant(a) => {
                assert_eq!(a.tool_calls[0].arguments, r#"{"path":"src/read.rs"}"#);
                assert_eq!(
                    a.thinking.as_ref().map(|t| t.text.as_str()),
                    Some("先读文件")
                );
                assert_eq!(a.thinking.as_ref().and_then(|t| t.signature.clone()), None);
            }
            other => panic!("应当是 assistant，实际 {other:?}"),
        }
    }

    /// 纯文本 assistant 不写 payload；user 也没有；tool 必须带 call_id
    #[test]
    fn payload_only_when_role_needs_it() {
        let tmp = TempStore::new("payload");
        let sid = seed(&tmp.store, "b");
        tmp.store
            .tx(|tx| insert(tx, &sid, &Msg::user("hi")))
            .expect("user");
        tmp.store
            .tx(|tx| insert(tx, &sid, &Msg::assistant("hello")))
            .expect("assistant");
        tmp.store
            .tx(|tx| insert(tx, &sid, &Msg::tool("c9", "ok")))
            .expect("tool");

        let rows: Vec<Option<String>> = tmp
            .store
            .tx(|tx| {
                query_all(
                    tx,
                    "SELECT payload FROM message WHERE session_id = ?1 ORDER BY seq",
                    params![sid],
                    |row| Ok(row.get("payload")?),
                )
            })
            .expect("查询");
        assert_eq!(rows[0], None, "user 无 payload");
        assert_eq!(rows[1], None, "纯文本 assistant 无 payload");
        assert!(
            rows[2].as_deref().unwrap_or("").contains("c9"),
            "tool 存 call_id"
        );
    }

    /// 缺结果的工具调用被补成占位消息，且位置在正确的那一轮里
    #[test]
    fn missing_tool_result_is_filled() {
        let tmp = TempStore::new("fill");
        let sid = seed(&tmp.store, "c");
        let msgs = [
            Msg::user("跑一下"),
            Msg::assistant_with(
                "",
                vec![
                    ToolCall::new("c1", "bash", "{}"),
                    ToolCall::new("c2", "bash", "{}"),
                ],
                None,
                None,
            ),
            // 只有 c1 有结果，c2 没来得及写
            Msg::tool("c1", "done"),
            Msg::user("算了，换个思路"),
        ];
        for m in &msgs {
            tmp.store.tx(|tx| insert(tx, &sid, m)).expect("插入");
        }

        let back = tmp.store.tx(|tx| load(tx, &sid, 0)).expect("读取");
        assert_eq!(back.len(), 5, "补一条占位");
        match &back[3] {
            Msg::Tool(t) => {
                assert_eq!(t.call_id, "c2");
                assert_eq!(t.content, INTERRUPTED);
            }
            other => panic!("第 4 条应是占位 tool，实际 {other:?}"),
        }
        assert!(matches!(back[4], Msg::User(_)), "占位补在用户新输入之前");
    }

    /// 从水位往后数第 n 条；不足时返回 None
    #[test]
    fn nth_seq_after_counts_from_mark() {
        let tmp = TempStore::new("nth");
        let sid = seed(&tmp.store, "d");
        let mut seqs = Vec::new();
        for i in 0..5 {
            let seq = tmp
                .store
                .tx(|tx| insert(tx, &sid, &Msg::user(format!("m{i}"))))
                .expect("插入");
            seqs.push(seq);
        }

        let third = tmp
            .store
            .tx(|tx| nth_seq_after(tx, &sid, 0, 3))
            .expect("查询");
        assert_eq!(third, Some(seqs[2]));

        // 从第二条之后数两条 → 第 4 条
        let fourth = tmp
            .store
            .tx(|tx| nth_seq_after(tx, &sid, seqs[1], 2))
            .expect("查询");
        assert_eq!(fourth, Some(seqs[3]));

        assert_eq!(
            tmp.store
                .tx(|tx| nth_seq_after(tx, &sid, 0, 6))
                .expect("查询"),
            None,
            "不足 n 条返回 None"
        );
    }

    /// `load` 只取水位之后的消息：恢复路径依赖它
    #[test]
    fn load_respects_watermark() {
        let tmp = TempStore::new("water");
        let sid = seed(&tmp.store, "e");
        let mut seqs = Vec::new();
        for i in 0..4 {
            let seq = tmp
                .store
                .tx(|tx| insert(tx, &sid, &Msg::user(format!("m{i}"))))
                .expect("插入");
            seqs.push(seq);
        }
        let back = tmp.store.tx(|tx| load(tx, &sid, seqs[1])).expect("读取");
        assert_eq!(back.len(), 2);
        assert_eq!(back[0], Msg::user("m2"));
    }

    #[test]
    fn system_msg_is_rejected() {
        let tmp = TempStore::new("sys");
        let sid = seed(&tmp.store, "f");
        let err = tmp
            .store
            .tx(|tx| insert(tx, &sid, &Msg::system("x")))
            .expect_err("system 不该落库");
        assert!(err.to_string().contains("system"));
    }

    /// 清理只在水位范围内删，且只删最旧的那几条；水位之上的消息一条都不能动。
    #[test]
    fn trim_only_below_watermark_and_only_the_oldest() {
        let tmp = TempStore::new("trim");
        let sid = seed(&tmp.store, "t");
        let mut seqs = Vec::new();
        for i in 0..10 {
            seqs.push(
                tmp.store
                    .tx(|tx| insert(tx, &sid, &Msg::user(format!("m{i}"))))
                    .expect("插入"),
            );
        }

        // 水位停在第 8 条：水位之内 8 条，keep=3 → 删最旧的 5 条
        let removed = tmp.store.tx(|tx| trim(tx, &sid, seqs[7], 3)).expect("清理");
        assert_eq!(removed, 5);

        let left = tmp.store.tx(|tx| load(tx, &sid, 0)).expect("读取");
        assert_eq!(left.len(), 5, "剩 3 条 + 水位之上 2 条");
        assert_eq!(left[0], Msg::user("m5"), "删的是最旧的 5 条");
        assert_eq!(left[4], Msg::user("m9"), "水位之上原样保留");
    }

    /// 没到阈值不删；从未压缩（水位 0）时一条也不删 —— 那种历史是用户仅有的。
    #[test]
    fn trim_is_noop_below_threshold_or_without_watermark() {
        let tmp = TempStore::new("trim-noop");
        let sid = seed(&tmp.store, "n");
        let mut seqs = Vec::new();
        for i in 0..5 {
            seqs.push(
                tmp.store
                    .tx(|tx| insert(tx, &sid, &Msg::user(format!("m{i}"))))
                    .expect("插入"),
            );
        }

        assert_eq!(
            tmp.store
                .tx(|tx| trim(tx, &sid, seqs[4], 100))
                .expect("清理"),
            0,
            "低于 keep 不删"
        );
        assert_eq!(
            tmp.store.tx(|tx| trim(tx, &sid, 0, 1)).expect("清理"),
            0,
            "水位为 0 时删不到任何行"
        );
        assert_eq!(tmp.store.tx(|tx| load(tx, &sid, 0)).expect("读取").len(), 5);
    }

    /// 按轮切页：每页以 user 开头、整轮带上，翻页能从新到旧走完且不多不少。
    #[test]
    fn load_turns_cuts_on_user_boundary() {
        let tmp = TempStore::new("turns");
        let sid = seed(&tmp.store, "p");

        // 三轮，每轮 user + assistant(两个工具调用) + 两条结果 + 收尾 assistant
        let mut users = Vec::new();
        for i in 0..3 {
            users.push(
                tmp.store
                    .tx(|tx| insert(tx, &sid, &Msg::user(format!("u{i}"))))
                    .expect("插入 user"),
            );
            let calls = Msg::assistant_with(
                "",
                vec![
                    ToolCall::new(&format!("c{i}a"), "bash", "{}"),
                    ToolCall::new(&format!("c{i}b"), "bash", "{}"),
                ],
                None,
                None,
            );
            tmp.store
                .tx(|tx| insert(tx, &sid, &calls))
                .expect("插入调用");
            for suffix in ["a", "b"] {
                tmp.store
                    .tx(|tx| insert(tx, &sid, &Msg::tool(format!("c{i}{suffix}"), "ok")))
                    .expect("插入结果");
            }
            tmp.store
                .tx(|tx| insert(tx, &sid, &Msg::assistant(format!("a{i}"))))
                .expect("插入回复");
        }

        // 一页一轮：5 条全在，页首是 user，且没有把一组掰开
        let first = tmp
            .store
            .tx(|tx| load_turns(tx, &sid, None, 1))
            .expect("第一页");
        assert_eq!(first.rows.len(), 5, "整轮必须一起带上");
        assert_eq!(first.rows[0].seq, users[2]);
        assert_eq!(first.rows[0].msg, Msg::user("u2"));
        assert!(first.has_more, "前面还有两轮");

        // 游标是上一页最老那行 → 正好上一轮
        let second = tmp
            .store
            .tx(|tx| load_turns(tx, &sid, Some(first.rows[0].seq), 1))
            .expect("第二页");
        assert_eq!(second.rows[0].msg, Msg::user("u1"));
        assert!(second.has_more);

        let third = tmp
            .store
            .tx(|tx| load_turns(tx, &sid, Some(second.rows[0].seq), 1))
            .expect("第三页");
        assert_eq!(third.rows[0].msg, Msg::user("u0"));
        assert_eq!(third.rows.len(), 5);
        assert!(!third.has_more, "到会话开头了");

        // 要的比有的多：一次全给，也不重复
        let all = tmp
            .store
            .tx(|tx| load_turns(tx, &sid, None, 10))
            .expect("全量");
        assert_eq!(all.rows.len(), 15);
        assert!(!all.has_more);
    }

    /// 分页同样补齐被中断的工具调用：否则历史里那张卡片没有结果，前端会一直转圈
    #[test]
    fn load_turns_fills_missing_tool_result() {
        let tmp = TempStore::new("turns-fill");
        let sid = seed(&tmp.store, "q");
        tmp.store
            .tx(|tx| insert(tx, &sid, &Msg::user("跑一下")))
            .expect("插入");
        let calls = Msg::assistant_with("", vec![ToolCall::new("c1", "bash", "{}")], None, None);
        tmp.store.tx(|tx| insert(tx, &sid, &calls)).expect("插入");

        let page = tmp
            .store
            .tx(|tx| load_turns(tx, &sid, None, 5))
            .expect("读取");
        assert_eq!(page.rows.len(), 3, "缺的那条被补上");
        match &page.rows[2].msg {
            Msg::Tool(t) => assert_eq!(t.content, INTERRUPTED),
            other => panic!("应补成 tool 占位，实际 {other:?}"),
        }
    }
}
