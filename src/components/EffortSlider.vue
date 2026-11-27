<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { ElMessage } from 'element-plus'
import HiTooltip from './HiTooltip.vue'
import { useChat } from '../composables/chat'
import { useLlmConfig } from '../composables/llmConfig'
import type { Effort } from '../api/types'

/**
 * 思考强度选择 —— 收起态只显示当前档位一枚轻按钮，点开才选档（与 Codex 的收起 / 展开一致）。
 *
 * 刻意不做的东西：
 * - 不做常驻三档分段：那块浅灰轨道 + 选中白底在工具条里太重，与旁边模型按钮挤在一起；
 * - 不做可拖动滑杆：本质只有三档，拖动是多余动作，点一下即切更直接；
 * - 不显示模型名：旁边已有 ModelPicker，再显示一次是重复信息；
 * - 不把 HiTooltip 挂在包住弹出面板的宿主上：早期版本黑底浮层盖住卡片，档位点不到。
 *   现在提示只包触发按钮，且可用时 content 为空 → 浮层根本不出现。
 *
 * 档位只有 low / high / max（DeepSeek 真机 /models 的 supported_levels），
 * medium / xhigh 会被上游静默折成 high，多造档位就是给用户假选择。
 * 后端行为：选 high 时不发 reasoning_effort、请求体字节不变；压缩摘要固定走默认强度。
 */
const LEVELS: Effort[] = ['low', 'high', 'max']

const { llmConfig, llmConfigured, setEffort } = useLlmConfig()
const { isStreaming } = useChat()

const rootEl = ref<HTMLElement | null>(null)
const open = ref(false)
const saving = ref(false)

const disabled = computed<boolean>(() => !llmConfigured.value || isStreaming.value)
// 只有禁用时才需要解释「为什么不能点」；可用时给空串，HiTooltip 会自动关闭浮层。
const disabledReason = computed<string>(() => {
  if (isStreaming.value) return '回复中，结束后可调思考强度'
  if (!llmConfigured.value) return '请先填写 API Key'
  return ''
})

// 后端已归一化脏值；前端再兜一层，避免异常配置下没有选中项。
const current = computed<Effort>(() =>
  LEVELS.includes(llmConfig.value.effort) ? llmConfig.value.effort : 'high'
)

function toggle(): void {
  if (disabled.value || saving.value) return
  open.value = !open.value
}

async function choose(level: Effort): Promise<void> {
  if (disabled.value || saving.value) return
  open.value = false // 点当前档也收起，行为一致
  if (level === current.value) return
  saving.value = true
  try {
    await setEffort(level)
  } catch (err) {
    ElMessage.error(`保存思考强度失败：${String(err)}`)
  } finally {
    saving.value = false
  }
}

function onDocPointerDown(e: PointerEvent): void {
  if (open.value && rootEl.value && !rootEl.value.contains(e.target as Node)) open.value = false
}

function onKeydown(e: KeyboardEvent): void {
  if (e.key === 'Escape') open.value = false
}

// 回复开始就把面板收起来，避免半开的浮层挡住消息区
watch(isStreaming, (streaming) => {
  if (streaming) open.value = false
})

onMounted(() => {
  document.addEventListener('pointerdown', onDocPointerDown)
  window.addEventListener('keydown', onKeydown)
})
onBeforeUnmount(() => {
  document.removeEventListener('pointerdown', onDocPointerDown)
  window.removeEventListener('keydown', onKeydown)
})
</script>

