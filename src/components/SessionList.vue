<script setup lang="ts">
import { useChat } from '../composables/chat'
import { useSessions } from '../composables/sessions'

/**
 * 侧边栏的会话列表。
 *
 * 只做两件事:告诉用户有哪些对话、点一条打开它。不排序、不分组、不在本地新增——
 * 顺序由后端按 `updated_at` 决定,标题是建行时按首条输入截的。
 *
 * 流式回复期间整列置灰:那一轮持着上下文锁,点会话会卡在锁上直到它跑完。
 */
const { sessions, loaded, open } = useSessions()
const { currentSessionId, isStreaming } = useChat()
</script>

<template>
  <div class="sessions">
    <div v-if="loaded && sessions.length === 0" class="sessions__empty">还没有对话</div>

    <button
      v-for="s in sessions"
      :key="s.id"
      class="sessions__item"
      :class="{ 'sessions__item--active': s.id === currentSessionId }"
      type="button"
      :disabled="isStreaming"
      :title="s.title"
      @click="open(s.id)"
    >
      {{ s.title }}
    </button>
  </div>
</template>

<style scoped>
/* 占满「新对话」与侧边栏底部之间的空间,自己滚。
   min-height: 0 不能省:flex 子项的默认最小高度是内容高度,不加就撑破侧边栏。 */
.sessions {
  flex: 1;
  min-height: 0;
  margin-top: 14px;
  overflow-y: auto;
}

.sessions__item {
  display: block;
  width: 100%;
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
  transition: background 0.15s, color 0.15s;
}

.sessions__item:hover:not(:disabled) {
  background: #f5f6f8;
}

.sessions__item--active {
  background: #f2f6ff;
  color: #3b82f6;
  font-weight: 500;
}

.sessions__item--active:hover:not(:disabled) {
  background: #eaf1ff;
}

.sessions__item:disabled {
  color: #b6bcc6;
  cursor: not-allowed;
}

.sessions__empty {
  padding: 8px;
  color: #a2a9b5;
  font-size: 11px;
  text-align: center;
}

@media (prefers-reduced-motion: reduce) {
  .sessions__item {
    transition: none;
  }
}
</style>
