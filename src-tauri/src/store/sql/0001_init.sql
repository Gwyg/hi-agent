-- v1 · 工作区、会话、消息
--
-- 纪律见同目录 README.md（已发布步骤不改、不写 BEGIN/COMMIT、不写 IF NOT EXISTS）。
--
-- 刻意不用外键，一致性由业务事务保证。两个前提，缺一不可：
--   ① 删父行前先删子行，顺序 message → session → workspace，同一事务；
--   ② 子表在关联列上必须有索引（见 ix_session_list、ix_message_session），否则删父行要全表扫。

-- 工作区 = 用户交给 AI 读写的一个目录。
--
-- `path` 在入库前统一成「可直接使用的绝对路径」：展开 `~` → 折 `.` 与 `..` → 取真实大小写
-- → 去掉多余尾分隔符。这套统一幂等，唯一索引才成立。
-- 尾分隔符交给 `Path::components()` 重建，**不能字符串 trim**：`C:\` 剪成 `C:` 在 Windows 上
-- 意思变成「C 盘当前目录」，指向完全不同的位置。
CREATE TABLE workspace (
  id             TEXT    PRIMARY KEY,           -- UUID v7（按字符串排 ≈ 按时间排）
  path           TEXT    NOT NULL,              -- 已统一的绝对路径，见上
  name           TEXT    NOT NULL DEFAULT '',   -- 显示名：入库时取目录名，之后可由用户改名
  created_at     INTEGER NOT NULL,              -- Unix 毫秒
  updated_at     INTEGER NOT NULL,              -- 改名等元数据变更
  last_opened_at INTEGER NOT NULL               -- 最近一次打开：列表排序 + 记住上次
);

-- 一个目录只能有一条；`C:\A` / `c:/a/` / `\\?\C:\A` 的差异靠入库前统一消掉。
CREATE UNIQUE INDEX ux_workspace_path ON workspace(path);

-- 最近打开的在前；最大值即「上次用的工作区」，不需要额外的状态列。
CREATE INDEX ix_workspace_recent ON workspace(last_opened_at DESC);

-- 会话 = 一段对话。消息不挂在这里（那会变成一列数组），由 `message.session_id` 反向指回。
--
-- 恢复上下文 = `summary` + `seq > summary_upto_seq` 的消息；更早的原文仍在盘上供用户翻。
-- 摘要是覆盖式更新（新摘要已含旧摘要），所以只需一份，没有摘要历史表。
CREATE TABLE session (
  id               TEXT    PRIMARY KEY,           -- UUID v7
  workspace_id     TEXT    NOT NULL,              -- workspace.id，不设外键
  title            TEXT    NOT NULL DEFAULT '',   -- 空串 → UI 显示「新对话」
  summary          TEXT    NOT NULL DEFAULT '',   -- 最新摘要；空串 = 从未压缩
  summary_upto_seq INTEGER NOT NULL DEFAULT 0,    -- 本会话 seq ≤ 此值的已被摘要覆盖
  created_at       INTEGER NOT NULL,              -- Unix 毫秒
  updated_at       INTEGER NOT NULL               -- 收新消息 / 改名都更新
);

-- 会话列表（某工作区下、最近的在前），同时支撑删工作区时按 workspace_id 批量删会话。
CREATE INDEX ix_session_list ON session(workspace_id, updated_at DESC);

-- 消息 = 一行一条。user / assistant / tool 落库，system 每轮现渲染、不落库。
--
-- `seq` 是唯一句柄（AUTOINCREMENT：只增、删了不回收）：既当 ORDER BY，也当前端定位 id，
-- 所以没有 UUID 列。**永不重排**——任何重建表的迁移都必须把 seq 显式写进 `INSERT ... SELECT`，
-- 号一旦重发，`session.summary_upto_seq` 立刻失效（旧消息当新消息重进上下文，或新消息被跳过）。
-- 号全局连续（跨会话），只保证同一会话内递增，不要跨会话比大小。
--
-- `content` 三种角色共有，独立成列（渲染 + 将来 FTS5）；角色专属字段一律进 `payload`（JSON）：
--   user      → NULL（正文在 content）
--   assistant → {"thinking":"…","tool_calls":[{"id","name","arguments"}]}，两键都可缺
--               `arguments` 存协议原文，不 parse、不重排，否则回传会失真
--   tool      → {"call_id":"…"}；**不存 status**，「执行中」看有没有结果行
-- 判据：三种角色都有 → 列；角色专属 → JSON；要 WHERE / ORDER BY → 单独一列。
--
-- `thinking` 必须原样存、并装回上下文：`req.rs` 一律回传成 `reasoning_content`，少一轮可能
-- 400；token 水位也按它估。`Thinking.signature` 不存（那是 Anthropic 的凭据）。
--
-- 每会话最多 1000 条，且只在压缩已覆盖的区间里删（水位之上模型下轮还要用）：
--   DELETE FROM message WHERE seq IN (
--     SELECT seq FROM message WHERE session_id = ?1 AND seq <= ?2 ORDER BY seq ASC LIMIT ?3);
CREATE TABLE message (
  seq        INTEGER PRIMARY KEY AUTOINCREMENT,  -- 全局插入序 + 消息句柄
  session_id TEXT    NOT NULL,                   -- session.id，不设外键
  role       TEXT    NOT NULL,                   -- user / assistant / tool
  content    TEXT    NOT NULL DEFAULT '',        -- 正文；只有工具调用的 assistant 是空串
  payload    TEXT,                               -- JSON，随 role 变；无内容时 NULL
  created_at INTEGER NOT NULL                    -- Unix 毫秒
);

-- 恢复（WHERE session_id = ? ORDER BY seq）与上面的清理都走它。
CREATE INDEX ix_message_session ON message(session_id, seq);
