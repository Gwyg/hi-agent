<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref, type ComponentPublicInstance } from 'vue'
import { ElMessage, ElMessageBox } from 'element-plus'
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
 * - 每行 hover 才出现改名 / 删除：低频维护动作，常驻会跟名字、路径抢宽度。
 * - 改名只改**显示名**，不动路径；删除只删**记录**，不动磁盘目录——
 *   用户在这里做的是「不再列出来」，不是「删掉这个项目」。
 * - 目录被删 / 盘没插的行照样列出（用户可能只是暂时没插盘），
 *   真选中了由后端 `switch` 报错，这里原样提示。
 */
const PAGE = 20

const {
  currentWorkspace,
  workspaceName,
  switchTo,
  selectWorkspace,
  listWorkspaces,
  renameWorkspace,
  removeWorkspace,
} = useWorkspace()
const { isStreaming } = useChat()

const rootEl = ref<HTMLElement | null>(null)
const listEl = ref<HTMLElement | null>(null)
const open = ref(false)
const items = ref<WorkspaceView[]>([])
const loading = ref(false)
const done = ref(false)
const saving = ref(false)

/** 正在改名的那一行；null = 没有在编辑 */
const editingId = ref<string | null>(null)
const draft = ref('')
const inputEl = ref<HTMLInputElement | null>(null)

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
  cancelRename()
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
  // 正在改名时整行都是「编辑区域」，点它不该触发切换
  if (editingId.value) return
  if (saving.value || isStreaming.value) return
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

// —— 改名 / 删除 ——

// 输入框是 v-if 出来的，v-for 里的 ref 会变成数组，用函数 ref 直接抓元素
function bindInput(el: Element | ComponentPublicInstance | null): void {
  inputEl.value = el instanceof HTMLInputElement ? el : null
}

async function startRename(ws: WorkspaceView): Promise<void> {
  if (saving.value || isStreaming.value) return
  editingId.value = ws.id
  draft.value = ws.name
  await nextTick()
  inputEl.value?.focus()
  inputEl.value?.select()
}

function cancelRename(): void {
  editingId.value = null
  draft.value = ''
}

/** 提交改名：空白与未改动都直接收场，交给后端只会多一次往返和一条错误提示 */
async function commitRename(ws: WorkspaceView): Promise<void> {
  if (editingId.value !== ws.id || saving.value) return
  const name = draft.value.trim()
  if (!name || name === ws.name) {
    cancelRename()
    return
  }

  saving.value = true
  try {
    await renameWorkspace(ws.id, name)
    // 就地更新，不重拉整页：重拉会把列表按 last_opened_at 重排，用户眼前的东西会跳
    ws.name = name
    // 只在还是这一行在编辑时收场，别把用户刚点开的另一行关掉
    if (editingId.value === ws.id) cancelRename()
  } catch (e) {
    ElMessage.error(`重命名工作区失败：${String(e)}`)
    inputEl.value?.focus()
  } finally {
    saving.value = false
  }
}

async function onDelete(ws: WorkspaceView): Promise<void> {
  if (saving.value || isStreaming.value) return
  const isCurrent = ws.path === currentWorkspace.value
  try {
    await ElMessageBox.confirm(
      isCurrent
        ? '这是当前工作区，删除后会自动切到最近使用的另一个工作区（没有就回到默认目录），该目录下的对话记录也会一并删除。磁盘上的文件夹不会被删除。'
        : `「${ws.name}」下的对话记录会一并删除。磁盘上的文件夹不会被删除。`,
      '删除这个工作区？',
      {
        confirmButtonText: '删除',
        cancelButtonText: '取消',
        type: 'warning',
        confirmButtonClass: 'el-button--danger',
      }
    )
  } catch {
    return // 用户取消，不是错误
  }

  saving.value = true
  try {
    await removeWorkspace(ws.id)
    items.value = items.value.filter((x) => x.id !== ws.id)
    // 删的就是当前工作区：面板里已经没有「当前」可言，直接收起
    if (isCurrent) close()
  } catch (e) {
    ElMessage.error(`删除工作区失败：${String(e)}`)
  } finally {
    saving.value = false
  }
}

function onDocPointerDown(e: PointerEvent): void {
  if (open.value && rootEl.value && !rootEl.value.contains(e.target as Node)) close()
}

