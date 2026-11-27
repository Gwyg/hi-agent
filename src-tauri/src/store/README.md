# 存储层（方案与现状）

程序生成的**一切结构化数据**住在一个 SQLite 库里：`~/.hi-agent/data/hi-agent.db`。
配置仍走 `app.toml`（人要能手改），诊断日志仍是纯文本——那是另外两类东西，不与此处重叠。

## 一、三层分工

| 层 | 谁 | 管什么 | 不认识什么 |
|---|---|---|---|
| 引擎 `store/engine/` | 私有内脏（`mod engine;` 无 `pub`） | 连接、参数、迁移、事务、参数化语句 | 业务表名、`Msg` |
| 仓储 `store/<聚合>.rs` | 对外服务台 | 这类数据**怎么读写**（排序、取最近 N、字段映射） | 业务规则、何时该写 |
| 业务 `ai/` `services/` | 调用方 | **什么时候、为什么要写** | SQL、表名、事务里的语句 |

一句话：**引擎管「能不能安全地写」，仓储管「这类数据怎么读写」，业务管「什么时候为什么要写」。**

边界只有一道，靠编译器：`mod engine;` 不带 `pub` —— 外部连 `crate::store::engine::` 这个路径都拼不出来。

引擎的执行路径**不定义自有类型**：事务、行、绑定值全用 `rusqlite` 的（`Transaction` / `Row` / `params!`），
`exec.rs` 只把「prepare → query → next」和错误上下文收成三个函数。包一层 `Row` 的代价是
「能取哪些类型」被我们的方法数量限死（`text` / `int` / 两个 `_opt` 就四种），而 `row.get(0)?`
的 `T` 能由目标字段反推，`bool` / `f64` / `u32` / BLOB 全都免费。

事务对象同样不包：`Store::tx` 交给闭包的就是 `rusqlite::Transaction`，仓储签名直接用它。
代价要说清：业务层拿到 `tx` 后 `tx.execute(...)`、`tx.execute_batch(...)` 都编得过，
「`store` 之外不出现 SQL」从此是**约定**，不再是编译期约束。

## 二、文件

```
store/
├─ mod.rs              Store 句柄（Mutex<Conn>）、open / open_at（路径归一化）/ tx / info / checkpoint
├─ engine/             ← 私有，外部不可见
│  ├─ conn.rs          打开 + 参数（WAL 拿不到则降级）+ 回读断言 + 按需 quick_check + 事务边界
│  ├─ migrate.rs       user_version + 有序 MIGRATIONS + 高版本拒开
│  └─ exec.rs          execute / query_one / query_all（不定义自有类型）
├─ sql/                0001_init.sql（v1）+ README.md（迁移纪律）
├─ workspace.rs        仓储：工作区（find_by_path / insert / touch / most_recent / list）
├─ session.rs          仓储：会话（insert / load / list_by_workspace / touch / rename / set_summary / trim_by_workspace）
└─ message.rs          仓储：消息（insert→seq / load / nth_seq_after / trim）；唯一认识 `Msg` 的地方
```

## 三、v1 schema

```sql
workspace(id TEXT PK, path TEXT, name TEXT, created_at INT, updated_at INT, last_opened_at INT)
session  (id TEXT PK, workspace_id TEXT, title TEXT, summary TEXT,
          summary_upto_seq INT, created_at INT, updated_at INT)
message  (seq INTEGER PK AUTOINCREMENT, session_id TEXT, role TEXT,
          content TEXT, payload TEXT, created_at INT)

ux_workspace_path(path)                    -- 同一目录只能一行
ix_workspace_recent(last_opened_at DESC)   -- 列表：最近打开的在前
ix_session_list(workspace_id, updated_at DESC)
ix_message_session(session_id, seq)
```

**没有外键**（一致性由业务事务保证，见下）。**没有 `message.id`**（`seq` 就是句柄）。
**没有 `thinking` 列**（进 `payload`）。

### 几个定型理由

