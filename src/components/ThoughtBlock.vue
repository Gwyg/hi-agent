<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { ArrowRight } from '@element-plus/icons-vue'

/**
 * 思考块:模型的内部推理。
 *
 * 它既不是答案(不放大字号、不走 markdown),也不是一次操作(不做成工具卡片),
 * 所以用「左侧细竖线 + 灰色小字 + 自身一级折叠」表达「过程资料」这个层级。
 * 流式阶段默认展开让人看得到推理,整轮结束后自动收成一行;用户手动展开过
 * 就不再自动收,避免阅读中被强行折叠。
 */
const props = defineProps<{
  text: string
  /** 整条 assistant 消息是否已结束(llm-done) */
  done?: boolean
}>()

const manual = ref(false)
const expanded = ref(!props.done)

watch(
  () => props.done,
  (now) => {
    if (now && !manual.value) expanded.value = false
  }
)

// 只能叫「字」:前端没做分词,后端也没给思考的 token 数
const count = computed<string>(() => props.text.length.toLocaleString())
const label = computed<string>(() => {
  if (!expanded.value) return `已思考 ${count.value} 字`
  return props.done ? `思考过程 · ${count.value} 字` : `正在思考 · ${count.value} 字`
})

function toggle(): void {
  manual.value = true
  expanded.value = !expanded.value
}
</script>

<template>
  <div class="thought">
    <button class="thought__head" type="button" :aria-expanded="expanded" @click="toggle">
      <el-icon class="thought__chevron" :class="{ 'thought__chevron--open': expanded }">
        <ArrowRight />
      </el-icon>
      <span class="thought__label">{{ label }}</span>
      <span v-if="!expanded" class="thought__hint">点击展开</span>
    </button>

    <!-- 纯文本渲染:推理里常有 #、*、反引号,走 markdown 会被误解析成标题/列表 -->
    <div v-show="expanded" class="thought__body">{{ text }}</div>
  </div>
</template>

<style scoped>
.thought {
  margin: 6px 0 8px;
}

.thought__head {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  padding: 2px 8px 2px 2px;
  border: none;
  border-radius: 6px;
  background: transparent;
  font: inherit;
  color: #9ca3af;
  cursor: pointer;
}

.thought__head:hover {
  background: rgba(0, 0, 0, 0.035);
  color: #6b7280;
}

.thought__chevron {
  font-size: 11px;
  transition: transform 0.15s;
}

.thought__chevron--open {
  transform: rotate(90deg);
}

.thought__label {
  font-size: 12px;
  line-height: 1.4;
  letter-spacing: 0.2px;
}

.thought__hint {
  font-size: 11px;
  color: #b6bcc4;
}

/* 细竖线是它与正文、工具卡片最直观的分界;不设背景块,视觉权重压到最低 */
.thought__body {
  margin: 2px 0 0 5px;
  padding: 4px 0 4px 10px;
  border-left: 2px solid #e8eaed;
  max-height: 220px;
  overflow-y: auto;
  font-family: ui-monospace, SFMono-Regular, Consolas, 'Microsoft YaHei', monospace;
  font-size: 12px;
  line-height: 1.65;
  color: #8a9099;
  white-space: pre-wrap;
  word-break: break-word;
}
</style>
