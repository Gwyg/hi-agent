# 迁移脚本

这里的每个 `.sql` 就是一次 schema 变更，文件名 = 版本号，四位零填充。

## 三条纪律

1. **已发布的步骤永不修改。** 你改了别人机器上的旧库并不知道；它只会跑到一半就撞上
   「表已存在」或悄悄缺列。要改 schema，就新增一个号。
2. **一步一个事务。** `migrate.rs` 会把整段 SQL 和 `PRAGMA user_version` 放在同一事务里
   提交，所以文件里**不要**出现 `BEGIN` / `COMMIT`。
3. **版本号从 1 开始、严格递增且连续。** 不按业务域分号（`session-01.sql` 这种），
   表之间有先后依赖，执行顺序必须确定。**跳号同样被拒**：`1, 3` 意味着中间那步被删了，
   老库的 `user_version` 已经跨过它，新库却会直接跑下一步，两份库从此结构不同。
   新增步骤 = 加一个文件 + 在 `store/engine/migrate.rs` 的 `MIGRATIONS` 里登记一行。

## 写法约定

- 只写 DDL 与必要的数据回填（回填过大时改用 Rust 分步做，别把几万行 IO 塞进一个事务）。
- 加列用 `ALTER TABLE ... ADD COLUMN`，且新列必须可空或带默认值（SQLite 的既有行要有值）。
- **不要**用 `IF NOT EXISTS` 当幂等兜底——幂等由版本号负责，兜底只会掩盖「步骤被重复执行」。
- 会被 `WHERE` / `ORDER BY` 的字段建列并加索引；只被原样回显的载荷放 `TEXT` 存 JSON。
- 改名/删列在 SQLite 里要重建表（`CREATE new → INSERT SELECT → DROP → RENAME`），
  放在单独一步里，别和别的变更混在一起。
- 重建带 `AUTOINCREMENT` 句柄列的表（`message.seq`）时，必须把该列**显式**写进
  `INSERT ... SELECT`，否则 SQLite 会重新发号，`session.summary_upto_seq` 立刻失效。

## 现状

| 版本 | 文件 | 内容 |
|---|---|---|
| v1 | `0001_init.sql` | `workspace`（工作区 = 一个目录）、`session`（一段对话）、`message`（一条消息） |

**v1 尚未发布**（正式库还没在任何用户机器上创建过），所以新表仍可并入 `0001`；
一旦发布，`0001_init.sql` 立即冻结，之后一律新增 `0002_xxx.sql`。

后续按「一次讨论定一张表」推进，新表一律新增下一个号。

登记方式：`store/engine/migrate.rs` 里的 `MIGRATIONS` 加一行
`(N, include_str!("../sql/000N_xxx.sql"))`（注意 `../`：那个文件在 `store/engine/` 下）。
