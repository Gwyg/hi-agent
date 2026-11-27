import { ref, watch, type Ref } from 'vue'
import { listen } from '@tauri-apps/api/event'
import { ElMessage } from 'element-plus'

import { answerAsk, newChat, streamChat, type AskDecision } from '../api/llm'
import { loadOlder, openSession as openSessionApi } from '../api/session'
import type {
  AskPayload,
  ContextUsagePayload,
  HistoryTrimmedPayload,
  StoredMessage,
  ToolStatus,
} from '../api/types'
import { useWorkspace } from './workspace'
import { useLlmConfig } from './llmConfig'
import { notifyTurnFinished } from '../utils/notify'

/** 气泡角色。 */
export type ChatRole = 'user' | 'assistant'

/**
 * 一条消息里的内容块,**顺序即发生顺序**。
 *
 * 后端的产出固定是「思考 → 正文 → 工具」:每轮模型回复先给推理、再给文本,
 * 最后可能跟一组工具调用。正文与思考各自按「末尾是同类块就续写」累积,
 * 工具事件插在中间,多次工具调用自然串成一条时间线,不需要额外的轮次字段。
 */
export type Block =
  | { kind: 'text'; text: string }
  /** 内部推理:纯文本渲染、自身可折叠,不复用 text 也不做成工具卡片 */
  | { kind: 'thinking'; text: string }
  | { kind: 'tool'; id: string; name: string; args: string; result?: string; status?: ToolStatus }
  | { kind: 'error'; text: string }

/** 前端展示用的消息。**不是**后端上下文的镜像,只负责气泡里显示什么。 */
export interface ChatMessage {
  role: ChatRole
  blocks: Block[]
  /** assistant 专用:本轮是否已结束。结束时过程区折叠,只留结论 */
  done?: boolean
  /** assistant 专用:本轮耗时(ms),结束时记账 */
  elapsed?: number
}

function greeting(content: string): ChatMessage[] {
  return [{ role: 'assistant', blocks: [{ kind: 'text', text: content }], done: true }]
}

/**
 * 库里的消息行 → 气泡列表。
 *
 * 拼法与实时路径对齐:一条 user 开一个气泡,其后**所有** assistant / tool 行都归
 * 紧接着的那一条 AI 消息(新一轮工具调用会再落一条 assistant,但它们属于同一次回答)。
 * 全部 `done: true` —— 历史都是已结束的轮次,折叠带的「已完成」和结论分区因此自动生效。
 *
 * 两条规则别改:
 * - **空 content 不推空 text 块**:只有工具调用的 assistant 会留下一个空白段;
 * - **tool 行只回填结果,不新建气泡**:否则工具卡片会散落在对话外面。
 */
function buildMessages(rows: StoredMessage[]): ChatMessage[] {
  const out: ChatMessage[] = []
  // 本组当前的 AI 气泡:遇到下一条 user 就换新的
  let cur: ChatMessage | null = null

  for (const row of rows) {
    if (row.role === 'user') {
      out.push({ role: 'user', blocks: [{ kind: 'text', text: row.content }] })
      cur = { role: 'assistant', blocks: [], done: true }
      out.push(cur)
      continue
    }

    if (row.role === 'tool') {
      // 后端保证每个 call_id 恰好一条结果(中断的那次也补了占位文本),
      // 所以这里回填完就不会再有「执行中」的卡片卡在历史里
      const target = cur?.blocks.find(
        (b) => b.kind === 'tool' && b.id === row.call_id && b.result === undefined
      )
      if (target?.kind === 'tool') target.result = row.content
      continue
    }

    // system 不落库,理论上不会出现;真出现也不进气泡
    if (row.role !== 'assistant') continue

    if (!cur) {
      cur = { role: 'assistant', blocks: [], done: true }
      out.push(cur)
    }
    if (row.thinking) cur.blocks.push({ kind: 'thinking', text: row.thinking })
    if (row.content) cur.blocks.push({ kind: 'text', text: row.content })
    for (const call of row.tool_calls ?? []) {
      cur.blocks.push({ kind: 'tool', id: call.id, name: call.name, args: call.arguments })
    }
  }

  // 一组只有 user、没有回复时留了个空 AI 气泡:不显示它
  return out.filter((m) => m.role === 'user' || m.blocks.length > 0)
}

// 跨组件共享的会话状态(模块级单例):侧边栏的「新对话」按钮与聊天页共用同一份,
// 所以不能把消息列表留在 ChatView 里。
const messages: Ref<ChatMessage[]> = ref(greeting('你好，我是 hi-agent，有什么可以帮你？'))
const isStreaming = ref(false)
// 每次重置会话 +1,聊天页监听它把视图滚到底(滚动是视图职责,不放这里)
const chatVersion = ref(0)
// 待确认的工具调用:非 null 时显示确认弹窗。后端鉴权是串行的,同一时刻最多一条
const pendingAsk: Ref<AskPayload | null> = ref(null)
// 最近一次模型请求的上下文用量快照(真值来自后端 usage);null = 尚未拿到 / 已重置
const contextUsage: Ref<ContextUsagePayload | null> = ref(null)
// 当前打开的历史会话。null = 还在「新对话」里(还没发过第一条消息)。
// 由「点开历史会话」和新会话落定两处更新，侧边栏据此高亮。
const currentSessionId: Ref<string | null> = ref(null)
// 历史分页：顶部还能不能往前翻、翻页游标（上一页最老那行的 seq）、是否正在翻
const hasMore = ref(false)
const oldestSeq = ref(0)
const loadingOlder = ref(false)

