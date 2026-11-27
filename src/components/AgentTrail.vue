<script setup lang="ts">
import { computed, ref } from 'vue'
import { ArrowRight } from '@element-plus/icons-vue'

import type { Block } from '../composables/chat'
import BlockList from './BlockList.vue'

/** 折叠带:一层,不再嵌套。展开后就是按发生顺序排列的旁白 + 工具卡片 */
const props = defineProps<{
  blocks: Block[]
  elapsed?: number
}>()

const tools = computed(() => props.blocks.filter((b) => b.kind === 'tool'))
// 思考块也沉在过程区里:摘要里报一句段数,让用户知道里面不只有工具卡片
const thoughts = computed(() => props.blocks.filter((b) => b.kind === 'thinking').length)
const failed = computed(() => tools.value.filter((b) => b.status === 'error').length)
const hasError = computed(() => failed.value > 0)

// 失败是必须一眼看到的信息:默认展开,不靠用户去点
const open = ref(hasError.value)

const elapsedText = computed<string>(() => {
  if (props.elapsed === undefined) return ''
  const s = Math.round(props.elapsed / 1000)
  return s >= 1 ? `${s}s` : ''
})

const summary = computed<string>(() => {
  const parts = ['已完成', `${tools.value.length} 个操作`]
  if (thoughts.value > 0) parts.push(`含 ${thoughts.value} 段思考`)
  if (hasError.value) parts.push(`${failed.value} 个失败`)
  if (elapsedText.value) parts.push(elapsedText.value)
  return parts.join(' · ')
})
</script>

<template>
  <div class="trail">
    <button class="trail__head" type="button" @click="open = !open">
      <el-icon class="trail__chevron" :class="{ 'trail__chevron--open': open }">
        <ArrowRight />
      </el-icon>
      <span class="trail__summary" :class="{ 'trail__summary--error': hasError }">{{ summary }}</span>
    </button>

    <div v-show="open" class="trail__body">
      <!-- 折叠带只在整轮结束后出现,思考块一进来就应是收起态 -->
      <BlockList :blocks="blocks" muted done />
    </div>
  </div>
</template>

<style scoped>
.trail {
  margin: 4px 0 10px;
}

.trail__head {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  padding: 3px 8px 3px 4px;
  border: none;
  border-radius: 6px;
  background: transparent;
  font: inherit;
  cursor: pointer;
  color: #6b7280;
}

.trail__head:hover {
  background: rgba(0, 0, 0, 0.035);
}

.trail__chevron {
  font-size: 11px;
  transition: transform 0.15s;
}

.trail__chevron--open {
  transform: rotate(90deg);
}

.trail__summary {
  font-size: 12px;
  line-height: 1.4;
}

.trail__summary--error {
  color: #b42318;
}

/* 左侧竖线:展开后一眼看出「这段是过程,不是答案」 */
.trail__body {
  margin: 4px 0 0 8px;
  padding-left: 10px;
  border-left: 1px solid #e8eaed;
}
</style>
