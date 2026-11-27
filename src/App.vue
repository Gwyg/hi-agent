<script setup lang="ts">
import { ref, onMounted, onBeforeUnmount } from 'vue'
import { useWorkspace } from './composables/workspace'
import { useChat } from './composables/chat'
import { useSessions } from './composables/sessions'
import { useLlmConfig } from './composables/llmConfig'
import LlmConfigBanner from './components/LlmConfigBanner.vue'
import ContextUsage from './components/ContextUsage.vue'
import AccountBalance from './components/AccountBalance.vue'
import HiTooltip from './components/HiTooltip.vue'
import TitleBar from './components/TitleBar.vue'
import WorkspacePicker from './components/WorkspacePicker.vue'
import SessionList from './components/SessionList.vue'

// 侧边栏可拖拽宽度：最小 160px，最大 480px，默认 200px
const MIN_WIDTH = 160
const MAX_WIDTH = 480

const sidebarWidth = ref(200)
const resizing = ref(false)

// 工作目录(workspace root):后端持久化 + 同步内存,此处只负责启动时拉一次;切换在 WorkspacePicker 里
const { currentWorkspace, workspaceName, refresh } = useWorkspace()

// 会话:侧边栏的「新对话」与聊天页共用同一份状态(消息列表在 composable 里)
const { isStreaming, startNewChat } = useChat()
// 会话列表:启动时拉一次,并按断点自动打开上次那个对话
const { restore: restoreSession } = useSessions()

// 大模型配置:未配置时顶部常驻醒目横幅(llmLoaded 用于避免查到结果前闪一下)
const { llmConfigured, llmLoaded, refresh: refreshLlm } = useLlmConfig()

// 在分隔条上按下：记录起点，挂 window 级 move/up，拖动期间改宽度
function onResizerDown(e: MouseEvent): void {
  e.preventDefault()
  resizing.value = true
  const startX = e.clientX
  const startW = sidebarWidth.value
  const onMove = (ev: MouseEvent): void => {
    const next = Math.min(MAX_WIDTH, Math.max(MIN_WIDTH, startW + (ev.clientX - startX)))
    sidebarWidth.value = next
  }
  const onUp = (): void => {
    resizing.value = false
    window.removeEventListener('mousemove', onMove)
    window.removeEventListener('mouseup', onUp)
  }
  window.addEventListener('mousemove', onMove)
  window.addEventListener('mouseup', onUp)
}

onMounted(() => {
  refresh() // 启动时拉取当前工作目录展示
  refreshLlm() // 启动时检查大模型是否已配置(未配置则顶部显示横幅)
  // 会话列表 + 断点恢复:上次停在哪个对话就回到哪个,和关掉之前看到的一样。
  // 不 await:它内部失败也只是回到空白,不该挡住界面出现。
  void restoreSession()
})
onBeforeUnmount(() => {
  // mouseup 中已移除监听;此处仅兜底复位状态
  resizing.value = false
})
</script>

<template>
  <div class="app">
    <!-- 自绘标题栏:替代系统装饰,右侧放设置与窗口按钮 -->
    <TitleBar />

    <LlmConfigBanner v-if="llmLoaded && !llmConfigured" />

    <div class="layout" :class="{ 'is-resizing': resizing }">
      <aside class="sidebar" :style="{ width: sidebarWidth + 'px' }">
        <!-- 工作区在顶部:它是范围界定,下面的会话列表都属于它(换目录就换一批会话) -->
        <WorkspacePicker />

        <!-- 新建会话:侧边栏唯一主操作;流式回复期间禁用
             (品牌名已上移到标题栏,导航已收敛为标题栏的齿轮) -->
        <button class="sidebar__new" type="button" :disabled="isStreaming" @click="startNewChat">
          新对话
        </button>

        <!-- 会话列表:占用剩余高度自己滚。它归属上面的工作区,换目录就换一批 -->
        <SessionList />
      </aside>

      <!-- 拖拽手柄:位于侧边栏与内容之间 -->
      <div
        class="resizer"
        :class="{ 'is-resizing': resizing }"
        @mousedown="onResizerDown"
      ></div>

      <main class="content">
        <router-view />
      </main>
    </div>

    <!-- 底部状态栏:左侧工作区名字(悬浮看完整路径),右侧上下文用量 + 缓存命中率 + 账户余额;
         三者口径互不相关,切换目录的入口在侧边栏底部,这里只做信息展示 -->
    <footer class="statusbar">
      <!-- 浮层用 top-start 左对齐这块区域的左边缘:默认的 top 是相对触发元素居中,
           而这块被撑满时居中会飘到屏幕正中,与左边的数据不对称。
           左对齐下箭头会被夹到浮层左下角、指着旁边的空白,所以一并关掉 -->
      <HiTooltip
        :content="currentWorkspace || '未选择工作目录'"
        placement="top-start"
        :arrow="false"
      >
        <span class="statusbar__ws">
          <span class="statusbar__label">工作目录</span>
          <span class="statusbar__path">{{ workspaceName || '未选择工作目录' }}</span>
        </span>
      </HiTooltip>
      <ContextUsage />
      <AccountBalance />
    </footer>
  </div>