// 后台清理完成:它不属于任何一轮对话(在收尾之后才发生),所以在这里全局注册一次。
// 用轻提示而不是弹窗——删除不需要用户确认,但用户得知道,否则某天往回翻会发现
// 历史莫名少了一截。会话列表接进来后,这里会换成「历史顶部常驻一行」,
// 并只在用户正看着被清理的那个会话时才提示。
listen<HistoryTrimmedPayload>('history-trimmed', ({ payload }) => {
  const parts: string[] = []
  if (payload.messages > 0) parts.push(`${payload.messages} 条较早的消息`)
  if (payload.sessions > 0) parts.push(`${payload.sessions} 个较早的会话`)
  if (parts.length > 0) ElMessage.info(`已清理${parts.join('、')}`)
}).catch(() => {
  // 非 Tauri 环境(纯前端调试)没有事件通道,静默跳过
})

// 切换工作目录:后端已 reset_context,这里同步清空本地展示
const { workspaceVersion } = useWorkspace()
const { refreshBalanceAfterTurn } = useLlmConfig()
watch(workspaceVersion, () => {
  messages.value = greeting('已切换到新的工作目录，对话已清空。')
  currentSessionId.value = null
  hasMore.value = false
  oldestSeq.value = 0
  pendingAsk.value = null
  contextUsage.value = null
  chatVersion.value++
})

export interface UseChat {
  messages: Ref<ChatMessage[]>
  isStreaming: Ref<boolean>
  chatVersion: Ref<number>
  pendingAsk: Ref<AskPayload | null>
  contextUsage: Ref<ContextUsagePayload | null>
  /** 当前打开的会话 id；null = 还没落库的新对话 */
  currentSessionId: Ref<string | null>
  /** 顶部还能不能往前翻更早的消息 */
  hasMore: Ref<boolean>
  loadingOlder: Ref<boolean>
  sendMessage: (text: string) => Promise<void>
  answerPendingAsk: (decision: AskDecision) => Promise<void>
  startNewChat: () => Promise<void>
  /** 打开历史会话（消息 + 模型上下文一起切）；失败把错误抛给调用方 */
  openSession: (id: string) => Promise<void>
  /** 往前翻一页并拼到顶部；失败自己提示，不抛 */
  loadOlderMessages: () => Promise<void>
}

/**
 * 会话 composable:持有消息列表与发送/重置逻辑。
 * 真实历史由后端 Memory 持有,这里的 messages 只用于前端展示。
 */
