<script setup lang="ts">
import { nextTick, ref, type ComponentPublicInstance } from 'vue'
import { ElMessage, ElMessageBox } from 'element-plus'
import { useChat } from '../composables/chat'
import { useSessions } from '../composables/sessions'

/**
 * 侧边栏的会话列表。
 *
 * 主体只做两件事:告诉用户有哪些对话、点一条打开它。不排序、不分组、不在本地新增——
 * 顺序由后端按 `updated_at` 决定,标题是建行时按首条输入截的。
 *
 * 顶部的「对话」标签是**分组标题**而非列表首项:它在滚动区之外,往上翻历史时留在原位。
 * 没有它时「新对话」按钮与会话条目直接相接,两者一个是操作、一个是导航,视觉上混成一列。
 *
 * 每行的改名 / 删除在 hover 时才出现:它们是低频的维护动作,常驻会盖住标题,
 * 而侧边栏只有 200px 宽。改名走就地编辑(不弹窗),删除必须二次确认。
 *
 * 流式回复期间整列置灰:那一轮持着上下文锁,点会话会卡在锁上直到它跑完。
 */
const { sessions, loaded, open, rename, remove } = useSessions()
const { currentSessionId, isStreaming } = useChat()

/** 正在改名的那一行;null = 没有在编辑 */
const editingId = ref<string | null>(null)
const draft = ref('')
const inputEl = ref<HTMLInputElement | null>(null)
/** 改名 / 删除进行中:挡住回车重复提交与连点 */
const busy = ref(false)

// 输入框是 v-if 出来的,按下标拿不准(v-for 里的 ref 会变成数组),用函数 ref 直接抓元素
function bindInput(el: Element | ComponentPublicInstance | null): void {
  inputEl.value = el instanceof HTMLInputElement ? el : null
}

async function startRename(id: string, title: string): Promise<void> {
  if (isStreaming.value || busy.value) return
  editingId.value = id
  draft.value = title
  // 等这一帧渲染完再聚焦:此刻输入框还没进 DOM
  await nextTick()
  inputEl.value?.focus()
  inputEl.value?.select()
}

function cancelRename(): void {
  editingId.value = null
  draft.value = ''
}

/**
 * 提交改名。三种情况直接收场,不打扰后端:
 * 空白(后端也会拒,没必要跑一趟再弹错)、没改、正在提交。
 */
async function commitRename(): Promise<void> {
  const id = editingId.value
  if (!id || busy.value) return
  const title = draft.value.trim()
  const origin = sessions.value.find((s) => s.id === id)?.title ?? ''
  if (!title || title === origin) {
    cancelRename()
    return
  }

  busy.value = true
  try {
    await rename(id, title)
    // 只在「还是同一行在编辑」时收场:提交期间用户可能已经去点别的行了,
    // 无条件 cancel 会把那一行刚打开的编辑框关掉
    if (editingId.value === id) cancelRename()
  } catch (e) {
    // 失败保持编辑态:刚打的字还在,用户改掉重试比从头再打一遍省事
    ElMessage.error(`重命名失败：${String(e)}`)
    inputEl.value?.focus()
  } finally {
    busy.value = false
  }
}

