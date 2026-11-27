import { ref, watch, type Ref } from 'vue'
import { ElMessage } from 'element-plus'

import { getResumeSession, listSessions } from '../api/session'
import type { SessionSummary } from '../api/types'
import { useChat } from './chat'
import { useWorkspace } from './workspace'

/**
 * 侧边栏的会话列表(跨组件共享的单例)。
 *
 * 列表只有一个来源:后端按 `updated_at` 倒序返回本工作区的会话。前端不排序、
 * 不拼接、不在本地新增 —— 会话是懒创建的,标题也是后端按首条输入截的,本地产不出来。
 */
const sessions: Ref<SessionSummary[]> = ref([])
// 是否已经拉过一次。列表为空时用来区分「真的没有对话」和「还没查完」——
// 后者闪一下「还没有对话」会让用户以为历史丢了。
const loaded = ref(false)

const { currentSessionId, isStreaming, openSession } = useChat()
const { workspaceVersion } = useWorkspace()

export interface UseSessions {
  /** 本工作区的会话,最近的在前 */
  sessions: Ref<SessionSummary[]>
  /** 是否已经拉过一次(为空时用来区分「没有」和「还没查」) */
  loaded: Ref<boolean>
  /** 重新拉一次(切工作区、新会话落定后自动触发,一般不用手动调) */
  refresh: () => Promise<void>
  /** 打开一条历史会话:消息与模型上下文一起切过去 */
  open: (id: string) => Promise<void>
  /** 启动时用:先拉列表,再按断点自动打开上次那个会话 */
  restore: () => Promise<void>
}

async function refresh(): Promise<void> {
  try {
    sessions.value = await listSessions()
  } catch {
    // 列表读不出来不该打断对话:保留上一份,不打扰用户
  } finally {
    loaded.value = true
  }
}

async function open(id: string): Promise<void> {
  // 流式期间整轮持着上下文锁,这时候切会话会卡在锁上直到那轮结束 ——
  // 用户会觉得「点了没反应」,所以直接不响应(侧边栏也会置灰)
  if (isStreaming.value || id === currentSessionId.value) return
  try {
    await openSession(id)
  } catch (e) {
    ElMessage.error(`打开会话失败：${String(e)}`)
  }
}

async function restore(): Promise<void> {
  await refresh()
  try {
    const id = await getResumeSession()
    if (!id) return
    await openSession(id)
  } catch {
    // 断点指向的会话可能已被清理:静默从空白开始(后端已经把这个指针清掉了)
  }
}

// 换工作区 → 整批换掉(会话归属于工作区,不能跨目录留存旧列表)
watch(workspaceVersion, () => {
  sessions.value = []
  void refresh()
})

// 当前会话落定(新会话首条消息 / 打开历史会话)→ 标题与顺序都可能变了。
// 这里会多拉一次(打开会话时 id 也变了),但列表最多 20 行,重复一次比漏更新好。
watch(currentSessionId, () => {
  void refresh()
})

export function useSessions(): UseSessions {
  return { sessions, loaded, refresh, open, restore }
}
