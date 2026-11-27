<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import { ElMessage } from 'element-plus'
import { useChat } from '../composables/chat'
import { useWorkspace } from '../composables/workspace'
import type { WorkspaceView } from '../api/types'

/**
 * 工作区切换器：侧边栏顶部显示当前工作区名，点开是「最近打开」列表。
 *
 * 位置在「新对话」之上：工作区是范围界定，下面的会话列表都属于它，
 * 换目录就换一批会话——层级关系比放在底部更直白。
 *
 * - 列表按 `last_opened_at` 倒序，后端分页；滚到底再取下一页（懒加载）。
 *   不做预取、不做虚拟滚动：一页 20 条、可视区约 5 行，滚一次够用一阵。
 * - 只读 + 切换，不从列表删任何项：删工作区要先定「清不清它的会话」，另说。
 * - 目录被删 / 盘没插的行照样列出（用户可能只是暂时没插盘），
 *   真选中了由后端 `switch` 报错，这里原样提示。
 */
const PAGE = 20

const { currentWorkspace, workspaceName, switchTo, selectWorkspace, listWorkspaces } =
  useWorkspace()
const { isStreaming } = useChat()

const rootEl = ref<HTMLElement | null>(null)
const listEl = ref<HTMLElement | null>(null)
const open = ref(false)
const items = ref<WorkspaceView[]>([])
const loading = ref(false)
const done = ref(false)
const saving = ref(false)

// 启动早期可能还没拿到工作区，给个中性占位而不是空白按钮
const currentName = computed<string>(() => workspaceName.value || '工作区')

function toggle(): void {
  if (open.value) {
    close()
    return
  }
  void openPanel()
}

function close(): void {
  open.value = false
}

// 每次打开都从第一页重拉：上次打开到现在可能已经切过工作区，列表顺序变了
async function openPanel(): Promise<void> {
  open.value = true
  items.value = []
  done.value = false
  loading.value = false
  await loadMore()
}

async function loadMore(): Promise<void> {
  if (loading.value || done.value) return
  loading.value = true
  try {
    const page = await listWorkspaces(items.value.length, PAGE)
    items.value.push(...page)
    // 不足一页即到底：多请求一次空页没有意义
    if (page.length < PAGE) done.value = true
  } catch (e) {
    ElMessage.error(`读取工作区列表失败：${String(e)}`)
    // 置为到底，避免滚动时反复重试同一个失败请求
    done.value = true
  } finally {
    loading.value = false
  }
}

function onScroll(): void {
  const el = listEl.value
  if (!el) return
  // 留 12px 余量：小数像素下滚动位置可能差一点点到不了底
  if (el.scrollTop + el.clientHeight >= el.scrollHeight - 12) void loadMore()
}

async function choose(ws: WorkspaceView): Promise<void> {
  if (saving.value) return
  if (ws.path === currentWorkspace.value) {
    close() // 点当前项等同于关闭，不白跑一次切换
    return
  }
  saving.value = true
  try {
    await switchTo(ws.path)
    close()
  } catch (e) {
    ElMessage.error(`切换工作目录失败：${String(e)}`)
  } finally {
    saving.value = false
  }
}

async function browse(): Promise<void> {
  if (saving.value) return
  close() // 先收面板再弹系统框，避免两个浮层叠在一起
  await selectWorkspace()
}

function onDocPointerDown(e: PointerEvent): void {
  if (open.value && rootEl.value && !rootEl.value.contains(e.target as Node)) close()
}

function onKeydown(e: KeyboardEvent): void {
  if (e.key === 'Escape') close()
}

onMounted(() => {
  document.addEventListener('pointerdown', onDocPointerDown)
  window.addEventListener('keydown', onKeydown)
})
onBeforeUnmount(() => {
  document.removeEventListener('pointerdown', onDocPointerDown)
  window.removeEventListener('keydown', onKeydown)
})
</script>

<template>
  <div ref="rootEl" class="ws">
    <button
      class="ws__trigger"
      :class="{ 'ws__trigger--open': open }"
      type="button"
      :aria-expanded="open"
      aria-label="切换工作区"
      @click="toggle"
    >
      <svg class="ws__folder" viewBox="0 0 16 16" aria-hidden="true">
        <path
          d="M1.8 5.2c0-.8.6-1.4 1.4-1.4h2.6l1.4 1.6h5c.8 0 1.4.6 1.4 1.4v5c0 .8-.6 1.4-1.4 1.4H3.2c-.8 0-1.4-.6-1.4-1.4V5.2z"
          fill="none"
          stroke="currentColor"
          stroke-width="1.2"
          stroke-linejoin="round"
        />
      </svg>
      <span class="ws__name">{{ currentName }}</span>
      <svg
        class="ws__chevron"
        :class="{ 'ws__chevron--open': open }"
        viewBox="0 0 12 12"
        aria-hidden="true"
      >
        <path
          d="M2.5 4.5 L6 8 L9.5 4.5"
          fill="none"
          stroke="currentColor"
          stroke-width="1.6"
          stroke-linecap="round"
          stroke-linejoin="round"
        />
      </svg>
    </button>

    <div v-if="open" class="ws__panel" role="menu" aria-label="工作区">
      <div class="ws__panel-title">最近打开</div>

      <div ref="listEl" class="ws__list" @scroll="onScroll">
        <button
          v-for="ws in items"
          :key="ws.id"
          class="ws__item"
          :class="{ 'ws__item--active': ws.path === currentWorkspace }"
          type="button"
          role="menuitemradio"
          :aria-checked="ws.path === currentWorkspace"
          :disabled="saving || isStreaming"
          @click="choose(ws)"
        >
          <span class="ws__item-main">
            <span class="ws__item-name">{{ ws.name }}</span>
            <span class="ws__item-path">{{ ws.path }}</span>
          </span>
          <svg v-if="ws.path === currentWorkspace" class="ws__check" viewBox="0 0 16 16" aria-hidden="true">
            <path
              d="M3 8.4 L6.4 11.6 L13 4.8"
              fill="none"
              stroke="currentColor"
              stroke-width="1.9"
              stroke-linecap="round"
              stroke-linejoin="round"
            />
          </svg>
        </button>

        <div v-if="loading" class="ws__hint">加载中…</div>
        <div v-else-if="items.length === 0" class="ws__hint">暂无工作区</div>
      </div>

      <button class="ws__browse" type="button" :disabled="saving || isStreaming" @click="browse">
        <svg class="ws__plus" viewBox="0 0 12 12" aria-hidden="true">
          <path
            d="M6 2.5 V9.5 M2.5 6 H9.5"
            fill="none"
            stroke="currentColor"
            stroke-width="1.5"
            stroke-linecap="round"
          />
        </svg>
        打开其他目录…
      </button>
    </div>
  </div>
