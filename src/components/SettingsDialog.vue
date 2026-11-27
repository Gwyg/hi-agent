<script setup lang="ts">
import { ref } from 'vue'
import { useLlmConfig } from '../composables/llmConfig'
import LlmConfigDialog from './LlmConfigDialog.vue'

// 设置面板:由标题栏齿轮唤起。本身不渲染入口,暴露 open() 给调用方。
const { llmConfig } = useLlmConfig()

const visible = ref(false)
const llmDialog = ref<InstanceType<typeof LlmConfigDialog> | null>(null)

function open(): void {
  visible.value = true
}

defineExpose({ open })
</script>

<template>
  <el-dialog v-model="visible" title="设置" width="520px">
    <div class="row">
      <div class="row__info">
        <div class="row__title">大模型</div>
        <div class="row__desc">
          {{ llmConfig.model }} ·
          {{ llmConfig.api_key_set ? `密钥已保存（${llmConfig.api_key_masked || ''}）` : '密钥未配置' }}
        </div>
      </div>
      <el-button type="primary" plain @click="llmDialog?.open()">配置</el-button>
    </div>
  </el-dialog>

  <LlmConfigDialog ref="llmDialog" />
</template>

<style scoped>
.row {
  display: flex;
  align-items: center;
  gap: 12px;
}

.row__info {
  flex: 1;
  min-width: 0;
}

.row__title {
  font-size: 14px;
  color: #1f2329;
}

.row__desc {
  margin-top: 2px;
  font-size: 12px;
  color: #999;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
</style>
