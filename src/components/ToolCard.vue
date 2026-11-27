<script setup lang="ts">
import { computed, ref } from 'vue'
import { ArrowRight, Loading } from '@element-plus/icons-vue'

import type { Block } from '../composables/chat'
import HiTooltip from './HiTooltip.vue'

type ToolBlock = Extract<Block, { kind: 'tool' }>

/** 行级 diff 的一行。`sign` 直接参与渲染,省得模板里再写一遍条件 */
type DiffRow = { kind: 'ctx' | 'del' | 'add' | 'gap'; sign: string; text: string }

/** 改动点前后保留的上下文行数。再多就是拿整份文件冒充 diff */
const CONTEXT_LINES = 3
/** 渲染行数上限:几百行的 diff 展开是一面墙,不解决「改了什么」 */
const MAX_DIFF_ROWS = 200
/** LCS 是 O(n×m):规模超了就退化成「整段删 + 整段加」,别为一次展示吃掉几百毫秒 */
const LCS_CELL_LIMIT = 200_000

/**
 * 行级最长公共子序列。
 *
 * 只有 `old_string` / `new_string` 两块文本,没有文件上下文,所以未变的行**只能**靠
 * 两侧比对认出来;直接整块染红染绿会把「改一行」重写成上百行噪音。
 *
 * 从尾部倒推:优先出删除行,同一位置的删与加才读得出「这行被换掉了」。
 */
function lcsRows(before: string[], after: string[]): DiffRow[] {
  const m = before.length
  const n = after.length
  const plain = (): DiffRow[] => [
    ...before.map((text) => ({ kind: 'del' as const, sign: '-', text })),
    ...after.map((text) => ({ kind: 'add' as const, sign: '+', text })),
  ]
  if (m * n > LCS_CELL_LIMIT) return plain()

  // dp[i][j] = before[i..] 与 after[j..] 的 LCS 长度
  const dp: number[][] = Array.from({ length: m + 1 }, () => new Array<number>(n + 1).fill(0))
  for (let i = m - 1; i >= 0; i--) {
    for (let j = n - 1; j >= 0; j--) {
      dp[i][j] =
        before[i] === after[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1])
    }
  }

  const rows: DiffRow[] = []
  let i = 0
  let j = 0
  while (i < m && j < n) {
    if (before[i] === after[j]) {
      rows.push({ kind: 'ctx', sign: ' ', text: before[i] })
      i++
      j++
    } else if (dp[i + 1][j] >= dp[i][j + 1]) {
      rows.push({ kind: 'del', sign: '-', text: before[i] })
      i++
    } else {
      rows.push({ kind: 'add', sign: '+', text: after[j] })
      j++
    }
  }
  while (i < m) rows.push({ kind: 'del', sign: '-', text: before[i++] })
  while (j < n) rows.push({ kind: 'add', sign: '+', text: after[j++] })
  return rows
}

/** 每个改动点只留前后各几行,中间大段未变的折成一行提示 */
function hunkify(rows: DiffRow[]): DiffRow[] {
  const changed = rows.map((r) => r.kind !== 'ctx')
  if (!changed.includes(true)) return rows

  const keep = rows.map(() => false)
  rows.forEach((_, i) => {
    if (!changed[i]) return
    const from = Math.max(0, i - CONTEXT_LINES)
    const to = Math.min(rows.length - 1, i + CONTEXT_LINES)
    for (let k = from; k <= to; k++) keep[k] = true
  })

  const out: DiffRow[] = []
  let skipped = 0
  const flush = (): void => {
    if (skipped > 0) out.push({ kind: 'gap', sign: '', text: `⋯ 省略 ${skipped} 行` })
    skipped = 0
  }
  rows.forEach((row, i) => {
    if (keep[i]) {
      flush()
      out.push(row)
    } else {
      skipped++
    }
  })
  flush()
  return out
}

function buildDiff(before: string, after: string): DiffRow[] {
  // 空串要当零行:`''.split('\n')` 会给出一个空串元素,删除(空 new_string)就会多出一行空的 `+`
  const lines = (s: string): string[] => (s === '' ? [] : s.split('\n'))
  const rows = hunkify(lcsRows(lines(before), lines(after)))
  if (rows.length <= MAX_DIFF_ROWS) return rows
  const rest = rows.length - MAX_DIFF_ROWS
  return [...rows.slice(0, MAX_DIFF_ROWS), { kind: 'gap', sign: '', text: `还有 ${rest} 行未显示` }]
}

