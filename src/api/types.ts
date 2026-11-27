/**
 * IPC 边界类型:与 Rust 命令层的传输形状一一对应。
 *
 * 这里只做**类型镜像**,不含任何逻辑——后端字段名是 snake_case(Rust 侧未做 serde rename),
 * 故此处保持原样,不做 camelCase 转换,避免两侧出现第二套命名。
 * 改后端 DTO 时同步改这里(两边字段对不上,vue-tsc 会当场报错)。
 */

/**
 * 思考强度。与 DeepSeek 官方 `reasoning_effort` 的三档一一对应；
 * 官方兼容别名(medium / xhigh / ultra 等)会被静默折成 high,故不在联集里。
 */
export type Effort = 'low' | 'high' | 'max'

/** `get_workspace` / `set_workspace` 的返回:当前工作区。 */
export interface WorkspaceView {
  id: string
  /** 入库时的绝对路径,供选择框定位与悬浮提示 */
  path: string
  /** 展示名:后端按目录名定的,不由前端拼 */
  name: string
}

/** `get_llm_config` 的返回:脱敏视图(api_key 永不明文回传,只给掩码)。 */
export interface LlmConfigView {
  /** 生效模型(文件里有就用,没有就是内置兜底),恒非空 */
  model: string
  /** 生效思考强度,后端已归一化,恒为三档之一 */
  effort: Effort
  api_key_set: boolean
  api_key_masked: string | null
  configured: boolean
}

/** `save_llm_config` 的入参。api_key 留空 = 沿用已保存的密钥(后端语义)。 */
export interface LlmConfigInput {
  model?: string
  api_key?: string
  /** 缺省 = 沿用当前档位 */
  effort?: Effort
}

/** `list_models` 的返回:官方模型列表的单条,供下拉选择 + 规格展示。 */
export interface ModelOption {
  id: string
  name: string
  context_window: number | null
  max_output_tokens: number | null
  input_modalities: string[]
}

/**
 * `get_balance` 的返回:官方 `GET /user/balance` 里的总可用余额。
 * 金额保留上游字符串原样(如 "11.80"),前端只拼货币符号、不做数值运算。
 */
export interface BalanceInfo {
  currency: string
  total_balance: string
}

/** `agent-tool-start` 事件载荷(传输层 DTO,由命令层构造)。 */
export interface ToolStartPayload {
  id: string
  name: string
  args: string
}

/**
 * 一次工具调用的结局,仅供展示:
 * - `ok`      跑成了
 * - `error`   跑了但报错(UI 标红并强制展开)
 * - `skipped` 压根没跑(被拒 / 被取消 / 工具不存在)
 */
export type ToolStatus = 'ok' | 'error' | 'skipped'

/** `agent-tool-result` 事件载荷。 */
export interface ToolResultPayload {
  id: string
  content: string
  status: ToolStatus
}

/**
 * `llm-usage` 事件载荷:一次模型请求的用量快照。
 * `used` 是端点 usage 的 input_tokens(真值,已含 system / 历史 / thinking / tools schema),
 * `total` 是模型上下文窗口,`cached` 是 `used` 里命中提示词缓存的部分;
 * 百分比由前端算,后端不做展示换算。
 */
export interface ContextUsagePayload {
  used: number
  total: number
  cached: number
}

/**
 * `agent-ask` 事件载荷:后端要问用户是否允许这次工具调用。
 *
 * `id` 是模型的 tool_call_id,回答时原样带回;`rememberable` 为 false 时不提供
 * 「总是允许」(后端对这类工具每次必问)。
 */
export interface AskPayload {
  id: string
  name: string
  args: string
  keys: string[]
  rememberable: boolean
}

/**
 * `history-trimmed` 事件载荷:后台清理已完成。
 *
 * 这是**会话维护**的通知,不属于任何一轮对话(它在收尾之后才发生),所以不在
 * `ChatEventPayloads` 里。删除不需要用户确认,但必须让他知道——否则某天往回翻
 * 会发现历史莫名少了一截。两类计数都可能是 0,事件只在至少一个非 0 时发出。
 */
export interface HistoryTrimmedPayload {
  /** 消息被清理的那个会话 */
  session_id: string
  /** 删掉的旧消息条数 */
  messages: number
  /** 删掉的旧会话个数(它们里面的消息不计入 messages) */
  sessions: number
}

/**
 * 事件名 → 载荷类型映射。
 *
 * `llm-done` / `llm-cancelled` 后端 emit 的是 `()`,序列化后即 `null`。
 */
export interface ChatEventPayloads {
  'llm-token': string
  /** 思考增量:与正文分流的内部推理,载荷就是文本片段 */
  'llm-thinking': string
  'llm-done': null
  'llm-cancelled': null
  'llm-error': string
  'agent-tool-start': ToolStartPayload
  'agent-tool-result': ToolResultPayload
  'agent-ask': AskPayload
  /** 该确认已作废(已回答 / 超时 / 取消),载荷是请求 id,前端据此关窗 */
  'agent-ask-closed': string
  /** 一次模型请求的上下文用量快照 */
  'llm-usage': ContextUsagePayload
}

// ——— 历史会话（session.rs）———

/** `list_sessions` 的返回:侧边栏列表里的一行。标题由后端截好,前端不再截。 */
export interface SessionSummary {
  id: string
  title: string
}

/** 一次工具调用(历史里唯一的携带者是 assistant 的 `tool_calls`)。 */
export interface StoredToolCall {
  id: string
  name: string
  /** 模型给的 JSON 原文,前端自己解析参数 */
  arguments: string
}

/**
 * 一条历史消息。后端已把三种角色摊平成同一套字段(不适用的是 undefined),
 * 前端不必再去解析 payload JSON。
 */
export interface StoredMessage {
  role: 'user' | 'assistant' | 'tool' | 'system'
  content: string
  /** assistant:这一轮的推理,可能是空的 */
  thinking?: string
  /** assistant:这一轮发起的工具调用 */
  tool_calls?: StoredToolCall[]
  /** tool:这条结果属于哪个调用 */
  call_id?: string
}

/** 一页历史消息。 */
export interface MessagePage {
  messages: StoredMessage[]
  /** 本页之前还有没有更早的 */
  has_more: boolean
  /** 本页最老一行的 seq,翻页时原样传回 */
  oldest_seq: number
}

/** `open_session` 的返回:会话 + 首屏那一页(后端 flatten,所以是平铺的)。 */
export interface SessionPage extends MessagePage {
  session: SessionSummary
}
