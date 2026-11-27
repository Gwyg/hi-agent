<script setup lang="ts">
import { computed } from 'vue'
import { useChat } from '../composables/chat'
import HiTooltip from './HiTooltip.vue'

// 状态栏右侧的用量区。数字是端点 usage 的真值(input_tokens),分母是后端给的
// 官方窗口;这里只负责换算成百分比和条宽,不自己估算 token、不参与压缩决策。
// 缓存命中率 = prompt_cache_hit_tokens / prompt_tokens:缓存只作用于输入,
// 不能用含输出 token 的 total_tokens 作分母(会把命中率系统性算低)。
const { contextUsage } = useChat()

/**
 * 展示缩写:1k = 1024 token(官方窗口 1,048,576 正好是 1024k)。
 * 只用于 tooltip 里的真值,不参与百分比计算;小值保留整数,大值最多一位小数。
 */
function kfmt(n: number): string {
  if (n < 1024) return String(n)
  const v = n / 1024
  return `${v >= 100 ? Math.round(v) : Number(v.toFixed(1))}k`
}

const percent = computed<number>(() => {
  const u = contextUsage.value
  if (!u || u.total === 0) return 0
  return Math.min(100, (u.used / u.total) * 100)
})

const hitPercent = computed<number>(() => {
  const u = contextUsage.value
  if (!u || u.used <= 0) return 0
  // clamp 只防上游异常:命中量理论上不会超过输入量,也不该是负数。
  // 0 命中是合法且常见(缓存尚未构建),照常显示 0.0% 而非「—」。
  const hit = Math.min(Math.max(u.cached ?? 0, 0), u.used)
  return (hit / u.used) * 100
})

// 用量很低时条也要有一点点可见宽度;0 / 未知时不画
const barWidth = computed<string>(() =>
  contextUsage.value && percent.value > 0 ? `${Math.max(percent.value, 1.5)}%` : '0%'
)

const label = computed<string>(() => (contextUsage.value ? `${percent.value.toFixed(1)}%` : '—'))

// 状态栏写全称:「缓存」两个字容易被读成「缓存占用了多少」,这里量的是命中比例。
const cacheLabel = computed<string>(() =>
  contextUsage.value ? `缓存命中率 ${hitPercent.value.toFixed(1)}%` : '缓存命中率 —'
)

// 两块指标各有自己的浮层:合在一个 tooltip 里要塞三行、还要解释口径,
// 反而把「已用/总量」「命中/输入」两组有用的真值淹没了。这里各留一行。
const usageTooltip = computed<string>(() => {
  const u = contextUsage.value
  return u ? `已用 ${kfmt(u.used)} / ${kfmt(u.total)} token` : '尚无模型请求'
})

const cacheTooltip = computed<string>(() => {
  const u = contextUsage.value
  return u
    ? `命中 ${kfmt(u.cached ?? 0)} / 输入 ${kfmt(u.used)} token · 上次请求`
    : '尚无模型请求'
})
</script>

<template>
  <div class="context-usage">
    <HiTooltip :content="usageTooltip">
      <span class="context-usage__group">
        <span class="context-usage__label">上下文 {{ label }}</span>
        <span class="context-usage__track">
          <span class="context-usage__fill" :style="{ width: barWidth }" />
        </span>
      </span>
    </HiTooltip>

    <HiTooltip :content="cacheTooltip">
      <span class="context-usage__cache">
        <!-- 与余额一致:数值一变就淡出淡入,避免状态栏数字生硬跳变 -->
        <Transition name="cache" mode="out-in">
          <span :key="cacheLabel" class="context-usage__cache-text">{{ cacheLabel }}</span>
        </Transition>
      </span>
    </HiTooltip>
  </div>
</template>

<style scoped>
.context-usage {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  flex-shrink: 0;
  font-size: 11px;
  color: var(--el-text-color-secondary);
  white-space: nowrap;
}
.context-usage__group {
  display: inline-flex;
  align-items: center;
  gap: 6px;
}
.context-usage__label {
  font-variant-numeric: tabular-nums;
}
.context-usage__track {
  width: 56px;
  height: 4px;
  border-radius: 999px;
  background: #e9ebef;
  overflow: hidden;
}
.context-usage__fill {
  display: block;
  height: 100%;
  border-radius: inherit;
  background: #1f2329;
  transition: width 0.3s ease;
}
/* 与右侧余额同一条细竖线分隔:三项数值口径不同,不挤成一团 */
.context-usage__cache {
  border-left: 1px solid #e5e7eb;
  padding-left: 8px;
  font-variant-numeric: tabular-nums;
}
.context-usage__cache-text {
  display: inline-block;
}
.cache-enter-active,
.cache-leave-active {
  transition: opacity 0.14s ease, transform 0.14s ease;
}
.cache-enter-from {
  opacity: 0;
  transform: translateY(4px);
}
.cache-leave-to {
  opacity: 0;
  transform: translateY(-4px);
}
</style>
