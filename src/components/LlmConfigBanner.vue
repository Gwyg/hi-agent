<script setup lang="ts">
import { ref } from 'vue'
import LlmConfigDialog from './LlmConfigDialog.vue'

// 未配置大模型时的醒目提示横幅。
// 显隐由 App.vue 按 llmConfigured 控制;配置表单复用 LlmConfigDialog(与设置面板同一份)。
const llmDialog = ref<InstanceType<typeof LlmConfigDialog> | null>(null)
</script>

<template>
  <div class="llm-notice">
    <span class="llm-notice__icon">!</span>
    <span class="llm-notice__text">尚未配置大模型，对话功能不可用</span>
    <button class="llm-notice__btn" type="button" @click="llmDialog?.open()">立即配置</button>
  </div>

  <LlmConfigDialog ref="llmDialog" />
</template>

<style scoped>
/* 横幅:常驻窗口顶部,红底警示色,不随内容滚动 */
.llm-notice {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 10px 16px;
  background: #fff1f0;
  border-bottom: 1px solid #ffccc7;
  color: #cf1322;
  font-size: 13px;
  flex-shrink: 0;
}

.llm-notice__icon {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 16px;
  height: 16px;
  border-radius: 50%;
  background: #ff4d4f;
  color: #fff;
  font-size: 11px;
  font-weight: 700;
  flex-shrink: 0;
}

.llm-notice__text {
  flex: 1;
}

.llm-notice__btn {
  flex-shrink: 0;
  background: #ff4d4f;
  color: #fff;
  border: none;
  border-radius: 6px;
  padding: 5px 12px;
  font-size: 12px;
  cursor: pointer;
}

.llm-notice__btn:hover {
  background: #d9363e;
}
</style>
