<script setup lang="ts">
import type { Block } from '../composables/chat'
import { renderMarkdown } from '../utils/markdown'
import ThoughtBlock from './ThoughtBlock.vue'
import ToolCard from './ToolCard.vue'

/**
 * 顺序渲染一组内容块。没有外层包裹(`template v-for`),所以调用方的容器
 * 直接就是各块的父节点——折叠时靠 `lastElementChild` 定位最后一个块。
 */
defineProps<{
  blocks: Block[]
  /** 旁白样式:小一号、次要色。用于折叠带内部,与最终结论区分 */
  muted?: boolean
  /** 整轮是否结束:交给思考块决定「流式展开 / 结束收起」 */
  done?: boolean
}>()
</script>

<template>
  <template v-for="(b, i) in blocks" :key="i">
    <div
      v-if="b.kind === 'text'"
      class="blk blk--text markdown"
      :class="{ 'blk--muted': muted }"
      v-html="renderMarkdown(b.text)"
    ></div>
    <div v-else-if="b.kind === 'error'" class="blk blk--error">{{ b.text }}</div>
    <ThoughtBlock v-else-if="b.kind === 'thinking'" :text="b.text" :done="done" />
    <ToolCard v-else :block="b" />
  </template>
</template>

<style scoped>
.blk--text {
  font-size: 14px;
  line-height: 1.7;
  color: #1f2329;
  word-break: break-word;
}

/* 旁白:过程叙述,不是答案,弱化一档 */
.blk--muted {
  font-size: 13px;
  color: #6b7280;
}

/* 全局 markdown 的段间距对旁白偏大,收紧一档 */
.blk--muted :deep(p) {
  margin-bottom: 6px;
}

.blk--error {
  margin: 6px 0;
  padding: 8px 12px;
  border-left: 3px solid #e5484d;
  background: #fef2f2;
  border-radius: 0 6px 6px 0;
  font-size: 13px;
  line-height: 1.6;
  color: #b42318;
  white-space: pre-wrap;
  word-break: break-word;
}
</style>
