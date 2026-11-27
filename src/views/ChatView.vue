<script setup lang="ts">
import { ref, nextTick, watch, onMounted, onBeforeUnmount } from 'vue'
import { Promotion, ArrowDown } from '@element-plus/icons-vue'
import { useChat } from '../composables/chat'
import AssistantMessage from '../components/AssistantMessage.vue'
import ModelPicker from '../components/ModelPicker.vue'
import EffortSlider from '../components/EffortSlider.vue'
import ToolConfirmDialog from '../components/ToolConfirmDialog.vue'
import HiTooltip from '../components/HiTooltip.vue'
import '../styles/highlight.css'
import '../styles/markdown.css'

// 产品名（显示在 AI 消息头）
const agentName = 'hi-agent'

// 消息列表与「发送 / 新对话」逻辑由共享 composable 持有(侧边栏的按钮要用同一份状态)
const {
  messages,
  isStreaming,
  chatVersion,
  pendingAsk,
  hasMore,
  loadingOlder,
  sendMessage,
  answerPendingAsk,
  loadOlderMessages,
} = useChat()

// 输入框内容
const inputText = ref('')

// ——— 滚动跟进：跟随 / 脱离 双状态 ———
const scrollEl = ref<HTMLElement | null>(null) // .chat__messages 滚动容器
const autoFollow = ref(true) // 是否在底部、需要自动跟进最新
const showJump = ref(false) // 是否显示"回到底部"按钮
const NEAR_BOTTOM = 80 // 距底部 ≤80px 视为"在底部"

function isNearBottom(el: HTMLElement): boolean {
  return el.scrollHeight - el.scrollTop - el.clientHeight <= NEAR_BOTTOM
}

function scrollToBottom(): void {
  const el = scrollEl.value
  if (el) el.scrollTop = el.scrollHeight
}

// 用户滚动时重新判定状态：在底→跟随并隐藏按钮；离底→脱离并显示按钮
function onScroll(): void {
  const el = scrollEl.value
  if (!el) return
  const near = isNearBottom(el)
  autoFollow.value = near
  showJump.value = !near
}

// 点击按钮：回到底部并恢复跟随
function jumpToBottom(): void {
  autoFollow.value = true
  showJump.value = false
  scrollToBottom()
}

/**
 * 内容指纹:block 结构下没有单一 content 可监听,按每块的「长度 + 结束标记」拼。
 * 只取长度而非全文,长对话逐 token 比较也不会有序列化开销;
 * 工具块取结果长度,这样执行中的输出增长同样能触发跟随。
 */
function contentKey(): string {
  return messages.value
    .map((m) => {
      const blocks = m.blocks
        .map((b) => {
          if (b.kind === 'text') return `t${b.text.length}`
          if (b.kind === 'thinking') return `k${b.text.length}`
          if (b.kind === 'error') return `e${b.text.length}`
          return `o${b.result?.length ?? 0}${b.status ?? ''}`
        })
        .join(',')
      return `${blocks}${m.done ? '!' : ''}`
    })
    .join('\u0000')
}

// 任意内容变化后，若处于跟随状态则滚到底（nextTick 等 DOM 更新完成）
watch(contentKey, async () => {
  await nextTick()
  if (autoFollow.value) scrollToBottom()
})

// 会话被重置(侧边栏「新对话」或切换工作目录)后恢复跟随并贴底
watch(chatVersion, async () => {
  autoFollow.value = true
  await nextTick()
  scrollToBottom()
})

onMounted(() => {
  scrollToBottom()
  scrollEl.value?.addEventListener('scroll', onScroll, { passive: true })
})
onBeforeUnmount(() => {
  scrollEl.value?.removeEventListener('scroll', onScroll)
})

async function send(): Promise<void> {
  const text = inputText.value.trim()
  if (!text || isStreaming.value) return
  inputText.value = ''
  // 用户主动发消息，回到底部跟随
  autoFollow.value = true
  await nextTick()
  scrollToBottom()
  await sendMessage(text)
}

/**
 * 往前翻一页。
 *
 * 新内容插在顶部会把当前位置整体往下推,所以翻完要把**多出来的高度**补进 scrollTop,
 * 用户的眼睛才会停在原来那一行上。不补的话每翻一次就跳一段。
 *
 * 翻页期间关掉跟随:内容变多会触发 `contentKey` 的自动贴底,那会把刚从顶部补好的
 * 位置又冲掉(列表本来就不足一屏时尤其明显)。
 */
async function loadOlder(): Promise<void> {
  const el = scrollEl.value
  if (!el) return
  autoFollow.value = false
  const before = el.scrollHeight
  await loadOlderMessages()
  await nextTick()
  el.scrollTop += el.scrollHeight - before
}
</script>

