import { invoke } from '@tauri-apps/api/core'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'

import type {
  AskPayload,
  ChatEventPayloads,
  ContextUsagePayload,
  ToolResultPayload,
  ToolStartPayload,
} from './types'

/** 用户对确认弹窗的回答。 */
export type AskDecision = 'allow' | 'always' | 'deny'

/** 流式对话的回调集合。三类终止回调(done / cancelled / error)只会触发其中一个。 */
export interface StreamHandlers {
  /** 收到一个正文增量 token。 */
  onToken?: (token: string) => void
  /** 收到一段思考增量:内部推理,展示层级与正文、工具都不同。 */
  onThinking?: (text: string) => void
  /** 模型请求执行某个工具。 */
  onToolStart?: (tool: ToolStartPayload) => void
  /** 某个工具执行完毕(或未执行),按 id 与 onToolStart 配对。 */
  onToolResult?: (result: ToolResultPayload) => void
  /** 后端请求确认某次工具调用:弹窗,等用户选择后调 `answerAsk`。 */
  onAsk?: (ask: AskPayload) => void
  /** 该确认已作废(回答后、超时、或本轮被取消):前端据此关窗。 */
  onAskClosed?: (id: string) => void
  /** 一次模型请求的上下文用量快照(工具轮会多次到达,压缩后可能回落)。 */
  onUsage?: (usage: ContextUsagePayload) => void
  /** 正常结束(含用户取消——后端已补齐工具结果,取消不算错误)。 */
  onDone?: () => void
  /** 出错:后端 `llm-error` 的文本,或 invoke 本身抛出的异常。 */
  onError?: (message: string) => void
}

/**
 * 流式调用后端 chat_stream 命令,把 LLM 的逐 token 回复通过回调交给调用方。
 *
 * @param userInput 仅本轮用户输入(历史由后端 Memory 持有,不往返前端)
 * @returns 本轮消息落进哪个会话(空串 = 一条都没落成)。**不能从 `onDone` 里取**:
 *          `llm-done` 事件先于 invoke 的返回值到达,要等这个 Promise 兑现。
 */
export async function streamChat(
  userInput: string,
  handlers: StreamHandlers = {}
): Promise<string> {
  const {
    onToken,
    onThinking,
    onToolStart,
    onToolResult,
    onAsk,
    onAskClosed,
    onUsage,
    onDone,
    onError,
  } = handlers

  let unlistenToken: UnlistenFn | undefined
  let unlistenThinking: UnlistenFn | undefined
  let unlistenToolStart: UnlistenFn | undefined
  let unlistenToolResult: UnlistenFn | undefined
  let unlistenAsk: UnlistenFn | undefined
  let unlistenAskClosed: UnlistenFn | undefined
  let unlistenUsage: UnlistenFn | undefined
  let unlistenDone: UnlistenFn | undefined
  let unlistenError: UnlistenFn | undefined
  let unlistenCancelled: UnlistenFn | undefined

  // 统一清理全部监听器,确保无论正常结束还是异常都只清理一次
  const cleanup = (): void => {
    unlistenToken?.()
    unlistenThinking?.()
    unlistenToolStart?.()
    unlistenToolResult?.()
    unlistenAsk?.()
    unlistenAskClosed?.()
    unlistenUsage?.()
    unlistenDone?.()
    unlistenError?.()
    unlistenCancelled?.()
  }

  // 终止事件(done / cancelled / error)只处理一次:
  // 先清理再回调,避免回调抛错导致清理被跳过;finished 开关防止 done/error 竞态重复触发
  let finished = false
  const finish = (kind: 'done' | 'error', message?: string): void => {
    if (finished) return
    finished = true
    cleanup()
    if (kind === 'done') {
      onDone?.()
    } else {
      onError?.(message ?? '未知错误')
    }
  }

  // 先注册事件监听,再发起调用,确保不漏掉首个 token
  unlistenToken = await listen<ChatEventPayloads['llm-token']>('llm-token', (e) => {
    onToken?.(e.payload)
  })
  unlistenThinking = await listen<ChatEventPayloads['llm-thinking']>('llm-thinking', (e) => {
    onThinking?.(e.payload)
  })
  unlistenToolStart = await listen<ChatEventPayloads['agent-tool-start']>(
    'agent-tool-start',
    (e) => onToolStart?.(e.payload)
  )
  unlistenToolResult = await listen<ChatEventPayloads['agent-tool-result']>(
    'agent-tool-result',
    (e) => onToolResult?.(e.payload)
  )
  unlistenAsk = await listen<ChatEventPayloads['agent-ask']>('agent-ask', (e) => {
    onAsk?.(e.payload)
  })
  unlistenAskClosed = await listen<ChatEventPayloads['agent-ask-closed']>(
    'agent-ask-closed',
    (e) => onAskClosed?.(e.payload)
  )
  unlistenUsage = await listen<ChatEventPayloads['llm-usage']>('llm-usage', (e) => {
    onUsage?.(e.payload)
  })
  unlistenDone = await listen<ChatEventPayloads['llm-done']>('llm-done', () => finish('done'))
  // 取消是正常收尾(后端已补齐工具结果),不当错误处理
  unlistenCancelled = await listen<ChatEventPayloads['llm-cancelled']>('llm-cancelled', () =>
    finish('done')
  )
  unlistenError = await listen<ChatEventPayloads['llm-error']>('llm-error', (e) =>
    finish('error', e.payload)
  )

  try {
    return await invoke<string>('chat_stream', { userInput })
  } catch (err) {
    // invoke 的拒绝值来自命令层的 Result<String, String>,但运行时仍可能是任意值
    finish('error', String(err))
    // 错误时拿不到 id:它只随成功返回。界面按「还不知道会话」处理,
    // 下一轮会带回同一个 id(后端已记住这一行)。
    return ''
  }
}

/** 新对话:清空后端持有的会话记忆。 */
export async function newChat(): Promise<void> {
  await invoke<void>('new_chat')
}

/** 取消当前轮次:后端收尾(补齐工具结果)后发 llm-cancelled,按正常结束处理。 */
export async function cancelChat(): Promise<boolean> {
  return invoke<boolean>('cancel_chat')
}

/**
 * 回答工具确认弹窗。返回 false 表示这条请求已不在等(超时 / 已答 / 已取消),
 * 属于正常竞态,调用方不必提示。
 */
export async function answerAsk(id: string, decision: AskDecision): Promise<boolean> {
  return invoke<boolean>('answer_ask', { id, decision })
}