async function onDelete(id: string, title: string): Promise<void> {
  if (isStreaming.value || busy.value) return
  try {
    await ElMessageBox.confirm(`「${title}」的消息会一起删除，且无法恢复。`, '删除这个对话？', {
      confirmButtonText: '删除',
      cancelButtonText: '取消',
      type: 'warning',
      confirmButtonClass: 'el-button--danger',
    })
  } catch {
    return // 用户取消,不是错误
  }

  busy.value = true
  try {
    await remove(id)
  } catch (e) {
    ElMessage.error(`删除失败：${String(e)}`)
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <div class="sessions">
    <!-- 分组标题固定在滚动区外:它是「这一列是什么」的标识,不该随列表滚走 -->
    <div class="sessions__label">对话</div>

    <div class="sessions__scroll">
      <div v-if="loaded && sessions.length === 0" class="sessions__empty">暂无记录</div>

      <div
        v-for="s in sessions"
        :key="s.id"
        class="sessions__row"
        :class="{ 'sessions__row--active': s.id === currentSessionId }"
      >
        <!-- 改名时就地换成一个输入框:比再弹一层对话框少一次遮挡 -->
        <input
          v-if="editingId === s.id"
          :ref="bindInput"
          v-model="draft"
          class="sessions__input"
          type="text"
          maxlength="40"
          :disabled="busy"
          @keydown.enter.prevent="commitRename"
          @keydown.esc.prevent="cancelRename"
          @blur="commitRename"
        />
        <button
          v-else
          class="sessions__title"
          type="button"
          :disabled="isStreaming"
          :title="s.title"
          @click="open(s.id)"
        >
          {{ s.title }}
        </button>

        <!-- 编辑中不显示:两个按钮会跟输入框抢同一行宽度 -->
        <div v-if="editingId !== s.id" class="sessions__actions">
          <button
            class="sessions__action"
            type="button"
            :disabled="isStreaming || busy"
            title="重命名"
            aria-label="重命名"
            @click.stop="startRename(s.id, s.title)"
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
            class="sessions__action"
            type="button"
            :disabled="isStreaming || busy"
            title="删除"
            aria-label="删除"
            @click.stop="onDelete(s.id, s.title)"
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
    </div>
  </div>
</template>

<style scoped>
/* 占满「新对话」与侧边栏底部之间的空间:标题固定、列表自己滚。
   min-height: 0 不能省:flex 子项的默认最小高度是内容高度,不加就撑破侧边栏。
   滚动交给内层 —— 外层一旦也有 overflow,标题会跟着列表一起滚走。 */
.sessions {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
  margin-top: 14px;
}

/* 分组标题:字号压到 11px、用最浅的一档灰,只做「这列是什么」的说明,
   不与可点击的会话标题争视觉权重。left padding 与条目文字对齐。 */
.sessions__label {
  flex-shrink: 0;
  padding: 0 8px;
  margin-bottom: 6px;
  color: #a2a9b5;
  font-size: 11px;
  letter-spacing: 0.4px;
  user-select: none;
}

.sessions__scroll {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
}

/* 一行 = 标题 + hover 才出现的两个图标。整行负责 hover 底色,
   两个子元素都透明背景,免得叠出两块不同深浅的灰。 */
.sessions__row {
  --row-bg: transparent;
  position: relative;
  display: flex;
  align-items: center;
  border-radius: 8px;
  transition: background 0.15s;
}

.sessions__row:hover {
  background: #f5f6f8;
  --row-bg: #f5f6f8;
}

.sessions__row--active {
  background: #f2f6ff;
  --row-bg: #f2f6ff;
}

.sessions__row--active:hover {
  background: #eaf1ff;
  --row-bg: #eaf1ff;
}

.sessions__title {
  display: block;
  flex: 1;
  min-width: 0;
  padding: 6px 8px;
  border: none;
  border-radius: 8px;
  background: transparent;
  color: #4a505b;
  font: inherit;
  font-size: 13px;
  line-height: 1.4;
  text-align: left;
  /* 标题已在后端按字符截到 10 个,这里再省略是给用户改名后的长标题兜底 */
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  cursor: pointer;
  transition: color 0.15s;
}

.sessions__row--active .sessions__title {
  color: #3b82f6;
  font-weight: 500;
}

.sessions__title:disabled {
  color: #b6bcc6;
  cursor: not-allowed;
}

/* 浮在标题右侧而不是占位:常驻会把每条标题都挤掉十几个像素,
   而这两个动作平时根本用不到。底色跟随整行,标题从底下穿过时看不出接缝。 */
.sessions__actions {
  position: absolute;
  right: 2px;
  top: 50%;
  display: none;
  gap: 1px;
  padding-left: 10px;
  transform: translateY(-50%);
  background: var(--row-bg);
}

.sessions__row:hover .sessions__actions {
  display: flex;
}

.sessions__action {
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

.sessions__action svg {
  width: 13px;
  height: 13px;
}

.sessions__action:hover:not(:disabled) {
  background: rgba(20, 26, 40, 0.07);
  color: #3f4550;
}

/* 最后一个图标悬停是「删除」,给它一点危险色,但仍不喧哗 */
.sessions__action:last-child:hover:not(:disabled) {
  color: #d94b4b;
}

.sessions__action:disabled {
  color: #ccd1d9;
  cursor: not-allowed;
}

/* 就地改名的输入框:字体字号与标题一致,只把容器换成一根细边框,
   编辑时不会出现「字突然变大变小」的跳变 */
.sessions__input {
  flex: 1;
  min-width: 0;
  margin: 3px 0;
  padding: 3px 7px;
  border: 1px solid #3b82f6;
  border-radius: 6px;
  background: #fff;
  color: #3f4550;
  font: inherit;
  font-size: 13px;
  line-height: 1.4;
  outline: none;
}

.sessions__input:disabled {
  background: #f7f8fa;
  color: #8a919c;
}

.sessions__empty {
  padding: 8px;
  color: #a2a9b5;
  font-size: 11px;
  text-align: center;
}

@media (prefers-reduced-motion: reduce) {
  .sessions__row,
  .sessions__title,
  .sessions__action {
    transition: none;
  }
}
</style>
