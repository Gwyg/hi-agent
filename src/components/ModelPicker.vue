<script setup lang="ts">
import { computed, ref } from 'vue'
import { ArrowDown } from '@element-plus/icons-vue'
import { ElMessage } from 'element-plus'
import { useChat } from '../composables/chat'
import { useLlmConfig } from '../composables/llmConfig'
import HiTooltip from './HiTooltip.vue'
import type { ModelOption } from '../api/types'

// 输入框工具条里的模型切换按钮。配置弹窗不再出现模型选择;
// 候选来自官方 /models,点开菜单时才拉(见 ensureModels),选中后落盘并立即对下一轮请求生效。
const { llmConfig, llmConfigured, modelOptions, ensureModels, saveLlmConfig } = useLlmConfig()
const { isStreaming } = useChat()

const loadingModels = ref(false)
const switching = ref(false)
const disabled = computed<boolean>(() => !llmConfigured.value || isStreaming.value)

// 列表拉到之前显示 id,拉到之后显示官方 name
const currentName = computed<string>(
  () =>
    modelOptions.value.find((m) => m.id === llmConfig.value.model)?.name ??
    llmConfig.value.model
)

const disabledReason = computed<string>(() => {
  if (isStreaming.value) return '回复中，结束后可切换模型'
  if (!llmConfigured.value) return '请先填写 API Key'
  return '切换模型'
})

function formatCount(n: number): string {
  if (n >= 1024 * 1024) return `${Math.round(n / (1024 * 1024))}M`
  if (n >= 1024) return `${Math.round(n / 1024)}K`
  return String(n)
}

// 规格是 /models 的展示数据,不驱动任何请求参数
function specOf(m: ModelOption): string {
  const parts: string[] = []
  if (m.context_window !== null) parts.push(`上下文 ${formatCount(m.context_window)}`)
  if (m.max_output_tokens !== null) parts.push(`最大输出 ${formatCount(m.max_output_tokens)}`)
  parts.push(m.input_modalities.includes('image') ? '支持图片' : '仅文本')
  return parts.join(' · ')
}

function onVisibleChange(visible: boolean): void {
  if (!visible || modelOptions.value.length > 0) return
  loadingModels.value = true
  void ensureModels().finally(() => {
    loadingModels.value = false
  })
}

async function onCommand(command: string | number | object): Promise<void> {
  const id = String(command)
  if (id === llmConfig.value.model || switching.value) return
  switching.value = true
  try {
    await saveLlmConfig({ model: id }) // api_key 留空沿用,后端保留其余配置
  } catch (e) {
    ElMessage.error(`切换模型失败：${String(e)}`)
  } finally {
    switching.value = false
  }
}
</script>

<template>
  <!-- 提示挂在宿主 span 上,不挂在按钮上:按钮 disabled 时不派发鼠标事件,
       而「为什么不能点」恰恰是禁用时最需要看到的信息 -->
  <HiTooltip :content="disabledReason">
    <span class="model-picker__host">
      <el-dropdown
        :disabled="disabled || switching"
        trigger="click"
        @command="onCommand"
        @visible-change="onVisibleChange"
      >
        <button class="model-picker" type="button" :disabled="disabled || switching">
          <span class="model-picker__name">{{ currentName }}</span>
          <el-icon><ArrowDown /></el-icon>
        </button>
        <template #dropdown>
          <el-dropdown-menu>
            <el-dropdown-item v-if="loadingModels" disabled>正在获取模型列表…</el-dropdown-item>
            <el-dropdown-item v-else-if="modelOptions.length === 0" disabled>
              模型列表暂不可用，稍后重试
            </el-dropdown-item>
            <template v-else>
              <el-dropdown-item
                v-for="m in modelOptions"
                :key="m.id"
                :command="m.id"
                :disabled="m.id === llmConfig.model"
              >
                <div class="model-picker__item">
                  <span class="model-picker__item-name">
                    {{ m.name }}{{ m.id === llmConfig.model ? ' · 当前' : '' }}
                  </span>
                  <span class="model-picker__item-spec">{{ specOf(m) }}</span>
                </div>
              </el-dropdown-item>
            </template>
          </el-dropdown-menu>
        </template>
      </el-dropdown>
    </span>
  </HiTooltip>
</template>

<style scoped>
/* tooltip 的触发宿主:既不改变工具条的 flex 布局,也让按钮 disabled 时仍能悬浮 */
.model-picker__host {
  display: inline-flex;
  align-items: center;
}

/* 轻量选择按钮:浅灰底 + 10px 圆角,与输入卡片的 16px、右侧圆形发送按钮
   构成同一套圆角层次;不用白底细边,避免在白卡片里显得单薄、像贴纸。
   30px 高与右侧发送按钮齐平,但字重和底色压低,不抢输入框的视觉重心。 */
.model-picker {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  height: 30px;
  max-width: 200px;
  padding: 0 12px;
  border: 1px solid transparent;
  border-radius: 10px;
  background: #f3f4f6;
  color: #3f4550;
  font-size: 13px;
  font-weight: 500;
  line-height: 1;
  cursor: pointer;
  transition: background 0.15s, color 0.15s;
}
.model-picker:hover:not(:disabled) {
  background: #e8eaef;
  color: #1f2329;
}
.model-picker:disabled {
  /* 流式期间只是「暂时不可点」:保留底色与形状,不整体降透明度让按钮消失 */
  background: #f7f8fa;
  color: #9ca3af;
  cursor: not-allowed;
}
.model-picker__name {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.model-picker :deep(.el-icon) {
  flex-shrink: 0;
  font-size: 12px;
  color: var(--el-text-color-secondary);
}
.model-picker:disabled :deep(.el-icon) {
  color: #c4c9d2;
}

/* 下拉项:主行模型名,次行官方规格 */
.model-picker__item {
  display: flex;
  flex-direction: column;
  gap: 2px;
  padding: 2px 0;
}
.model-picker__item-name {
  font-size: 13px;
}
.model-picker__item-spec {
  font-size: 11px;
  color: var(--el-text-color-secondary);
}
</style>
