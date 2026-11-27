import { getCurrentWindow, UserAttentionType } from '@tauri-apps/api/window'
import {
  isPermissionGranted,
  requestPermission,
  sendNotification,
} from '@tauri-apps/plugin-notification'

/**
 * 一轮对话在**用户已经离开应用**时跑完，怎么告诉他。
 *
 * 三件事各自独立、互不阻塞，因为它们的失效条件完全不同：系统通知可能被系统设置拦住、
 * 也可能根本没弹出来；声音可能被自动播放策略吞掉，也可能与系统通知音叠在一起变成双响；
 * 任务栏闪烁则在两者都失效时仍然有效。任意一条成功，用户就知道回来看看。
 */

/** 通知标题固定用产品名，正文才区分是哪个任务——这样通知中心里一堆条目能一眼归堆。 */
const TITLE = 'hi-agent'
/** 正文里回指用户那句话的截断长度。比后端会话标题（10）长，因为这里一行放得下。 */
const PROMPT_PREVIEW_CHARS = 20
/** 错误信息只取开头，完整内容在气泡里。 */
const ERROR_PREVIEW_CHARS = 60

/**
 * 开发环境不发系统通知，只自己响一声、闪一下任务栏。
 *
 * 两个原因都出在「身份」上：
 *
 * 1. **标题会变成 Windows PowerShell**。`notify-rust` 的 Windows 后端在没拿到 AUMID 时
 *    回退到 `Toast::POWERSHELL_APP_ID`，图标也跟着变成 PowerShell；而插件只在「exe 不在
 *    `target/debug` / `target/release` 下」时才把 `com.gwyg.hiagent` 传下去——开发时永远不传。
 *    于是 dev 的 toast 顶着一个不属于我们的名字，比不弹还难看。
 * 2. **会双响**。它照样能弹（PowerShell 的 AUMID 系统上是注册过的），而 Windows toast
 *    自带系统通知音（`notify-rust` 转给 WinRT，XML 里不带 audio 节点 → 系统用默认音），
 *    再叠上我们自己的合成音就是两声。
 *
 * 安装包（NSIS/MSI）会把 `com.gwyg.hiagent` 注册进注册表，插件那时也会正常带上它，
 * 标题和图标都是 hi-agent，声音交给系统——所以发布环境不需要我们的合成音。
 *
 * Vite 在打包时把这个常量折成 `false`，整段音频代码会一起被 tree-shake 掉。
 */
const IS_DEV = import.meta.env.DEV

export interface TurnFinishedNotice {
  /** 本轮用户输入的原文，用来在通知里回指是哪件事。 */
  prompt: string
  /** 本轮耗时（ms）。 */
  elapsed: number
  /** 出错信息：有值表示这一轮是失败的，正文改成失败措辞。 */
  error?: string
}

/**
 * 「用户不在应用里」= 窗口没有焦点。最小化、切到别的应用、被其他窗口盖住都算。
 *
 * 判断失败（纯前端调试时没有窗口 API）按「人就在应用里」处理：宁可少提醒，
 * 也不要在浏览器里开发预览时弹一堆假通知。
 */
async function isAway(): Promise<boolean> {
  try {
    return !(await getCurrentWindow().isFocused())
  } catch {
    return false
  }
}

/** 折成单行并按**字符**截断（`Array.from` 才不会切坏 emoji 的代理对）。 */
function preview(text: string, max: number): string {
  const flat = text.replace(/\s+/g, ' ').trim()
  const chars = Array.from(flat)
  return chars.length > max ? `${chars.slice(0, max).join('')}…` : flat
}

/** 耗时写成人话：一分钟以内用秒，超过就用分秒。 */
function formatElapsed(ms: number): string {
  const total = Math.max(0, Math.round(ms / 1000))
  if (total < 60) return `${total} 秒`
  const minutes = Math.floor(total / 60)
  const seconds = total % 60
  return seconds === 0 ? `${minutes} 分` : `${minutes} 分 ${seconds} 秒`
}

function bodyOf(notice: TurnFinishedNotice): string {
  const subject = `「${preview(notice.prompt, PROMPT_PREVIEW_CHARS)}」`
  if (notice.error) {
    return `${subject}回复失败：${preview(notice.error, ERROR_PREVIEW_CHARS)}`
  }
  return `${subject}已完成 · 用时 ${formatElapsed(notice.elapsed)}`
}