<template>
  <div class="chat">
    <div class="chat__scroll-wrap">
      <div ref="scrollEl" class="chat__messages">
        <!-- 顶部「加载更早」:只有滚到顶才看得见。不做滚到顶自动加载 ——
             用户正往回读时不断插内容,位置会一直跳 -->
        <div v-if="hasMore" class="chat__older">
          <button class="chat__older-btn" type="button" :disabled="loadingOlder" @click="loadOlder">
            {{ loadingOlder ? '加载中…' : '加载更早的消息' }}
          </button>
        </div>

        <template v-for="(m, i) in messages" :key="i">
          <div v-if="m.role === 'user'" class="msg msg--user">
            <div class="msg__bubble">{{ m.blocks[0]?.kind === 'text' ? m.blocks[0].text : '' }}</div>
          </div>
          <!-- AI 消息:过程(旁白 + 工具)与结论分区由组件内部裁决 -->
          <AssistantMessage v-else :message="m" :name="agentName" />
        </template>
      </div>

      <!-- 脱离底部时浮动显示，点击回到底部并恢复跟随 -->
      <transition name="jump-fade">
        <HiTooltip v-if="showJump" content="回到底部">
          <button class="chat__jump" type="button" @click="jumpToBottom">
            <el-icon><ArrowDown /></el-icon>
          </button>
        </HiTooltip>
      </transition>
    </div>

    <div class="chat__input">
      <el-input
        v-model="inputText"
        type="textarea"
        :autosize="{ minRows: 2, maxRows: 8 }"
        resize="none"
        class="chat__textarea"
        placeholder="输入消息…"
        @keydown.enter.exact.prevent="send"
      />
      <div class="chat__toolbar">
        <ModelPicker />
        <EffortSlider />
        <span class="chat__hint">Enter 发送，Shift + Enter 换行</span>
        <el-button class="chat__send" type="primary" circle :disabled="!inputText.trim() || isStreaming" @click="send">
          <el-icon><Promotion /></el-icon>
        </el-button>
      </div>
    </div>

    <!-- 工具确认:后端阻塞等待回答,超时/取消由后端关窗 -->
    <ToolConfirmDialog :ask="pendingAsk" @answer="answerPendingAsk" />
  </div>
</template>

<style scoped>
.chat {
  height: 100%;
  display: flex;
  flex-direction: column;
}

.chat__scroll-wrap {
  position: relative;
  flex: 1;
  min-height: 0;
}

.chat__messages {
  position: absolute;
  inset: 0;
  overflow: auto;
  /* 关掉浏览器原生滚动锚定:它会和折叠时的手动补偿叠加,导致过冲。
     WebView2 默认开着,WKWebView 又没实现,两边行为还不一致——统一自己算 */
  overflow-anchor: none;
  padding: 16px;
  display: flex;
  flex-direction: column;
  gap: 18px;
}

/* 顶部「加载更早」:居中一颗小胶囊,视觉重量低于任何一条消息 */
.chat__older {
  display: flex;
  justify-content: center;
}

.chat__older-btn {
  padding: 4px 12px;
  border: 1px solid #e8eaed;
  border-radius: 999px;
  background: #fff;
  color: #6b7280;
  font: inherit;
  font-size: 12px;
  line-height: 1.4;
  cursor: pointer;
  transition: background 0.15s, color 0.15s;
}
.chat__older-btn:hover:not(:disabled) {
  background: #f5f6f8;
  color: #3f4550;
}
.chat__older-btn:disabled {
  color: #b6bcc6;
  cursor: default;
}

/* 回到底部按钮：固定在消息区右下角，不随内容滚动 */
.chat__jump {
  position: absolute;
  right: 18px;
  bottom: 18px;
  width: 40px;
  height: 40px;
  padding: 0;
  border-radius: 50%;
  border: 1px solid var(--el-border-color);
  background: #fff;
  color: #1f2329;
  display: flex;
  align-items: center;
  justify-content: center;
  cursor: pointer;
  box-shadow: 0 2px 10px rgba(0, 0, 0, 0.12);
  z-index: 5;
}
.chat__jump:hover {
  background: #f1f3f5;
}

.jump-fade-enter-active,
.jump-fade-leave-active {
  transition: opacity 0.2s, transform 0.2s;
}
.jump-fade-enter-from,
.jump-fade-leave-to {
  opacity: 0;
  transform: translateY(8px);
}

/* 用户消息：靠右的浅灰气泡，无头像 */
.msg--user {
  display: flex;
  justify-content: flex-end;
}

.msg__bubble {
  max-width: 70%;
  padding: 10px 14px;
  border-radius: 14px;
  background: #f1f3f5;
  color: #1f2329;
  font-size: 14px;
  line-height: 1.6;
  word-break: break-word;
  white-space: pre-wrap;
}

/* 输入区：圆角卡片 + 上下两区，无分割线 */
.chat__input {
  display: flex;
  flex-direction: column;
  gap: 8px;
  margin: 12px 16px;
  padding: 12px 14px;
  background: #fff;
  border: 1px solid var(--el-border-color);
  border-radius: 16px;
  box-shadow: 0 2px 12px rgba(0, 0, 0, 0.06);
}

.chat__textarea {
  width: 100%;
}

/* 去掉 el-input 自带边框，让它融入卡片 */
.chat__textarea :deep(.el-textarea__inner) {
  box-shadow: none;
  border: none;
  padding: 2px 4px;
  background: transparent;
  resize: none;
}

/* 底栏：左侧「模型按钮 + 思考强度 + 输入提示」成一组，右侧圆形发送按钮独立；
   32px 行高让两端控件在同一条基线上，提示文字退到次要层级。 */
.chat__toolbar {
  display: flex;
  align-items: center;
  min-height: 32px;
  gap: 10px;
}

.chat__hint {
  flex: 1;
  min-width: 0;
  text-align: left;
  font-size: 11px;
  color: var(--el-text-color-placeholder);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.chat__send {
  flex-shrink: 0;
  --el-button-bg-color: #1f2329;
  --el-button-border-color: #1f2329;
  --el-button-hover-bg-color: #33373d;
  --el-button-hover-border-color: #33373d;
  --el-button-active-bg-color: #111317;
  --el-button-active-border-color: #111317;
  --el-button-disabled-bg-color: #e5e7eb;
  --el-button-disabled-border-color: #e5e7eb;
  --el-button-disabled-text-color: #9ca3af;
}
</style>
