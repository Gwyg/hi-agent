<script setup lang="ts">
import { computed } from 'vue'
import type { AskDecision } from '../api/llm'
import type { AskPayload } from '../api/types'

// 工具确认弹窗:纯展示 + 三选一,不持有任何状态(由 useChat 的 pendingAsk 驱动)。
// 后端等不到回答会超时关窗并推 agent-ask-closed,所以这里不做倒计时。
const props = defineProps<{ ask: AskPayload | null }>()
const emit = defineEmits<{ answer: [decision: AskDecision] }>()

const visible = computed<boolean>(() => props.ask !== null)

// args 是模型原样给的 JSON 字符串:解析失败就整体当文本显示,不做假设
const args = computed<Record<string, unknown> | null>(() => {
  const raw = props.ask?.args ?? ''
  try {
    const v: unknown = JSON.parse(raw)
    return v !== null && typeof v === 'object' ? (v as Record<string, unknown>) : null
  } catch {
    return null
  }
})

// 标题:模型自述的一句话,**仅供展示**。缺失时回退工具名(老对话里的调用没有这个字段)
const title = computed<string>(() => {
  const d = args.value?.description
  if (typeof d === 'string' && d.trim()) return d.trim()
  return props.ask?.name ?? '工具调用'
})

// 正文:bash 直接给命令原文——用户要确认的是它;其他工具给格式化参数。
// description 只占标题位,真实执行内容永远在正文里,不能被折叠或替换
const body = computed<string>(() => {
  const a = args.value
  if (!a) return props.ask?.args ?? ''
  if (typeof a.command === 'string') return a.command
  return JSON.stringify(a, null, 2)
})

// 命令在哪个目录跑也是执行内容的一部分,指定了才提示
const cwd = computed<string>(() => {
  const c = args.value?.cwd
  return typeof c === 'string' ? c : ''
})

// 标题就是工具名时不再重复一行
const showToolName = computed<boolean>(() => title.value !== props.ask?.name)

// 用户点 X / 按 ESC:当作拒绝,不留悬空请求。
// 用 before-close 而不是 close——后者在程序化关闭(已答完)时也会触发
function onBeforeClose(done: () => void): void {
  emit('answer', 'deny')
  done()
}
</script>

<template>
  <el-dialog
    :model-value="visible"
    title="需要确认"
    width="560px"
    :close-on-click-modal="false"
    :before-close="onBeforeClose"
  >
    <div v-if="ask" class="ask">
      <p class="ask__title">{{ title }}</p>
      <p v-if="showToolName || cwd" class="ask__meta">
        <code v-if="showToolName">{{ ask.name }}</code>
        <span v-if="cwd">工作目录 {{ cwd }}</span>
      </p>
      <pre class="ask__body">{{ body }}</pre>
      <p v-if="ask.rememberable" class="ask__hint">
        「总是允许」会在本次会话内记住
        <code>{{ ask.keys.join('、') }}</code>
        ，之后同类操作不再询问。
      </p>
      <p v-else class="ask__hint">这类操作无法记住，每次都会询问。</p>
    </div>
    <template #footer>
      <el-button @click="emit('answer', 'deny')">拒绝</el-button>
      <el-button v-if="ask?.rememberable" @click="emit('answer', 'always')">总是允许</el-button>
      <el-button type="primary" @click="emit('answer', 'allow')">允许一次</el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
/* 标题是模型写的一句话,可能偏长:限两行,别把弹窗顶开 */
.ask__title {
  margin: 0 0 6px;
  font-size: 15px;
  font-weight: 600;
  line-height: 1.5;
  color: var(--el-text-color-primary);
  display: -webkit-box;
  -webkit-line-clamp: 2;
  -webkit-box-orient: vertical;
  overflow: hidden;
}

.ask__meta {
  display: flex;
  flex-wrap: wrap;
  gap: 10px;
  margin: 0 0 12px;
  font-size: 12px;
  color: var(--el-text-color-secondary);
}

.ask__meta code {
  padding: 1px 6px;
  border-radius: 4px;
  background: #f1f3f5;
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  font-size: 12px;
}

/* 真实执行内容:限高滚动,不撑破弹窗,但绝不折叠隐藏 */
.ask__body {
  max-height: 260px;
  overflow: auto;
  margin: 0 0 12px;
  padding: 12px 14px;
  border-radius: 8px;
  background: #f6f8fa;
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  font-size: 12.5px;
  line-height: 1.55;
  white-space: pre-wrap;
  word-break: break-all;
}

.ask__hint {
  margin: 0;
  font-size: 12px;
  line-height: 1.6;
  color: var(--el-text-color-secondary);
}

.ask__hint code {
  padding: 1px 4px;
  border-radius: 3px;
  background: #f1f3f5;
}
</style>