const props = defineProps<{ block: ToolBlock }>()

const expanded = ref(false)

// 参数是模型给的 JSON 原文:解析失败不猜结构,整体当文本展示
//
// 保留 `null` 这一态,是因为 edit / write 的专属展示要判断「参数到底解出来没有」:
// 解不出来时必须退回原始 JSON,不能把空对象当成「没有 content」渲染成一片空白
const parsedArgs = computed<Record<string, unknown> | null>(() => {
  try {
    const v: unknown = JSON.parse(props.block.args)
    return v !== null && typeof v === 'object' ? (v as Record<string, unknown>) : null
  } catch {
    return null
  }
})

const args = computed<Record<string, unknown>>(() => parsedArgs.value ?? {})

const str = (key: string): string => {
  const v = args.value[key]
  return typeof v === 'string' ? v : ''
}

/** 标题只用 args 派生,不看结果文本——结果格式一变,摘要不会跟着崩 */
const title = computed<string>(() => {
  switch (props.block.name) {
    case 'read':
      return str('path') ? `查看 ${str('path')}` : '查看文件'
    case 'write':
      return str('path') ? `写入 ${str('path')}` : '写入文件'
    case 'edit':
      return str('path') ? `编辑 ${str('path')}` : '编辑文件'
    case 'search_files':
      return str('pattern') ? `搜索 ${str('pattern')}` : '搜索'
    case 'bash': {
      // description 是模型自述的用途,只当标题;命令原文在展开区,不会被它替代
      const d = str('description')
      if (d) return d
      return str('command').split('\n')[0].trim() || '执行命令'
    }
    case 'web_search':
      return str('query') ? `联网搜索 ${str('query')}` : '联网搜索'
    case 'web_fetch': {
      // 去掉协议头:省 8 个字符,且在固定宽度里前面的域名比 `https://` 有用得多
      const url = str('url').replace(/^https?:\/\//i, '')
      return url ? `抓取网页 ${url}` : '抓取网页'
    }
    default:
      return props.block.name
  }
})

/**
 * 是否还在跑:**看有没有结果**,不看 `status`。
 *
 * `status` 只在流式过程中由事件给到(ok/error/skipped),它不落库——历史恢复出来的
 * 工具块没有 `status`。若用它判断,从历史加载的每张卡片都会永久转圈、显示「执行中…」。
 * 「有结果行 = 已结束」这个判据在实时路径上与原写法等价,在历史路径上也成立。
 */
const running = computed(() => props.block.result === undefined)

/**
 * 编辑工具的展开内容:改了什么,而不是参数长什么样。
 *
 * `null` 表示这条不走 diff 渲染(不是 edit,或参数解不出来),退回原始 JSON。
 * 参数里的 `\n` 是字面转义,一段二十行的改动会挤成一行,没法读。
 */
const diffRows = computed<DiffRow[] | null>(() => {
  const a = parsedArgs.value
  if (props.block.name !== 'edit' || !a) return null
  const before = a.old_string
  const after = a.new_string
  // 后端拒空 old_string,空串 new_string 是合法的「删除」
  if (typeof before !== 'string' || !before || typeof after !== 'string') return null
  return buildDiff(before, after)
})

/** `new_string` 为空串时不是「改动」而是整段删除,标签跟着说实话 */
const diffLabel = computed<string>(() => (str('new_string') ? '改动' : '删除'))

/**
 * 写入工具的展开内容:整份文件内容。
 *
 * 不染色:新文件全绿等于没给信息;覆盖时旧内容根本不在参数里(后端也不回),
 * 没有可比的东西,染绿只是假装知道改了什么。`null` 同样表示退回原始 JSON。
 */
const writeContent = computed<string | null>(() => {
  const a = parsedArgs.value
  if (props.block.name !== 'write' || !a) return null
  const content = a.content
  return typeof content === 'string' ? content : null
})

const detail = computed<string>(() => {
  if (props.block.name === 'bash' && str('command')) return str('command')
  if (!props.block.args) return ''
  try {
    return JSON.stringify(JSON.parse(props.block.args), null, 2)
  } catch {
    return props.block.args
  }
})
</script>

<template>
  <div
    class="tool"
    :class="{
      'tool--error': block.status === 'error',
      'tool--skipped': block.status === 'skipped',
      'tool--running': running,
    }"
  >
    <button class="tool__head" type="button" @click="expanded = !expanded">
      <el-icon class="tool__chevron" :class="{ 'tool__chevron--open': expanded }">
        <ArrowRight />
      </el-icon>
      <el-icon v-if="running" class="is-loading tool__spinner"><Loading /></el-icon>
      <HiTooltip :content="title">
        <span class="tool__title">{{ title }}</span>
      </HiTooltip>
      <span v-if="block.status === 'error'" class="tool__tag tool__tag--error">失败</span>
      <span v-else-if="block.status === 'skipped'" class="tool__tag">未执行</span>
    </button>

    <div v-show="expanded" class="tool__body">
      <div v-if="diffRows" class="tool__section">
        <div class="tool__label">{{ diffLabel }}</div>
        <div class="tool__diff">
          <div
            v-for="(row, i) in diffRows"
            :key="i"
            class="tool__line"
            :class="`tool__line--${row.kind}`"
          >{{ row.sign }}{{ row.text }}</div>
        </div>
      </div>
      <div v-else-if="writeContent !== null" class="tool__section">
        <div class="tool__label">内容</div>
        <pre class="tool__pre">{{ writeContent }}</pre>
      </div>
      <div v-else-if="detail" class="tool__section">
        <div class="tool__label">{{ block.name === 'bash' ? '命令' : '参数' }}</div>
        <pre class="tool__pre">{{ detail }}</pre>
      </div>
      <div class="tool__section">
        <div class="tool__label">结果</div>
        <pre class="tool__pre">{{ block.result ?? '执行中…' }}</pre>
      </div>
    </div>
  </div>
