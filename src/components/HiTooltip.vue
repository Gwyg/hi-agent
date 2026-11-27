<script setup lang="ts">
import { computed } from 'vue'
import type { Placement } from 'element-plus'

/**
 * 统一的悬浮说明浮层,替代浏览器原生 `title`。
 * 原生 title 由系统绘制:浅黄底、直角、延迟一秒多、样式无法控制,
 * 放在灰白界面里很突兀。这里统一收敛成深色底 + 白字(见 global.css 的 .hi-tooltip),
 * 也把 effect / 样式类 / 延迟 / 空内容处理收在一处,调用方只给文案。
 * 用 ElTooltip 而不是自己算坐标:它自带边界翻转、箭头、滚动跟随与卸载清理。
 *
 * `placement` 直接用 Element Plus 的类型(含 `top-start` 等 12 个值):
 * 默认 `top` 是**相对触发元素居中**,触发元素很宽时浮层会飘到屏幕中间,
 * 这种「文字在左边、浮层居中」的场合要传 `*-start` 改成左对齐。
 * 但 `*-start` 下箭头会被夹到浮层的起始角(参考元素比浮层宽时更是如此),
 * 看起来像指着旁边的空白 —— 这类场合配 `:arrow="false"` 一起用。
 */
const props = withDefaults(
  defineProps<{
    content: string
    placement?: Placement
    /** 是否画指向箭头。浮层贴边、左对齐等箭头对不上内容时关掉 */
    arrow?: boolean
    disabled?: boolean
  }>(),
  { placement: 'top', arrow: true, disabled: false }
)

// 内容为空时不要弹一个空壳浮层(例如模型按钮可用时没有禁用原因)
const off = computed<boolean>(() => props.disabled || props.content.trim().length === 0)
</script>

<template>
  <el-tooltip
    :content="content"
    :disabled="off"
    :placement="placement"
    :show-arrow="arrow"
    effect="dark"
    popper-class="hi-tooltip"
    :show-after="250"
    :hide-after="0"
  >
    <slot />
  </el-tooltip>
</template>
