<script setup lang="ts">
import { computed, ref } from 'vue'
import { ElMessage } from 'element-plus'
import { useLlmConfig } from '../composables/llmConfig'

// 大模型配置弹窗。本身不渲染任何可见入口,由调用方通过 ref.open() 唤起,
// 便于「未配置横幅」与「设置弹窗」共用同一份表单与保存逻辑。
// 专做 DeepSeek 后端点写死、模型选择挪到输入框工具条,这里只剩一个 API Key。
const { llmConfig, saveLlmConfig } = useLlmConfig()

interface LlmForm {
  apiKey: string
}

const visible = ref(false)
const saving = ref(false)
const form = ref<LlmForm>({ apiKey: '' })

// api_key 永不回显明文:已保存过时只显示掩码,留空即"不修改"
const keyPlaceholder = computed<string>(() =>
  llmConfig.value.api_key_set
    ? `已保存（${llmConfig.value.api_key_masked ?? ''}），留空则不修改`
    : '请输入 API Key'
)

function open(): void {
  form.value = { apiKey: '' }
  visible.value = true
}

async function submit(): Promise<void> {
  saving.value = true
  try {
    // 只传密钥:后端会沿用当前生效模型,不会把输入框里选过的模型删掉
    await saveLlmConfig(form.value)
    visible.value = false
    ElMessage.success('大模型配置已保存')
  } catch (e) {
    ElMessage.error(String(e))
  } finally {
    saving.value = false
  }
}

defineExpose({ open })
</script>

<template>
  <el-dialog v-model="visible" title="配置大模型" width="480px" :close-on-click-modal="false">
    <el-form label-width="86px" @submit.prevent>
      <el-form-item label="API Key">
        <el-input
          v-model="form.apiKey"
          type="password"
          show-password
          :placeholder="keyPlaceholder"
        />
      </el-form-item>
    </el-form>
    <template #footer>
      <el-button @click="visible = false">取消</el-button>
      <el-button type="primary" :loading="saving" @click="submit">保存</el-button>
    </template>
  </el-dialog>
</template>