</template>

<style scoped>
.tool {
  margin: 6px 0;
  border: 1px solid #e8eaed;
  border-radius: 8px;
  background: #fafbfc;
  overflow: hidden;
}

.tool--running {
  border-color: #dfe3e8;
}

.tool--skipped {
  background: #f7f8f9;
}

.tool--error {
  border-color: #f5c2c0;
  background: #fffafa;
}

.tool__head {
  width: 100%;
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 7px 10px;
  border: none;
  background: transparent;
  font: inherit;
  text-align: left;
  cursor: pointer;
  color: #1f2329;
}

.tool__head:hover {
  background: rgba(0, 0, 0, 0.025);
}

.tool__chevron {
  flex-shrink: 0;
  font-size: 12px;
  color: #9ca3af;
  transition: transform 0.15s;
}

.tool__chevron--open {
  transform: rotate(90deg);
}

.tool__spinner {
  flex-shrink: 0;
  font-size: 12px;
  color: #6b7280;
}

.tool__title {
  flex: 1;
  min-width: 0;
  font-size: 13px;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.tool--error .tool__title {
  color: #b42318;
}

.tool--skipped .tool__title {
  color: #6b7280;
}

.tool__tag {
  flex-shrink: 0;
  font-size: 11px;
  line-height: 1;
  padding: 3px 6px;
  border-radius: 4px;
  background: #eef0f2;
  color: #6b7280;
}

.tool__tag--error {
  background: #fee4e2;
  color: #b42318;
}

.tool__body {
  padding: 0 10px 10px;
}

.tool__section + .tool__section {
  margin-top: 8px;
}

.tool__label {
  margin-bottom: 4px;
  font-size: 11px;
  color: #9ca3af;
}

.tool__pre {
  margin: 0;
  max-height: 260px;
  overflow: auto;
  padding: 8px 10px;
  border-radius: 6px;
  background: #f3f4f6;
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  font-size: 12px;
  line-height: 1.55;
  color: #374151;
  white-space: pre-wrap;
  word-break: break-word;
}

/* 行级 diff:未变的行留作上下文,删/增各自染色。行数已在 buildDiff 里截过,这里只管滚动 */
.tool__diff {
  max-height: 320px;
  overflow: auto;
  border-radius: 6px;
  background: #f7f8f9;
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  font-size: 12px;
  line-height: 1.6;
}

/* pre-wrap 不只是为了长行:它同时保住 sign 那一位空格(=上下文行的缩进对齐) */
.tool__line {
  padding: 1px 10px;
  color: #6b7280;
  white-space: pre-wrap;
  word-break: break-word;
}

.tool__line--del {
  background: #fdecec;
  color: #b42318;
}

.tool__line--add {
  background: #eaf7ee;
  color: #1a7f37;
}

.tool__line--gap {
  padding: 2px 10px;
  font-size: 11px;
  color: #b6bcc4;
}
</style>
