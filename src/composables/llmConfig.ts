import { ref, type Ref } from 'vue'

import { invoke } from '@tauri-apps/api/core'

import type { BalanceInfo, Effort, LlmConfigInput, LlmConfigView, ModelOption } from '../api/types'

// 后端 config::llm 的内置兜底,只用于首次查询前的占位显示
const FALLBACK_MODEL = 'deepseek-flash'
const FALLBACK_EFFORT: Effort = 'high'

// 跨组件共享的大模型配置状态(模块级单例,与 workspace.ts 同套约定):
// - llmConfigured:是否已配置(后端以「生效 api_key 非空」为准)
// - llmConfig:后端返回的脱敏视图(生效 model / api_key_set / api_key_masked)
// - llmLoaded:首次查询是否完成(用于避免横幅在查到结果前闪一下)
// - modelOptions:输入框模型按钮的下拉候选,点开菜单时懒加载
const llmConfigured = ref(false)
const llmLoaded = ref(false)
const llmConfig: Ref<LlmConfigView> = ref({
  model: FALLBACK_MODEL,
  effort: FALLBACK_EFFORT,
  api_key_set: false,
  api_key_masked: null,
  configured: false,
})
const modelOptions: Ref<ModelOption[]> = ref([])

// 账户余额是账户级信息,和配置共用同一把 API Key、同一组刷新时机(启动 / 保存配置后),
// 故放在这个 composable 里,而不是再建一份单例状态。null = 没 key 或查询失败,UI 显示「—」。
const balance: Ref<BalanceInfo | null> = ref(null)
// 余额请求的序号:并发刷新时只认最后一次的结果(见 refreshBalance)
let balanceRequest = 0

/** 保存入参用 camelCase(界面/表单侧习惯),发 IPC 前再映射成后端 snake_case。 */
export interface SaveLlmConfigInput {
  model?: string
  apiKey?: string
  effort?: Effort
}

export interface UseLlmConfig {
  llmConfig: Ref<LlmConfigView>
  llmConfigured: Ref<boolean>
  llmLoaded: Ref<boolean>
  modelOptions: Ref<ModelOption[]>
  balance: Ref<BalanceInfo | null>
  refresh: () => Promise<LlmConfigView>
  ensureModels: () => Promise<ModelOption[]>
  saveLlmConfig: (input?: SaveLlmConfigInput) => Promise<void>
  setEffort: (effort: Effort) => Promise<void>
  refreshBalance: () => Promise<void>
  refreshBalanceAfterTurn: () => void
}

/**
 * 大模型配置 composable:启动时查询配置状态,以及保存配置。
 * 落盘由后端 save_llm_config 负责(写 app.toml + 热更运行时全局),这里只管前端状态与交互。
 */
export function useLlmConfig(): UseLlmConfig {
  // 启动时(或保存后)向后端取一次配置状态
  async function refresh(): Promise<LlmConfigView> {
    const cfg = await invoke<LlmConfigView>('get_llm_config')
    llmConfig.value = cfg
    llmConfigured.value = cfg.configured
    llmLoaded.value = true
    // 顺带刷一次余额:换了 key 就等于换了账户,展示的余额必须跟着换。
    // 不 await —— 余额是纯展示,查不到不能拖住配置状态与横幅。
    void refreshBalance()
    // 顺带拉一次模型列表:输入框按钮要显示官方 name,而 name 只在 /models 里。
    // 以前只在点开下拉时拉,所以刚打开窗口时按钮会先露出一截持久化的 model id。
    // 同样不 await;失败不缓存,点开下拉会自动重试(见 ensureModels)。
    void ensureModels()
    return cfg
  }

  /**
   * 账户总可用余额。失败(没 key / 网络 / 上游)一律静默清空,状态栏显示「—」即可。
   * 启动刷新、保存 key、每轮对话收尾可能时间上重叠,故只认最后一次请求的结果,
   * 不让先发后到的旧响应覆盖新值。
   */
  async function refreshBalance(): Promise<void> {
    const seq = ++balanceRequest
    try {
      const value = await invoke<BalanceInfo>('get_balance')
      if (seq === balanceRequest) balance.value = value
    } catch {
      if (seq === balanceRequest) balance.value = null
    }
  }

  /**
   * 一轮对话收尾:立即刷一次,再延迟补刷一次。
   * 上游余额不是随请求即时结算的(官方未写明刷新延迟,响应还带 CDN 缓存头),
   * 对话刚结束那一次常拿到的是旧值;之后没有别的触发点,数字就会一直停在旧值。
   * 补刷间隔取 3s:够跨过结算/缓存,又不至于变成轮询。序号保护会丢掉先发后到的旧响应。
   */
  function refreshBalanceAfterTurn(): void {
    void refreshBalance()
    window.setTimeout(() => void refreshBalance(), 3000)
  }

  /**
   * 官方模型列表:第一次点开输入框的模型菜单时才拉,成功后本次会话复用。
   * 失败(没 key / 网络失败 / 空列表)不缓存、不弹错,菜单显示「暂不可用」,
   * 下次点开会自动重试——所以先填 key 保存、再点开菜单就能拿到。
   */
  async function ensureModels(): Promise<ModelOption[]> {
    if (modelOptions.value.length > 0) return modelOptions.value
    try {
      const list = await invoke<ModelOption[]>('list_models')
      modelOptions.value = list
      return list
    } catch {
      return []
    }
  }

  /**
   * 保存配置 / 切换模型。
   * apiKey 留空表示"沿用已保存的密钥"(后端语义),仅首次配置时必填;
   * model 不传时后端沿用当前生效值,不会把已选模型删掉。
   */
  async function saveLlmConfig({ model, apiKey, effort }: SaveLlmConfigInput = {}): Promise<void> {
    // 嵌套结构体字段按 Rust 字段名原样传(snake_case)
    const input: LlmConfigInput = { model, api_key: apiKey, effort }
    await invoke<void>('save_llm_config', { input })
    await refresh() // 保存后即时刷新状态,横幅与模型按钮据此更新
  }

  /**
   * 只切思考强度:后端其余字段缺省沿用当前值。
   * 不走 saveLlmConfig 的 refresh —— 换档位与账户 / 模型列表无关,
   * 没必要每拖一次就顺带刷余额、拉一次 /models;直接同步本地视图即可。
   */
  async function setEffort(effort: Effort): Promise<void> {
    const input: LlmConfigInput = { effort }
    await invoke<void>('save_llm_config', { input })
    llmConfig.value = { ...llmConfig.value, effort }
  }

  return {
    llmConfig,
    llmConfigured,
    llmLoaded,
    modelOptions,
    balance,
    refresh,
    ensureModels,
    saveLlmConfig,
    setEffort,
    refreshBalance,
    refreshBalanceAfterTurn,
  }
}