function onKeydown(e: KeyboardEvent): void {
  if (e.key !== 'Escape') return
  // 改名中的 Esc 只退出编辑，不关面板——一次按键只做一件事
  if (editingId.value) {
    cancelRename()
    return
  }
  close()
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
        <!-- 用 div 而不是 button：行内要放输入框和两个按钮，button 不能嵌套 button -->
        <div
          v-for="ws in items"
          :key="ws.id"
          class="ws__item"
          :class="{
            'ws__item--active': ws.path === currentWorkspace,
            'ws__item--disabled': saving || isStreaming,
          }"
          role="menuitemradio"
          :aria-checked="ws.path === currentWorkspace"
          :tabindex="saving || isStreaming ? -1 : 0"
          @click="choose(ws)"
          @keydown.enter.prevent="choose(ws)"
          @keydown.space.prevent="choose(ws)"
        >
          <span class="ws__item-main">
            <input
              v-if="editingId === ws.id"
              :ref="bindInput"
              v-model="draft"
              class="ws__item-input"
              type="text"
              maxlength="40"
              :disabled="saving"
              @click.stop
              @keydown.enter.prevent.stop="commitRename(ws)"
              @keydown.esc.prevent.stop="cancelRename"
              @keydown.space.stop
              @blur="commitRename(ws)"
            />
            <span v-else class="ws__item-name">{{ ws.name }}</span>
            <span class="ws__item-path">{{ ws.path }}</span>
          </span>

          <svg
            v-if="ws.path === currentWorkspace && editingId !== ws.id"
            class="ws__check"
            viewBox="0 0 16 16"
            aria-hidden="true"
          >
            <path
              d="M3 8.4 L6.4 11.6 L13 4.8"
              fill="none"
              stroke="currentColor"
              stroke-width="1.9"
              stroke-linecap="round"
              stroke-linejoin="round"
            />
          </svg>

          <div v-if="editingId !== ws.id" class="ws__actions">
            <button
              class="ws__action"
              type="button"
              :disabled="saving || isStreaming"
              title="重命名"
              aria-label="重命名"
              @click.stop="startRename(ws)"
            >
              <svg viewBox="0 0 16 16" aria-hidden="true">
                <path
                  d="M10.6 2.9 L13.1 5.4 L5.6 12.9 L2.6 13.4 L3.1 10.4 Z"
                  fill="none"
                  stroke="currentColor"
                  stroke-width="1.3"
                  stroke-linejoin="round"
                />
              </svg>
            </button>
            <button
              class="ws__action"
              type="button"
              :disabled="saving || isStreaming"
              title="删除"
              aria-label="删除"
              @click.stop="onDelete(ws)"
            >
              <svg viewBox="0 0 16 16" aria-hidden="true">
                <path
                  d="M3.4 4.8 H12.6 M6.4 4.8 V3.4 H9.6 V4.8 M4.9 4.8 L5.5 12.6 H10.5 L11.1 4.8"
                  fill="none"
                  stroke="currentColor"
                  stroke-width="1.3"
                  stroke-linecap="round"
                  stroke-linejoin="round"
                />
              </svg>
            </button>
          </div>
        </div>

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
/* 行本身不再透明切换:图标要盖在名字右侧,底色必须由这一层给,
   否则浮层底色与行底色不同,悬停时会出现一块明显的方块 */
.ws__item {
  --row-bg: transparent;
  position: relative;
  display: flex;
  align-items: center;
  gap: 8px;
  width: 100%;
  padding: 6px 8px;
  border-radius: 8px;
  text-align: left;
  cursor: pointer;
  transition: background 0.15s;
}
.ws__item:hover {
  background: #f5f6f8;
  --row-bg: #f5f6f8;
}
.ws__item--disabled {
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
  --row-bg: #f2f6ff;
}
.ws__item--active:hover {
  background: #eaf1ff;
  --row-bg: #eaf1ff;
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
/* 悬停时让位给两个操作图标:同占右侧,一起出现会互相压住 */
.ws__item:hover .ws__check {
  visibility: hidden;
}

/* 就地改名:字号与名字一致,高度压住两行(名字 + 路径)的整体,
   编辑时不改变行高,列表不会抖一下 */
.ws__item-input {
  width: 100%;
  margin: -1px 0;
  padding: 1px 5px;
  border: 1px solid #3b82f6;
  border-radius: 5px;
  background: #fff;
  color: #3f4550;
  font: inherit;
  font-size: 13px;
  line-height: 1.3;
  outline: none;
}
.ws__item-input:disabled {
  background: #f7f8fa;
  color: #8a919c;
}

.ws__actions {
  position: absolute;
  right: 6px;
  top: 50%;
  display: none;
  gap: 1px;
  padding-left: 10px;
  transform: translateY(-50%);
  background: var(--row-bg);
}
.ws__item:hover .ws__actions {
  display: flex;
}
.ws__action {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 20px;
  height: 20px;
  padding: 0;
  border: none;
  border-radius: 5px;
  background: transparent;
  color: #8a919c;
  cursor: pointer;
  transition: background 0.15s, color 0.15s;
}
.ws__action svg {
  width: 13px;
  height: 13px;
}
.ws__action:hover:not(:disabled) {
  background: rgba(20, 26, 40, 0.07);
  color: #3f4550;
}
.ws__action:last-child:hover:not(:disabled) {
  color: #d94b4b;
}
.ws__action:disabled {
  color: #ccd1d9;
  cursor: not-allowed;
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
  .ws__action,
  .ws__browse {
    transition: none;
  }
}
</style>