</template>

<style scoped>
/* 纵向容器:横幅常驻顶部,下方布局占满剩余高度(横幅不随内容滚动)。
   高度用 100%(跟随 #app),不用 100vh —— 后者不扣掉任何外层盒模型尺寸,容易多出几像素撑出滚动条。 */
.app {
  display: flex;
  flex-direction: column;
  height: 100%;
}

.layout {
  display: flex;
  flex: 1;
  min-height: 0;
}

/* 拖拽进行时:禁用文本选中 + 统一光标 */
.layout.is-resizing {
  user-select: none;
  cursor: col-resize;
}

/* 纵向 flex:标题+导航在上,选择目录按钮靠 margin-top:auto 贴底 */
/* 不加 overflow:hidden —— 工作区列表的浮层要从侧边栏向右溢出，
   裁掉的话面板会被切成一条。侧边栏本身没有会溢出的子元素。 */
.sidebar {
  padding: 16px;
  flex-shrink: 0;
  display: flex;
  flex-direction: column;
}

/* 新建会话:侧边栏唯一主操作,用主色实心按钮 */
.sidebar__new {
  flex-shrink: 0;
  width: 100%;
  padding: 7px 8px;
  font-size: 13px;
  color: #fff;
  background: #1f2329;
  border: none;
  border-radius: 8px;
  cursor: pointer;
}
.sidebar__new:hover:not(:disabled) {
  background: #33373d;
}
.sidebar__new:disabled {
  background: #d9d9d9;
  cursor: not-allowed;
}

/* 拖拽手柄:透明命中区(便于抓取),视觉为 2px 蓝线 + 中间稍宽、上下淡出的短把手 */
.resizer {
  flex-shrink: 0;
  width: 6px;
  cursor: col-resize;
  position: relative;
  background: transparent;
}
/* 常驻分隔线:同一位置,默认灰、悬停/拖拽变蓝;上下 mask 淡出(两端窄、中段可见) */
.resizer::before {
  content: '';
  position: absolute;
  top: 0;
  bottom: 0;
  left: 50%;
  transform: translateX(-50%);
  width: 1px;
  background: #e5e5e5;
  transition: background 0.15s;
  -webkit-mask-image: linear-gradient(to bottom, transparent 0%, #000 18%, #000 82%, transparent 100%);
  mask-image: linear-gradient(to bottom, transparent 0%, #000 18%, #000 82%, transparent 100%);
}
.resizer:hover::before,
.resizer.is-resizing::before {
  background: #1e90ff;
}

/* 内容区:自身不滚动,由页面内部自己滚(聊天页只滚消息区)。
   注意:这里不能加 padding —— 子页面的 height:100% 相对的是本容器的内容区高度,
   加 padding 会多出上下 padding 的高度而溢出,逼出一条多余的滚动条。 */
.content {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
  overflow: hidden;
}

/* 子页面撑满剩余高度(Vue scoped 会把本组件的 scope 标记打到子组件根节点上,故能选中) */
.content > * {
  flex: 1;
  min-height: 0;
}

/* 底部状态栏:横跨整个窗口,只做信息展示(沙箱根在哪),视觉权重压到最低 */
.statusbar {
  flex-shrink: 0;
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 4px 12px;
  border-top: 1px solid #eee;
  background: #fafafa;
  font-size: 12px;
}

/* 左侧工作区信息:只占内容宽度,靠 margin-right:auto 把右侧三块推到右边缘。
   它同时是 tooltip 的触发元素 —— 宽度只包住文字,鼠标扫过右侧空白不会弹提示。 */
.statusbar__ws {
  display: flex;
  align-items: center;
  gap: 8px;
  min-width: 0;
  margin-right: auto;
}

.statusbar__label {
  flex-shrink: 0;
  color: #999;
}

/* 名字只占内容宽度(不再 flex:1),这样它自己不会被拉成一个超宽盒子;
   太长时在本块内省略,完整值靠 HiTooltip 悬浮查看 */
.statusbar__path {
  min-width: 0;
  color: #666;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
</style>