export function useChat(): UseChat {
  // 发一条用户消息并接收流式回复(只把本轮输入发给后端,历史由后端 Memory 管理)
  async function sendMessage(text: string): Promise<void> {
    messages.value.push({ role: 'user', blocks: [{ kind: 'text', text }] })
    // 先放一条空的 assistant 占位,后面的事件都往它的 blocks 里追加
    messages.value.push({ role: 'assistant', blocks: [], done: false })
    const idx = messages.value.length - 1
    // 必须每次经由数组取回该条消息:直接持有 push 进去的**原始对象**再改它,
    // 会绕过 reactive 的 set 拦截,视图不会跟着更新(经典的 ref 深响应陷阱)
    const reply = (): ChatMessage => messages.value[idx]
    const startedAt = Date.now()

    // 三种收尾(正常结束 / 报错 / 取消——取消在 api 层折成 done)都可能已经产生费用,
    // 统一在这里刷账户余额,不需要用户手动点。不 await:纯展示,失败静默。
    // 延迟补刷由 refreshBalanceAfterTurn 负责,绕开上游结算/缓存延迟。
    const endTurn = (error?: string): void => {
      const elapsed = Date.now() - startedAt
      reply().done = true
      reply().elapsed = elapsed
      pendingAsk.value = null
      isStreaming.value = false
      refreshBalanceAfterTurn()
      // 用户已经离开应用才提醒(判断在 notify 里)。取消虽然也走这条路径,但要点取消
      // 按钮就必须先回到窗口,所以那一次不会被误判成「不在」。
      // 不 await:通知/声音/闪烁都是尽力而为,不能拖慢收尾。
      void notifyTurnFinished({ prompt: text, elapsed, error })
    }

    // 末尾是文本块就续写,否则新开一块。工具事件插在中间,时间线由此自然成形。
    // 必须经 reply() 取回响应式数组再按下标修改,不能持有 push 进去的原始对象。
    const appendText = (t: string): void => {
      const blocks = reply().blocks
      const last = blocks[blocks.length - 1]
      if (last?.kind === 'text') last.text += t
      else blocks.push({ kind: 'text', text: t })
    }

    // 思考同理:连续的思考片段合成一段,被正文或工具打断后下一段另起一块。
    // 于是多轮工具调用里的思考天然按段分开,块本身就记录了轮次边界。
    const appendThinking = (t: string): void => {
      const blocks = reply().blocks
      const last = blocks[blocks.length - 1]
      if (last?.kind === 'thinking') last.text += t
      else blocks.push({ kind: 'thinking', text: t })
    }

    isStreaming.value = true
    const sessionId = await streamChat(text, {
      onToken: appendText,
      onThinking: appendThinking,
      onToolStart: (tool) => {
        reply().blocks.push({ kind: 'tool', id: tool.id, name: tool.name, args: tool.args })
      },
      onToolResult: ({ id, content, status }) => {
        // 后端保证每个 tool_call_id 恰好一条结果,按 id 配回发起事件
        const target = reply().blocks.find((b) => b.kind === 'tool' && b.id === id)
        if (target && target.kind === 'tool') {
          target.result = content
          target.status = status
        }
      },
      onAsk: (ask) => {
        pendingAsk.value = ask
      },
      onAskClosed: (id) => {
        // 只关当前这条:迟到的作废通知不能误关下一条(超时后用户才点允许的竞态)
        if (pendingAsk.value?.id === id) pendingAsk.value = null
      },
      onUsage: (usage) => {
        // 每次模型响应都会推一条(含工具轮),后到覆盖前到;压缩后数值可能回落
        contextUsage.value = usage
      },
      onDone: () => {
        const blocks = reply().blocks
        // 整轮没有任何正文(异常路径)时兜一句,避免留下只有思考/工具条的空气泡
        if (!blocks.some((b) => b.kind === 'text')) {
          blocks.push({ kind: 'text', text: '（未收到回复）' })
        }
        endTurn()
      },
      onError: (e) => {
        reply().blocks.push({ kind: 'error', text: e })
        endTurn(e)
      },
    })
    // 会话 id 只能从 invoke 的返回值拿(事件先于它到达);空串 = 一条都没落成。
    // 首轮失败时这里留空,下一轮会带回同一个 id(后端已记住那一行),高亮届时补上。
    if (sessionId) currentSessionId.value = sessionId
  }

  // 回答确认弹窗:先关窗再回传,防连点;false(已作废)是正常竞态,不提示
  async function answerPendingAsk(decision: AskDecision): Promise<void> {
    const ask = pendingAsk.value
    if (!ask) return
    pendingAsk.value = null
    try {
      await answerAsk(ask.id, decision)
    } catch {
      // 回传本身失败(窗口正在关闭等):弹窗已关,后端会按超时收尾,不打断对话
    }
  }

  // 新对话:清空后端记忆 + 重置本地展示
  async function startNewChat(): Promise<void> {
    if (isStreaming.value) return
    await newChat()
    messages.value = greeting('你好，我是 hi-agent，有什么可以帮你？')
    currentSessionId.value = null
    hasMore.value = false
    oldestSeq.value = 0
    pendingAsk.value = null
    contextUsage.value = null
    chatVersion.value++
  }

  /**
   * 打开历史会话。
   *
   * 后端会把**模型上下文一起**切过去,所以这不是「取一页消息」那么轻——
   * 打开之后继续发消息,是接着这段对话说的。
   */
  async function openSession(id: string): Promise<void> {
    if (isStreaming.value) return
    const page = await openSessionApi(id)
    messages.value = buildMessages(page.messages)
    currentSessionId.value = page.session.id
    hasMore.value = page.has_more
    oldestSeq.value = page.oldest_seq
    loadingOlder.value = false
    pendingAsk.value = null
    contextUsage.value = null
    chatVersion.value++ // 打开即贴底:用户要看的是最后说到哪
  }

  /**
   * 往前翻一页,拼到列表顶部。
   *
   * 不碰 `chatVersion`:那会被视图当成「换了会话」而滚到底部,可用户此刻正站在顶部,
   * 等的是更早的内容。滚动位置的保持交给视图(只有它知道滚动容器)。
   */
  async function loadOlderMessages(): Promise<void> {
    const id = currentSessionId.value
    if (!id || !hasMore.value || loadingOlder.value || oldestSeq.value <= 0) return
    loadingOlder.value = true
    try {
      const page = await loadOlder(id, oldestSeq.value)
      messages.value = [...buildMessages(page.messages), ...messages.value]
      hasMore.value = page.has_more
      oldestSeq.value = page.oldest_seq
    } catch (e) {
      ElMessage.error(`加载更早的消息失败：${String(e)}`)
    } finally {
      loadingOlder.value = false
    }
  }

  return {
    messages,
    isStreaming,
    chatVersion,
    pendingAsk,
    contextUsage,
    currentSessionId,
    hasMore,
    loadingOlder,
    sendMessage,
    answerPendingAsk,
    startNewChat,
    openSession,
    loadOlderMessages,
  }
}
