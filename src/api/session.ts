import { invoke } from '@tauri-apps/api/core'

import type { MessagePage, SessionPage, SessionSummary } from './types'

/**
 * 本工作区的会话，最近的在前。
 *
 * 不分页:一个工作区最多留 20 个会话(见后端会话清理),一次列全即可。
 */
export function listSessions(): Promise<SessionSummary[]> {
  return invoke<SessionSummary[]>('list_sessions')
}

/**
 * 打开一个已有会话。
 *
 * 这不只是「读消息」:后端会同时把模型上下文切到这个会话,所以本该在流式回复
 * 进行时禁用(整轮持锁,调用会卡到那一轮结束)。
 */
export function openSession(id: string): Promise<SessionPage> {
  return invoke<SessionPage>('open_session', { id })
}

/** 往前翻一页。`beforeSeq` 传上一页的 `oldest_seq`。只读,不动模型上下文。 */
export function loadOlder(sessionId: string, beforeSeq: number): Promise<MessagePage> {
  return invoke<MessagePage>('load_older', { sessionId, beforeSeq })
}

/** 上次打开过的会话(断点)。没有或已被清理时返回 null。 */
export function getResumeSession(): Promise<string | null> {
  return invoke<string | null>('get_resume_session')
}