</template>

<style scoped>
/* 顶部第一行:与下方「新对话」留一点呼吸 */
.ws {
  position: relative;
  flex-shrink: 0;
  margin-bottom: 8px;
}

/* 常态不画框:它只回答「我在哪个目录」,不是主操作。
   视觉重量全部让给下面的「新对话」,只在 hover / 展开时才浮一层浅底——
   两个等宽方框上下叠会把「我在哪」和「我要做什么」读成平级。 */
.ws__trigger {
  display: flex;
  align-items: center;
  gap: 6px;
  width: 100%;
  height: 28px;
  padding: 0 4px;
  border: none;
  border-radius: 6px;
  background: transparent;
  color: #3f4550;
  font-size: 12.5px;
  line-height: 1;
  cursor: pointer;
  transition: background 0.15s;
}
.ws__trigger:hover,
.ws__trigger--open {
  background: #eef0f3;
}
/* 文件夹图标:补上「这是本地目录」这条信息,纯文字看不出它是什么 */
.ws__folder {
  width: 13px;
  height: 13px;
  flex-shrink: 0;
  color: #8a919c;
}
.ws__name {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-align: left;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.ws__chevron {
  width: 11px;
  height: 11px;
  flex-shrink: 0;
  color: #9ca3af;
  transition: transform 0.15s;
}
.ws__chevron--open {
  transform: rotate(180deg);
}

/* 向下弹：触发按钮在侧边栏顶部，上方没有空间。
   最小 264px —— 侧边栏拖到最窄时也要放得下两行名字 + 路径。 */
.ws__panel {
  position: absolute;
  top: calc(100% + 8px);
  left: 0;
  z-index: 30;
  width: max(100%, 264px);
  padding: 8px 6px 6px;
  border-radius: 12px;
  background: #fff;
  box-shadow: 0 8px 28px rgba(20, 26, 40, 0.14), 0 0 0 1px rgba(20, 26, 40, 0.06);
  user-select: none;
}
.ws__panel-title {
  padding: 2px 8px 7px;
  color: #9ca3af;
  font-size: 11px;
  line-height: 1;
}

/* 约 5 行高度；再多靠滚动，滚到底自动取下一页 */
.ws__list {
  max-height: 264px;
  overflow-y: auto;
}
.ws__item {
  display: flex;
  align-items: center;
  gap: 8px;
  width: 100%;
  padding: 6px 8px;
  border: none;
  border-radius: 8px;
  background: transparent;
  text-align: left;
  cursor: pointer;
  transition: background 0.15s;
}
.ws__item:hover:not(:disabled) {
  background: #f5f6f8;
}
.ws__item:disabled {
  cursor: not-allowed;
}
.ws__item-main {
  display: flex;
  flex: 1;
  min-width: 0;
  flex-direction: column;
  gap: 2px;
}
.ws__item-name {
  overflow: hidden;
  color: #4a505b;
  font-size: 13px;
  line-height: 1.3;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.ws__item-path {
  overflow: hidden;
  color: #a2a9b5;
  font-size: 11px;
  line-height: 1.2;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.ws__item--active {
  background: #f2f6ff;
}
.ws__item--active:hover:not(:disabled) {
  background: #eaf1ff;
}
.ws__item--active .ws__item-name {
  color: #3b82f6;
  font-weight: 600;
}
.ws__check {
  width: 13px;
  height: 13px;
  flex-shrink: 0;
  color: #3b82f6;
}

.ws__hint {
  padding: 8px;
  color: #a2a9b5;
  font-size: 11px;
  text-align: center;
}

.ws__browse {
  display: flex;
  align-items: center;
  gap: 6px;
  width: 100%;
  margin-top: 4px;
  padding: 8px;
  border: none;
  border-top: 1px solid #f0f1f3;
  border-radius: 0 0 8px 8px;
  background: transparent;
  color: #6b7280;
  font-size: 12px;
  text-align: left;
  cursor: pointer;
  transition: background 0.15s, color 0.15s;
}
.ws__browse:hover:not(:disabled) {
  background: #f5f6f8;
  color: #3f4550;
}
.ws__browse:disabled {
  color: #b6bcc6;
  cursor: not-allowed;
}
.ws__plus {
  width: 11px;
  height: 11px;
  flex-shrink: 0;
}

@media (prefers-reduced-motion: reduce) {
  .ws__trigger,
  .ws__chevron,
  .ws__item,
  .ws__browse {
    transition: none;
  }
}
</style>
