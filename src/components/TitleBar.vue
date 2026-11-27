<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref } from 'vue'
import { getCurrentWindow } from '@tauri-apps/api/window'
import type { UnlistenFn } from '@tauri-apps/api/event'
import { Setting, Minus, FullScreen, CopyDocument, Close } from '@element-plus/icons-vue'
import SettingsDialog from './SettingsDialog.vue'
import HiTooltip from './HiTooltip.vue'

// 自绘标题栏(窗口配置 decorations:false,系统标题栏已关闭)。
// data-tauri-drag-region 标在整条栏上 —— Tauri 的判定只看 mousedown 的 target 自身,
// 所以按钮不标就不会被拖拽吞掉点击;双击空白处仍由 Tauri 自动切换最大化。
// 图标一律取 Element Plus 官方图标集(@element-plus/icons-vue,按需引入),不自己画。
// 浏览器/预览面板里没有 Tauri 运行时,getCurrentWindow() 会在 setup 阶段读
// __TAURI_INTERNALS__.metadata 抛错,导致整个 App 挂载失败(页面全白)。只在真窗口里
// 构造;其余环境按钮保留但点击无操作,界面照常渲染。
const appWindow =
  typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window ? getCurrentWindow() : null

const maximized = ref(false)
// 子组件通过 defineExpose 暴露 open();InstanceType 能取到该方法
const settingsDialog = ref<InstanceType<typeof SettingsDialog> | null>(null)

// 最大化/还原两套图标,靠窗口尺寸变化同步状态
let unlisten: UnlistenFn | undefined
onMounted(async () => {
  if (!appWindow) return
  maximized.value = await appWindow.isMaximized()
  unlisten = await appWindow.onResized(async () => {
    maximized.value = await appWindow.isMaximized()
  })
})
onBeforeUnmount(() => unlisten?.())
</script>

<template>
  <header class="titlebar" data-tauri-drag-region>
    <div class="titlebar__left">
      <span class="titlebar__title">hi-agent</span>
      <span class="titlebar__sep"></span>

      <!-- 标题栏贴着窗口上沿,提示统一朝下弹,避免被窗口边界裁掉 -->
      <HiTooltip content="设置" placement="bottom">
        <button class="tbtn" type="button" @click="settingsDialog?.open()">
          <el-icon :size="15"><Setting /></el-icon>
        </button>
      </HiTooltip>
    </div>

    <div class="titlebar__actions">
      <HiTooltip content="最小化" placement="bottom">
        <button class="tbtn tbtn--win" type="button" @click="appWindow?.minimize()">
          <el-icon :size="11"><Minus /></el-icon>
        </button>
      </HiTooltip>

      <HiTooltip :content="maximized ? '还原' : '最大化'" placement="bottom">
        <button class="tbtn tbtn--win" type="button" @click="appWindow?.toggleMaximize()">
          <!-- 还原态用双框图标(Windows 惯例),最大化态用四角展开箭头 -->
          <el-icon :size="11">
            <CopyDocument v-if="maximized" />
            <FullScreen v-else />
          </el-icon>
        </button>
      </HiTooltip>

      <HiTooltip content="关闭" placement="bottom">
        <button class="tbtn tbtn--win tbtn--close" type="button" @click="appWindow?.close()">
          <el-icon :size="11"><Close /></el-icon>
        </button>
      </HiTooltip>
    </div>

    <SettingsDialog ref="settingsDialog" />
  </header>
</template>

<style scoped>
/* 固定 32px:关掉系统装饰后,拖动区与三个窗口按钮都由这里提供 */
.titlebar {
  flex-shrink: 0;
  height: 32px;
  display: flex;
  align-items: center;
  justify-content: space-between;
  background: #fff;
  border-bottom: 1px solid #eee;
  user-select: none;
}

.titlebar__left {
  display: flex;
  align-items: center;
}

.titlebar__title {
  padding: 0 10px 0 12px;
  font-size: 12px;
  color: #999;
}

/* 名字与设置之间的一道极细竖线:只做视觉分隔 */
.titlebar__sep {
  width: 1px;
  height: 11px;
  margin-right: 8px;
  background: #e0e0e0;
}

.titlebar__left .tbtn {
  width: 26px;
}

.titlebar__actions {
  display: flex;
  flex-shrink: 0;
}

/* 图标整体不接收指针事件,确保 mousedown 的 target 落在按钮上(否则拖拽判定会失效) */
.tbtn {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 32px;
  height: 32px;
  border: none;
  background: transparent;
  color: #333;
  cursor: pointer;
}
.tbtn :deep(.el-icon) {
  pointer-events: none;
}
.tbtn:hover {
  background: #e9e9e9;
}

.tbtn--win {
  width: 44px;
}
.tbtn--win:hover {
  background: #e5e5e5;
}

.tbtn--close:hover {
  background: #e81123;
  color: #fff;
}
</style>