<template>
  <div ref="rootEl" class="effort" :class="{ 'effort--disabled': disabled }">
    <!-- 提示只包触发按钮，不包展开面板：面板展开时绝不会被黑底浮层盖住 -->
    <HiTooltip :content="disabledReason">
      <span class="effort__host">
        <button
          class="effort__trigger"
          type="button"
          :aria-expanded="open"
          aria-label="选择思考强度"
          :disabled="disabled || saving"
          @click="toggle"
        >
          <span class="effort__label">思考</span>
          <span class="effort__level">{{ current }}</span>
          <svg
            class="effort__chevron"
            :class="{ 'effort__chevron--open': open }"
            viewBox="0 0 12 12"
            aria-hidden="true"
          >
            <path d="M2.5 4.5 L6 8 L9.5 4.5" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" />
          </svg>
        </button>
      </span>
    </HiTooltip>

    <div v-if="open" class="effort__panel" role="menu" aria-label="思考强度">
      <div class="effort__panel-title">思考强度</div>
      <button
        v-for="level in LEVELS"
        :key="level"
        class="effort__option"
        :class="{ 'effort__option--active': level === current }"
        type="button"
        role="menuitemradio"
        :aria-checked="level === current"
        :disabled="disabled || saving"
        @click="choose(level)"
      >
        <span class="effort__dot"></span>
        <span class="effort__option-name">{{ level }}</span>
        <span v-if="level === 'high'" class="effort__tag">默认</span>
        <svg v-if="level === current" class="effort__check" viewBox="0 0 16 16" aria-hidden="true">
          <path d="M3 8.4 L6.4 11.6 L13 4.8" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round" />
        </svg>
      </button>
    </div>
  </div>
</template>

<style scoped>
.effort {
  position: relative;
  display: inline-flex;
  align-items: center;
  flex-shrink: 0;
}

.effort__host {
  display: inline-flex;
  align-items: center;
}

/* 收起态：无填充、无边框，只有文字 + 箭头；hover 才给一层极浅底，
   避免和旁边的模型按钮形成两块并排的灰底。 */
.effort__trigger {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  height: 30px;
  padding: 0 8px;
  border: none;
  border-radius: 9px;
  background: transparent;
  color: #7b828e;
  font-size: 12px;
  font-weight: 500;
  line-height: 1;
  cursor: pointer;
  transition: background 0.15s, color 0.15s;
}
.effort__trigger:hover:not(:disabled) {
  background: #f3f4f6;
  color: #3f4550;
}
.effort__trigger:disabled {
  color: #b6bcc6;
  cursor: not-allowed;
}
.effort__label {
  font-size: 11px;
  color: #9ca3af;
}
.effort__level {
  color: #3b82f6;
  font-size: 13px;
  font-weight: 600;
}
.effort__trigger:disabled .effort__level {
  color: #b6bcc6;
}
.effort__chevron {
  width: 11px;
  height: 11px;
  flex-shrink: 0;
  color: #9ca3af;
  transition: transform 0.15s;
}
.effort__chevron--open {
  transform: rotate(180deg);
}

/* 展开态：输入框上方一张白卡片，三档直接点选，不用滑杆 */
.effort__panel {
  position: absolute;
  bottom: calc(100% + 8px);
  left: 0;
  z-index: 20;
  width: 164px;
  padding: 8px 6px 6px;
  border-radius: 12px;
  background: #fff;
  box-shadow: 0 8px 28px rgba(20, 26, 40, 0.14), 0 0 0 1px rgba(20, 26, 40, 0.06);
  user-select: none;
}
.effort__panel-title {
  padding: 2px 10px 7px;
  color: #9ca3af;
  font-size: 11px;
  line-height: 1;
}
.effort__option {
  display: flex;
  align-items: center;
  gap: 9px;
  width: 100%;
  height: 32px;
  padding: 0 10px;
  border: none;
  border-radius: 8px;
  background: transparent;
  color: #4a505b;
  font-size: 13px;
  text-align: left;
  cursor: pointer;
  transition: background 0.15s;
}
.effort__option:hover:not(:disabled) {
  background: #f5f6f8;
}
.effort__option:disabled {
  cursor: not-allowed;
}
.effort__dot {
  width: 6px;
  height: 6px;
  flex-shrink: 0;
  border-radius: 50%;
  background: #d6dae1;
}
.effort__option--active {
  color: #3b82f6;
  font-weight: 600;
  background: #f2f6ff;
}
.effort__option--active:hover:not(:disabled) {
  background: #eaf1ff;
}
.effort__option--active .effort__dot {
  background: #3b82f6;
}
.effort__option-name {
  flex: 1;
}
.effort__tag {
  color: #a2a9b5;
  font-size: 11px;
  font-weight: 400;
}
.effort__check {
  width: 13px;
  height: 13px;
  flex-shrink: 0;
  color: #3b82f6;
}

@media (prefers-reduced-motion: reduce) {
  .effort__trigger,
  .effort__chevron,
  .effort__option {
    transition: none;
  }
}
</style>
