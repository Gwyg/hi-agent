// 归档记忆层:跨会话持久化,按需检索召回。
//
// 参考成熟方案:
// - MemGPT/Letta:Archival Memory(向量库,语义检索)
// - Claude Code:memory/ 目录(文件式)
// - Codex:~/.codex/memories/*.md(grep markdown)
//
// 本期不实现,占位。未来做"会话续接/跨会话记忆"时在此落地。
//
// TODO(长期):
//   - 会话存档/加载
//   - 按需召回(语义检索 or 文件 grep)
//   - 与工作层压缩的衔接(压缩产物可入档)