- **`workspace.path` 只有一列**，入库前统一成「可直接使用的绝对路径」：
  `expand_tilde → resolve_lexical → canonical → 去 \\?\`，并且要求 `is_dir()`。
  幂等，所以它本身就能当唯一键。尾分隔符靠 `Path::components()` 重建，**不能字符串 trim**
  （`C:\` 剪成 `C:` 会变成「C 盘当前目录」）。
- **`last_opened_at` 初始值 = `created_at`**，否则新加的工作区沉到列表底部。
- **`session.summary` + `summary_upto_seq`** 表达「模型不必再看的边界」：
  恢复 = `[summary] + [seq > summary_upto_seq]`。摘要覆盖式更新，一个会话只有一份。
- **`message.seq` 是全局发号器**（`AUTOINCREMENT`：只增、删了不回收）。
  它有两个身份：`ORDER BY` 的依据 + 前端气泡的 key。**永不重排** ——
  任何「建新表 → 拷数据 → 改名」的迁移都必须把 `seq` 显式写进 `INSERT ... SELECT`
  并保住 `sqlite_sequence` 水位，否则 `summary_upto_seq` 当场失效。
- **`content` 独立成列**（三种角色共有的正文 + 将来 FTS），其余角色专属字段进 `payload`：

  ```jsonc
  user      → NULL
  assistant → {"thinking":"…","tool_calls":[{"id","name","arguments"}]}   两个键都可缺
  tool      → {"call_id":"…"}                                           配对 tool_calls[].id
  ```

  **`tool.status` 不存**：它只是流式过程中的即时反馈。历史里没有「正在跑」这回事——
  有结果行就是结束了，所以前端判断「执行中」看的是有没有结果，不是 status。
  代价：历史里失败/被拒的工具卡不再标红（结果文本里仍有说明）。

- **`thinking` 必须存、且恢复时装回上下文**（不是只发给界面）：
  `protocol/openai/req.rs` 把历史思考一律回传成 `reasoning_content`，DeepSeek 做成
  load-bearing，工具循环少回传一轮可能 400；`compaction::estimate_bytes` 也按它估 token。
  `Thinking.signature` 不存（Anthropic 凭据，DeepSeek 侧丢弃）。

## 四、落库与恢复

**落库粒度**：一轮对话结束（`Done` / 报错 / 取消）提交一次事务，流式 token 不写。

**一轮 = 一个事务**，内容包含：

```
INSERT message × N（user / assistant / tool / assistant …）
UPDATE session SET updated_at = ?, summary = ?, summary_upto_seq = ?   -- 需要时
```

这一步是**跨聚合**的，所以事务边界必须由业务层划：仓储函数接收事务对象
（`message::append(tx, &msg)`、`session::touch(tx, id)`），业务把它们放进同一个闭包。
业务层开事务、调仓储，SQL 只出现在仓储里（约定，见第一节末）。

**取号**：插入后用 `RETURNING seq` 一次拿到发到的号（SQLite 3.35+，我们 bundled 3.54），
不必再查 `last_insert_rowid()`。

**恢复**：`[session.summary] + [该会话 seq > summary_upto_seq 的消息，按 seq 升序]`。
盘上 `seq <= upto` 的那些只给用户回看，不进上下文。

**仓储要两条读路径**，不能只有一条：

| 方法 | 用途 | SQL 形状 |
|---|---|---|
| `load_context(session_id)` | 装回模型上下文 | `seq > upto ORDER BY seq` |
| `list_page(session_id, before_seq, limit)` | 用户向上翻看更早消息 | `seq < ? ORDER BY seq DESC LIMIT ?` |

**前端渲染**：磁盘上「一轮」是多行，屏上是**一个**气泡。恢复时按「两条 `user` 之间的一切
属于同一条 AI 消息」拼回去，`done` 一律 `true`（历史都是完成态），折叠逻辑原样生效。

四条必须守住的细节：

1. `assistant` 行 `content` 为空串时**不要推空 `text` 块** —— 否则折叠切点会落在这个空段落上，
   整轮内容全被折进过程区，外面只剩空白。
2. `payload.thinking` 要同时进 `Context` 和界面（两条路不能分叉）。
3. `tool` 行按 `payload.call_id` 回填到同组的工具块。
4. 组内一个块都没有就不 push 气泡（否则留下只有头像的空白气泡）。

**删除顺序**（无外键的代价，同一事务、顺序固定）：

```
message → session → workspace
```

反了子行就再也没人认领。`ix_session_list` / `ix_message_session` 的前导列就是关联列，
所以批量删子行不会全表扫。

## 五、清理

| 层 | 规则 | 触发 |
|---|---|---|
| 会话 | 每工作区**暂定 5 个**，超出删最老的（连带消息） | 新建会话时顺手做，一条 SQL |
| 消息 | 每会话**暂定 1000 条**，**只在 `seq <= summary_upto_seq` 区间里删** | 同上 |

条数上限是**兜底**，不是主清理手段：水位以上的消息模型下轮还要用，删了只剩摘要能救。
压缩由 token 触发（1M 窗口的 70%），通常远在 1000 条之后，所以单会话**可能暂时超上限**——
先超着，等压缩把水位推过去再清。真正的体积上界是「会话数 × 会话实际体积」。

**压缩只动内存，不删盘**：压缩是为省钱（可反复），删数据是为控体积（不可逆），两件事解耦。

**回收空间**：`auto_vacuum = INCREMENTAL`（建库时设），删完之后显式调
`PRAGMA incremental_vacuum` 才真正缩文件。调用点：删完会话/工作区后、退出时 best-effort。
**不要在对话中途调**（要写锁并搬页）。它只收文件尾部的空闲页，不整理碎片。
库中已有表时 SQLite **拒绝**切换到 INCREMENTAL（静默 no-op）：这种库只 warn 不阻塞启动，
要缩体积只能整库 `VACUUM` 重建。所以这个 pragma 的价值只在「第一次建库那一刻」。

## 六、迁移

- 版本号 = `PRAGMA user_version`；`MIGRATIONS` 是一个有序数组，一步一事务、版本号同事务写。
- **已发布的步骤永不修改**，改结构一律新增号（细则见 `sql/README.md`）。
- 库版本高于程序 → 拒绝打开，一个字节都不写。
- 某步失败 → 整步回滚，库停在原版本，下次启动从同一处重试（错误文本带版本号与库路径）。
- **迁移前不做备份**：`VACUUM INTO` 快照曾实现过（含保留 5 份的清理），但没有消费者，
  已整体删除。等真要做「可回退」时再回来设计——那时要连「怎么让用户找到并使用快照」一起想。

## 七、已完成的口子与接入时仍要做的事

原「接入前三件事」已全部清掉：

1. **`Store::tx` 是 `pub(crate)`**：业务层拿到的 `tx` 就是 `rusqlite::Transaction`，
   仓储签名直接收它。原来的 `Tx` 包装层已删除——它只转发三个方法，白加一层理解成本。
2. **`auto_vacuum` 回读失败不再一律致命**：空库仍 `bail`（此时设置必然生效，不生效就是构建/文件系统层面的真故障），
   库中已有表则 warn 后继续（SQLite 规定此时静默 no-op，不能因此让应用打不开）。守卫见
   `conn.rs::opens_legacy_database_whose_auto_vacuum_cannot_change`。
3. **迁移列表校验补全**：从 1 开始 + 严格递增 + **连续**，跳号直接拒（原来只拦乱序与 v0）；
   迁移失败的错误文本现在自带「哪一步、哪个库文件」，用户只看到弹窗那一行也能定位。
   `migrate.rs` 头注释里的外键表述也已改为「表之间有删除顺序依赖」。
4. **`quick_check` 从「每次启动」改成「上次可疑时」**：判据是 `-wal` 长度非 0
   （正常退出会 `checkpoint(TRUNCATE)` 把它截成 0 字节）。全库扫描是 O(库大小)，成本随用户积累线性增长；
   干净退出的库交给 SQLite 自己的页级校验（访问到坏页照样报 `SQLITE_CORRUPT`），
   命中判据则 warn 一条 + 查一次。**没启用 WAL 时也查**——那时 `-wal` 不存在、判据恒假，等于失去信号。
   `check_integrity()` 保持公开，留给将来的「诊断」入口手动触发。
   判据的前提是**退出路径真的调了 `checkpoint()`**，所以下面第一条不是可选项。
5. **WAL 拿不到不再拦启动**：网络盘 / 虚拟机共享目录 / FUSE 上 `PRAGMA journal_mode=WAL`
   会静默返回原模式（不报错）。现在退到 rollback journal 后正常启动，但 `synchronous` 必须跟着
   变成 `FULL` —— `NORMAL` 在该模式下断电**可能损坏库文件**，不只是丢最后一次提交。
   降级只在日志里 warn，UI 提示等接入时补。守卫见
   `conn.rs::falls_back_to_full_sync_when_wal_unavailable`。

顺带修掉一处静默风险：**`open_at` 会归一化路径**（`~` 展开 → 绝对化 → 跟软链 → 去 `\\?\`），
相对路径直接报错而不是落到进程 cwd。没有这层，`~/x` 会在某处默默建出一个名为 `~` 的目录。

**启动失败已接线**（`lib.rs::bootstrap` + `fatal`）。这一层的设计只有一条：

> **致命性 = 是否用 `?` 往上抛。** 想拦启动就 `?` / `bail!`，想降级运行就 `warn!` 后继续。
> 不需要额外的错误分类枚举——`WAL 不可用`、`老库 auto_vacuum 改不了`、`api_key 解不开`
> 都已经是后者，各自在模块内处理完了。

失败时走「记日志 → `logging::flush()` → 原生对话框 → `process::exit(1)`」。
顺序不能换：`process::exit` 跳过所有 `Drop`，非阻塞日志的缓冲必须主动刷出，
否则丢的正好是那条致命错误本身。对话框用 `{err:#}`（单行因果链），日志用 `{:?}`（含 backtrace）。

同理，**迁移失败的错误链里必须带库路径**——启动失败时用户只看得到弹窗那一行。

接入时仍要做：

- 退出时 `checkpoint()`（它同时是上面 quick_check 判据的前提：不调，下次启动就会白扫一遍）；
  开库是阻塞 IO 但量级只有几十毫秒，`bootstrap` 里同步做；真遇到大库秒级再改成「先出窗口 + 就绪事件」；
- 三张表的仓储都已就位并全部接进业务：`workspace` 接进启动恢复；`session` / `message`
  接进「跑一轮对话」（懒创建会话 + 逐条落库 + 收尾回写摘要水位，见 `services::conversation`）
  与「读取侧」（`session::list_by_workspace` / `message::load_turns`，见 `services::session`）。
  **展示与模型读的不是同一批**：界面要全量（`load_turns`，按轮切），模型只要水位之后
  （`load`，接 `summary_upto_seq`）——两样一起看才是完整历史，但混用会重复计数；
- **清理已接**（`message::trim` / `session::trim_by_workspace`，由 `services::conversation`
  在每轮收尾后踢一次后台任务）。两条业务参数在 `services` 里：水位之前留 2000 条消息、
  每工作区留 20 个会话。消息清理只删 `seq <= summary_upto_seq`，水位之上一条不动——
  那是模型当前还要用的上下文，也是 `nth_seq_after` 反推水位的完整性前提；
- `incremental_vacuum` 的调用点**还没接**：删完会话/工作区后、退出时 best-effort，**别在对话中途**。
  三条硬约束：必须带限额（`incremental_vacuum(128)`，裸调用会一次全收，那才可能长时间持写锁）；
  必须在事务外（它自己开写事务）；WAL 下 `ftruncate` 要等 checkpoint 才落地，所以调完文件可能
  一点没变，需要紧跟一次 `checkpoint(TRUNCATE)`，或接受「退出时才缩」。
  （不接也不漏空间：`auto_vacuum=INCREMENTAL` 下空闲页会被后续插入复用，文件约等于历史峰值。）
- 退出时 `checkpoint()` / 启动时 `check_integrity()` 都还没有调用点（实现已就位，
  `Store` 上是两处带注释的 `#[allow(dead_code)]`）。退出流程要按 `RunEvent::Exit` 接；
  开库是阻塞 IO 但量级只有几十毫秒，`bootstrap` 里同步做；真遇到大库秒级再改成
  「先出窗口 + 就绪事件」；
- `session::rename` 同样待接（侧边栏改名入口）；精确豁免而不是模块级 `allow`——
  新冒出来的死代码还能被看见。
