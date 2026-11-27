import { ref, type Ref } from 'vue'

import { invoke } from '@tauri-apps/api/core'
import { open } from '@tauri-apps/plugin-dialog'
import { ElMessage } from 'element-plus'
import type { WorkspaceView } from '../api/types'

// 跨组件共享的工作区状态(模块级单例):
// - currentWorkspace:当前目录的绝对路径,判断「是不是同一个工作区」用,不直接展示
// - workspaceName:展示名(后端按目录名定的),状态栏与切换器只显示这个
// - workspaceVersion:每次成功切换 +1,ChatView 监听它来清空本地历史(与服务端 reset 同步)
const currentWorkspace = ref('')
const workspaceName = ref('')
const workspaceVersion = ref(0)

export interface UseWorkspace {
  /** 绝对路径:选择框的默认位置、悬浮提示的完整值、列表里标当前项 */
  currentWorkspace: Ref<string>
  /** 展示名:状态栏显示的就是它 */
  workspaceName: Ref<string>
  workspaceVersion: Ref<number>
  refresh: () => Promise<void>
  /** 切到指定路径(列表里点一条走这里);失败把后端错误抛给调用方 */
  switchTo: (path: string) => Promise<void>
  /** 弹系统目录框选一个新目录,内部复用 switchTo,错误自己提示 */
  selectWorkspace: () => Promise<void>
  /** 分页取工作区(最近打开在前);limit 后端会再夹紧一次 */
  listWorkspaces: (offset: number, limit: number) => Promise<WorkspaceView[]>
}

/**
 * 工作目录 composable:封装「切换 + 展示 + 列表」的前端状态。
 * 后端负责入库、注入边界根、写断点与清对话;名字也由后端定,这里不猜。
 */
export function useWorkspace(): UseWorkspace {
  // 启动时从后端取当前工作区(启动流程已经确定好一个,不会是空的)
  async function refresh(): Promise<void> {
    const ws = await invoke<WorkspaceView | null>('get_workspace')
    currentWorkspace.value = ws?.path ?? ''
    workspaceName.value = ws?.name ?? ''
  }

  // 切换的唯一落地点:列表选择与系统选择框都走这里,
  // 避免两处各写一遍「更新三个 ref」而漏掉某处
  async function switchTo(path: string): Promise<void> {
    const ws = await invoke<WorkspaceView>('set_workspace', { path })
    currentWorkspace.value = ws.path
    workspaceName.value = ws.name
    workspaceVersion.value++ // 通知聊天页清空本地历史
  }

  // 弹系统目录选择框,选中后通知后端切换
  async function selectWorkspace(): Promise<void> {
    try {
      const selected = await open({
        directory: true,
        defaultPath: currentWorkspace.value || undefined,
      })
      if (!selected) return // 用户取消,不是错误
      // 插件在未显式传 multiple 时返回 string;这里仍兼容数组形态(选项组合变化时不至于崩)
      const path = Array.isArray(selected) ? selected[0] : selected
      if (!path) return
      await switchTo(path)
    } catch (e) {
      // 失败时不动本地状态:后端没切成功,界面就不该显示成已切换。
      // 不透传会让用户看到「点了没反应」,所以显式提示。
      ElMessage.error(`切换工作目录失败：${String(e)}`)
    }
  }

  function listWorkspaces(offset: number, limit: number): Promise<WorkspaceView[]> {
    return invoke<WorkspaceView[]>('list_workspaces', { offset, limit })
  }

  return {
    currentWorkspace,
    workspaceName,
    workspaceVersion,
    refresh,
    switchTo,
    selectWorkspace,
    listWorkspaces,
  }
}