/**
 * 通知权限。`null` = 还没问过；问过一次就记住结果。
 *
 * 只在用户不在应用时才会走到这里，所以「首次授权框弹出来时用户正看着别的窗口」
 * 是常态——他回来时能看到系统弹窗，不算丢。拒绝后本次运行不再戳，免得每轮都打扰；
 * 下次启动会重新问一次，用户改系统设置后能自愈。
 */
let granted: boolean | null = null

async function ensurePermission(): Promise<boolean> {
  if (granted !== null) return granted
  try {
    granted = await isPermissionGranted()
    if (!granted) granted = (await requestPermission()) === 'granted'
  } catch {
    granted = false
  }
  return granted
}

/** 音频上下文单例。`IS_DEV` 为 false（发布包）时永远不会被创建。 */
let audio: AudioContext | null = null

/** 懒建音频上下文；已存在但被挂起（切后台、系统休眠）时唤醒。 */
function ensureAudio(): AudioContext | null {
  try {
    audio ??= new AudioContext()
    if (audio.state === 'suspended') void audio.resume()
    return audio
  } catch {
    // 没有音频设备 / 被系统策略禁用：静默降级，通知照发
    return null
  }
}

/**
 * 双音「叮咚」：A5 → E6，正弦波 + 指数衰减。
 *
 * 只在开发环境用（见 `IS_DEV`），发布包的提示音交给系统 toast。现场合成而不是打包
 * 一个音频文件：不用管素材路径和打包，音量、音高、间隔都在这一处可调，改一个数字就能试。
 */
function chime(): void {
  const ctx = ensureAudio()
  if (!ctx) return
  const at = ctx.currentTime
  // 频率 / 相对起始 / 时长：第二个音稍长，收尾落得稳一些
  const notes: ReadonlyArray<readonly [number, number, number]> = [
    [880, 0, 0.13],
    [1318.51, 0.1, 0.2],
  ]
  for (const [frequency, offset, duration] of notes) {
    const start = at + offset
    const oscillator = ctx.createOscillator()
    const gain = ctx.createGain()
    oscillator.type = 'sine'
    oscillator.frequency.value = frequency
    // 起音 12ms 淡入避免爆音；`exponentialRamp` 的目标必须大于 0，所以用 0.0001 当「静音」
    gain.gain.setValueAtTime(0.0001, start)
    gain.gain.exponentialRampToValueAtTime(0.14, start + 0.012)
    gain.gain.exponentialRampToValueAtTime(0.0001, start + duration)
    oscillator.connect(gain).connect(ctx.destination)
    oscillator.start(start)
    oscillator.stop(start + duration + 0.02)
  }
}

/**
 * 预热音频上下文。
 *
 * 自动播放策略要求 `AudioContext` 在用户手势里创建或恢复，否则会一直 suspended。
 * 等到真要响的那一刻（窗口已经失焦）再建就晚了——那时没有手势可用，声音会被吞掉。
 * 所以第一次点击或按键时先建一个空上下文放着，之后随时可播。
 */
let warmed = false
function warmUp(): void {
  if (warmed) return
  warmed = true
  ensureAudio()
}
if (IS_DEV) {
  window.addEventListener('pointerdown', warmUp, { once: true, capture: true })
  window.addEventListener('keydown', warmUp, { once: true, capture: true })
}

/**
 * 一轮对话结束时调用。**用户还在应用里就直接返回**——他已经看见结果了，再弹通知只是打扰。
 *
 * 调用方不需要 `await`：整条链路都是尽力而为，失败也只是「少一个提醒」，不该拖慢收尾。
 */
export async function notifyTurnFinished(notice: TurnFinishedNotice): Promise<void> {
  if (!(await isAway())) return

  // 闪烁零依赖，dev 和发布包都有效，是通知被权限或打包问题挡住时的兜底
  void getCurrentWindow()
    .requestUserAttention(UserAttentionType.Informational)
    .catch(() => {
      // 窗口 API 不可用时忽略：闪烁只是锦上添花
    })

  // 开发环境到此为止：没有 AUMID，发了也是个顶着 PowerShell 名字的框，还会和合成音双响
  if (IS_DEV) {
    chime()
    return
  }

  if (!(await ensurePermission())) return
  sendNotification({ title: TITLE, body: bodyOf(notice) })
}
