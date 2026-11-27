<script setup lang="ts">
import { computed, nextTick, ref, watch } from 'vue'

import type { Block, ChatMessage } from '../composables/chat'
import AgentTrail from './AgentTrail.vue'
import BlockList from './BlockList.vue'

const props = defineProps<{ message: ChatMessage; name: string }>()

const trailEl = ref<HTMLElement | null>(null)

const hasTool = computed(() => props.message.blocks.some((b) => b.kind === 'tool'))
const folded = computed(() => Boolean(props.message.done) && hasTool.value)

/**
 * 分区:结束时**最后一个文本块是结论**,它之前的一切(旁白 + 工具)进折叠带。
 *
 * 这一条成立的前提是后端的产出顺序固定(文本在前、工具在后),所以不需要轮次字段,
 * 也不需要判断「哪句是旁白」——旁白在折叠后天然沉到过程区里。
 * 没调用过工具则整条平铺,否则每句普通问答都会多出一条「已完成」。
 */
const split = computed<{ head: Block[]; tail: Block[] }>(() => {
  const blocks = props.message.blocks
  if (!folded.value) return { head: [], tail: blocks }
  let lastText = -1
  for (let i = blocks.length - 1; i >= 0; i--) {
    if (blocks[i].kind === 'text') {
      lastText = i
      break
    }
  }
  // 整轮没有正文(异常路径):全部进折叠带,外面不留空白
  if (lastText < 0) return { head: blocks, tail: [] }
  return { head: blocks.slice(0, lastText), tail: blocks.slice(lastText) }
})

const NEAR_BOTTOM = 80

/**
 * 折叠会让上方内容高度骤减:用户不在底部时,视口里的东西会整体跳走。
 *
 * 折叠前(pre flush,DOM 还是旧结构)量出「过程区高度」——也就是结论块相对
 * 过程区顶部的偏移;折叠后再量折叠条高度,差值就是要补的滚动距离。
 * 只在视口整个落在过程区下方时才补(用户在读结论或更后);与过程区相交时
 * 保持原位,否则会把正在读的东西强行抽走。
 * 在底部时什么都不做,交给 ChatView 的跟随逻辑,两边不同时改 scrollTop。
 */
watch(
  () => props.message.done,
  async (now, before) => {
    if (!now || before) return
    const trail = trailEl.value
    const scroller = trail?.closest('.chat__messages') as HTMLElement | null
    if (!trail || !scroller || split.value.tail.length === 0) return

    const rect = trail.getBoundingClientRect()
    const scroll = scroller.scrollTop
    const trailTop = rect.top - scroller.getBoundingClientRect().top + scroll
    const last = trail.lastElementChild as HTMLElement | null
    const processH =
      last && split.value.tail.length > 0 ? last.getBoundingClientRect().top - rect.top : trail.offsetHeight
    const nearBottom = scroller.scrollHeight - scroll - scroller.clientHeight <= NEAR_BOTTOM

    await nextTick()

    if (nearBottom) return
    const d = processH - trail.offsetHeight
    if (d > 0 && scroll >= trailTop + processH) scroller.scrollTop = scroll - d
  }
)
</script>

<template>
  <div class="msg msg--agent">
    <div class="msg__head">
      <div class="msg__avatar">hi</div>
      <span class="msg__name">{{ name }}</span>
    </div>

    <div class="msg__body">
      <!-- 过程区容器:流式阶段与折叠后都是同一个节点,折叠测量靠它定位 -->
      <div v-if="hasTool" ref="trailEl" class="msg__trail">
        <AgentTrail v-if="folded" :blocks="split.head" :elapsed="message.elapsed" />
        <BlockList v-else :blocks="message.blocks" :done="message.done" />
      </div>
      <BlockList v-if="folded" :blocks="split.tail" :done="message.done" />
      <!-- 没调过工具时 folded 恒为 false,由这一支平铺全部内容(含思考块) -->
      <BlockList v-else-if="!hasTool" :blocks="message.blocks" :done="message.done" />
    </div>
  </div>
</template>

<style scoped>
.msg--agent {
  display: block;
}

.msg__head {
  display: flex;
  align-items: center;
  gap: 10px;
  margin-bottom: 6px;
}

.msg__avatar {
  width: 30px;
  height: 30px;
  border-radius: 50%;
  background: #eef0f2;
  color: #1f2329;
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 12px;
  font-weight: 500;
  line-height: 1;
  letter-spacing: 0.2px;
}

.msg__name {
  font-size: 13px;
  font-weight: 500;
  color: #1f2329;
}

.msg__body {
  padding-left: 40px;
  font-size: 14px;
  line-height: 1.7;
  color: #1f2329;
  word-break: break-word;
}

/* 建立 BFC:内部块的 margin 不溢出到容器外。
   折叠补偿靠这个容器的高度差,若 margin 折叠出去,量到的值会比视觉占位小 */
.msg__trail {
  display: flow-root;
}
</style>
