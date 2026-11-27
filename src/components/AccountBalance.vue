<script setup lang="ts">
import { computed } from 'vue'
import { useLlmConfig } from '../composables/llmConfig'
import HiTooltip from './HiTooltip.vue'

// 状态栏右侧:账户总可用余额。只展示 total_balance,赠金/充值拆分与
// is_available 都不展示;金额是上游字符串(如 "11.80"),原样拼符号不转数字。
// 启动 / 保存 API Key / 每轮对话收尾(含延迟补刷)会自动拉,点击这里可随时手动刷新。
const { balance, refreshBalance } = useLlmConfig()

const SYMBOLS: Record<string, string> = { CNY: '¥', USD: '$' }

const label = computed<string>(() => {
  const b = balance.value
  if (!b) return '余额 —'
  return `余额 ${SYMBOLS[b.currency] ?? `${b.currency} `}${b.total_balance}`
})

const tooltip = computed<string>(() =>
  balance.value ? '账户总可用余额 · 点击刷新' : '暂无余额信息 · 点击刷新'
)
</script>

<template>
  <HiTooltip :content="tooltip">
    <button class="account-balance" type="button" @click="refreshBalance">
      <!-- key 跟着文案走:数值一变就走一次淡出淡入,让状态栏的变化能被看见。
           mode="out-in" 避免新旧两个数字叠在一起抖动。 -->
      <Transition name="balance" mode="out-in">
        <span :key="label" class="account-balance__text">{{ label }}</span>
      </Transition>
    </button>
  </HiTooltip>
</template>

<style scoped>
.account-balance {
  flex-shrink: 0;
  border: none;
  border-left: 1px solid #e5e7eb;
  padding: 0 0 0 8px;
  background: transparent;
  font: inherit;
  font-size: 11px;
  color: var(--el-text-color-secondary);
  font-variant-numeric: tabular-nums;
  white-space: nowrap;
  cursor: pointer;
}
.account-balance:hover {
  color: #1f2329;
}

.account-balance__text {
  display: inline-block;
}

/* 数字变化:旧值轻退、新值轻入,各 140ms;不做数字滚动——金额是上游字符串,
   转数字插值会破坏「原样展示、不丢末尾 0」的口径,两位小数的滚动也过于花哨。 */
.balance-enter-active,
.balance-leave-active {
  transition: opacity 0.14s ease, transform 0.14s ease;
}
.balance-enter-from {
  opacity: 0;
  transform: translateY(4px);
}
.balance-leave-to {
  opacity: 0;
  transform: translateY(-4px);
}
</style>
