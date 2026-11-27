//! 应用配置的 Tauri 命令层(IPC 入口)。
//!
//! 本模块只负责把前端的 `invoke` 请求翻译成对 `services` / `config` 的调用,
//! 不持有任何真源、也不直接读写 app.toml 或数据库。
//! 命令名即前端契约,不要随意改。

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tauri::State;

use crate::ai::llm::{
    get_balance as fetch_balance, list_models as fetch_models, BalanceInfo, ModelInfo,
};
use crate::config::llm::LlmConfig;
use crate::config::{llm_config, set_llm_config};
use crate::services::workspace as workspace_service;
use crate::store::Store;

/// 取当前工作区的展示信息(供前端显示名字、选择框定位到当前路径)。
///
/// 启动流程已经把工作区确定下来了,这里只读内存,不碰库。
#[tauri::command]
pub fn get_workspace() -> Option<WorkspaceView> {
    workspace_service::current().map(WorkspaceView::from)
}

/// 切换工作目录:定位并入库 → 注入边界根 → 写断点 → 清空当前对话。
///
/// 返回切换后的工作区,前端据此更新展示(名字是后端按目录名定的,不由前端猜)。
#[tauri::command]
pub async fn set_workspace(
    state: State<'_, Arc<Store>>,
    path: String,
) -> Result<WorkspaceView, String> {
    let ws = workspace_service::switch(state.inner(), &path)
        .map_err(|e| format!("设置工作目录失败: {e:#}"))?;
    // 切项目结束当前会话并清空对话(不同项目不共享历史/摘要)。
    // 走用例层而不是直接清上下文:会话指针也要一起清,否则下一条消息会写进
    // 属于旧目录的会话里。
    crate::services::conversation::reset().await;
    Ok(ws.into())
}

/// 列出工作区（最近打开在前），供侧边栏的工作区列表懒加载。
///
/// 分页参数由前端给，夹紧在 `services::workspace::list` 里——
/// 这一层只做 IPC 形状转换，不加策略。
#[tauri::command]
pub fn list_workspaces(
    state: State<'_, Arc<Store>>,
    offset: u32,
    limit: u32,
) -> Result<Vec<WorkspaceView>, String> {
    workspace_service::list(state.inner(), offset, limit)
        .map(|list| list.into_iter().map(WorkspaceView::from).collect())
        .map_err(|e| format!("读取工作区列表失败: {e:#}"))
}

/// 取大模型配置(脱敏视图):供前端启动时判断「是否已配置」并预填表单。
/// api_key 永不回明文,只回 `api_key_set` 与末 4 位掩码。
/// `model` 回的是**生效值**(文件里有就用,没有就是内置兜底),故前端恒非空。
#[tauri::command]
pub fn get_llm_config() -> LlmConfigView {
    let cfg = llm_config();
    let configured = cfg.is_configured();
    let effective_key = cfg.api_key();
    let api_key_masked = if effective_key.is_empty() {
        None
    } else {
        Some(format!(
            "…{}",
            &effective_key[effective_key.len().saturating_sub(4)..]
        ))
    };
    LlmConfigView {
        model: cfg.model(),
        effort: cfg.effort(),
        api_key_set: configured,
        api_key_masked,
        configured,
    }
}

/// 保存大模型配置:校验 → 持久化 app.toml → 热更新运行时全局(即时生效,无需重启)。
/// 端点是写死的,这里只收「选了哪个模型」「思考强度」和密钥;模型不再由用户手填。
///
/// `model` / `effort` 缺省时**沿用当前生效值**,不能留 None —— `set_llm_config` 的 `None`
/// 语义是「删键」;只改 API Key 时若把它们传成 None,会把用户选过的模型与档位一起抹掉。
#[tauri::command]
pub fn save_llm_config(input: LlmConfigInput) -> Result<(), String> {
    // 一次读快照:三个字段缺省都沿用「当前生效值」,不各自重读全局。
    let current = llm_config();

    let model = input
        .model
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| current.model());

    // api_key:本次留空则沿用已保存的(允许只换模型而不重填密钥);
    // 新旧皆空才视为未配置 —— 缺 api_key 必然调不通大模型。
    // 沿用值取运行时而非磁盘:两者应当一致,但运行时才是「实际生效」的那一份。
    let api_key = input
        .api_key
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| current.api_key.clone())
        .ok_or_else(|| "请填写 API Key".to_string())?;

    // 只认官方三档:别名 medium / xhigh / ultra 会被上游静默折成 high,
    // 存进文件就成了「以为切了档、实际没切」的假档位。非法值一律沿用当前值。
    let effort = input
        .effort
        .map(|s| s.trim().to_string())
        .filter(|s| matches!(s.as_str(), "low" | "high" | "max"))
        .unwrap_or_else(|| current.effort());

    // 落盘 + 热更新是一个用例(先落盘成功才生效),顺序在 config 域内保证
    set_llm_config(LlmConfig {
        model: Some(model),
        api_key: Some(api_key),
        effort: Some(effort),
    })
    .map_err(|e| format!("保存配置失败: {e}"))?;
    Ok(())
}

/// 配置弹窗打开时拉一次官方模型列表,供下拉选择 + 规格展示。
/// 后端自读已存的 API Key,前端零参数;没 key / 网络失败 / 列表为空都给 Err,
/// 前端统一降级成内置兜底选项,不弹错、不阻塞保存。
#[tauri::command]
pub async fn list_models() -> Result<Vec<ModelInfo>, String> {
    fetch_models().await.map_err(|e| e.to_string())
}

/// 底部状态栏展示的账户总可用余额。后端自读已存的 API Key，前端零参数；
/// 没 key / 网络失败 / 上游报错都给 Err，前端统一降级成「—」，不弹错、不阻塞对话。
#[tauri::command]
pub async fn get_balance() -> Result<BalanceInfo, String> {
    fetch_balance().await.map_err(|e| e.to_string())
}

/// `get_workspace` / `set_workspace` 返回给前端的工作区视图。
///
/// `name` 是入库时按目录名定下的显示名（用户改名会走另一条命令，暂未提供）；
/// `path` 只用于选择框定位与悬浮提示，界面上展示的是 `name`。
#[derive(Serialize)]
pub struct WorkspaceView {
    pub id: String,
    pub path: String,
    pub name: String,
}

impl From<workspace_service::Workspace> for WorkspaceView {
    fn from(ws: workspace_service::Workspace) -> Self {
        Self {
            id: ws.id,
            path: ws.path,
            name: ws.name,
        }
    }
}

/// `get_llm_config` 返回给前端的脱敏视图。
#[derive(Serialize)]
pub struct LlmConfigView {
    pub model: String,
    /// 生效思考强度:恒为 `low` / `high` / `max` 之一(配置域已归一化)。
    pub effort: String,
    pub api_key_set: bool,
    pub api_key_masked: Option<String>,
    pub configured: bool,
}

/// `save_llm_config` 接收的前端入参(明文 api_key 仅在此一跳,不落日志、不回显)。
#[derive(Deserialize)]
pub struct LlmConfigInput {
    pub model: Option<String>,
    pub api_key: Option<String>,
    /// 缺省 = 沿用当前档位;只认 `low` / `high` / `max`。
    pub effort: Option<String>,
}
