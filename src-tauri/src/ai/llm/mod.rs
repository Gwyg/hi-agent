//! LLM 层：DeepSeek 专用客户端 + 中立模型 + 协议编解码。
//!
//! # 范围（2026-11-10 定）
//!
//! 只接 `https://api.deepseek.com/chat/completions` 的 OpenAI Chat Completions 形状，
//! 不做多模型 / 多厂商适配、不建端点方言表、不做 (端点, 模型) 匹配 —— 各家私有扩展
//! 没有统一规范，也无法从请求或配置推断，只能靠持续维护一张会过期的表，与「一次只配
//! 一个端点」的桌面应用不匹配。形状按 DeepSeek 口径在 `openai/req.rs` 一次写死；
//! 不建表后，**上游 400 时原样回显错误原文**是用户唯一能自救的路径，勿压缩成状态码。
//!
//! # 结构
//!
//! | 模块 | 职责 | 认识 | 不认识 |
//! |---|---|---|---|
//! | `types/` | 中立模型(纯数据) | `Msg`、`ToolDef`、`ChatResponse`、`Delta` | JSON 字段名、HTTP |
//! | `protocol/` | 中立模型 ↔ 线上 JSON(纯函数) | 报文形状 | HTTP、超时、重试 |
//! | `deepseek.rs` | 端点路径 + encode + decode + SSE 串联 + 5 个接口语义 | 报文形状 | HTTP 怎么发 |
//! | `tools/` | 工具声明 + 执行 + 安全裁决 | `ToolDef` 是什么 | 线上报文 |
//!
//! **HTTP 发送不属于 LLM 层**：`util/http.rs`（通用工具层）持有连接池，只收完整 URL /
//! 请求头列表 / body / 超时，返回原始 `Response`，连 header 名都不认识；`deepseek.rs` 是
//! 唯一知道 DeepSeek 路径、Key 来源与请求头怎么组的地方。早期只有对话两个入口时写在一起，
//! 加了 `/models` / `/user/balance` / `/v1/tools/*` 之后就是职责越界，故拆开。将来接
//! 取消 / 重试也只改 `util/http.rs` 一处。
//!
//! `types/` 是**解耦层不是兼容层**：即便只做 DeepSeek，agent 循环、上下文压缩、
//! 工具回灌也不该认识线上字段名。`protocol/` 编译 `openai/`（DeepSeek 形状）与
//! `anthropic/`（完整但不再投入的存档）；响应侧只实现 DeepSeek 形状，不做协议分派。
//!
//! # 对话只有两个无状态函数
//!
//! ```text
//! agent / context → chat(Vec<Msg>) 或 chat_stream(Vec<Msg>, Vec<ToolDef>, on_delta)
//!      ↓ send(): 读配置 → build() → encode → util::http::post → Response
//!      ↓ chat: text() → decode_full   chat_stream: SSE 逐帧 decode_frame + 累积
//!      ↓ ChatResponse{ message: Msg, finish_reason, usage }
//! ```
//!
//! 调用方只给消息列表（流式再多给 `tools` 与逐 token 回调），**不构造 `ChatRequest`**：
//! `model` / `api_key` 由 `send()` 读一次全局配置、`effort` 由 `chat_stream()` 读配置
//! （压缩摘要恒用端点默认 `high`，不跟随用户档位）、端点写死在 `deepseek.rs`、
//! `stream` 由所选函数定死、其余取 `ChatRequest::new()` 默认值。`tools` 必须由调用方给 —— 它是
//! 「本次给模型哪些能力」的声明，只有 `Toolbox` 知道；压缩摘要不收工具。
//! 不需要 `LlmClient` 那样的对象：连接配置是全局单例，没有要缓存的会话状态。
//! `list_models()`（模型菜单候选）与 `get_balance()`（状态栏账户余额）都是只读辅助路径，
//! 不进配置、不参与对话请求，拉不到由前端静默降级。
//! `web_search()` / `web_fetch()` 是同层另外两个原始 HTTP 接口（`/v1/tools/*`），供
//! `tools/` 下的联网工具调用；它们不进 `chat/completions`、不构造 `Msg`，结果由工具层
//! 整理成普通 tool 结果回灌——与官方「服务端联网工具」不是一回事。
//!
//! SSE 字节层分帧（空行、多行 `data`、CRLF、UTF-8 边界）交给 `eventsource-stream`；
//! 本层只过滤心跳 / 空帧，`[DONE]` 由循环 `break`，且**缺 `[DONE]` 一律报错** ——
//! 底层 EOF 不报错，不能把半截响应当成功。`protocol` 一帧直接交出 `Vec<Delta>`，
//! 上层在同一个循环里一边推前端、一边 `acc.push`，[DONE] 时 `finish()` 产出完整响应。
//!
//! # 接线现状（2026-11-15）
//!
//! agent / context / tools / services 已全部基于 `types/`，`async-openai` 已从依赖移除。
//! 旧的 `client.rs` / `message.rs`（async-openai 客户端与消息构造器）已删除；`llm/` 下
//! 只剩 `deepseek.rs` / `types/` / `protocol/` / `tools/` 四个在编模块，没有旁支存档。
//! 命令层 `commands/chat.rs` 一行未改：`AgentEvent` 形态不变，只有 `Delta::Text`
//! 翻成 `AgentEvent::Token`。
//!
//! ## 必须守住的两条线上约束
//!
//! - **历史思考全量回传**：带 `tools` 的请求里，每一条历史 assistant 都必须带
//!   `reasoning_content`，否则 DeepSeek 直接 400。所以 agent 落历史时一律用
//!   `ChatResponse.message` 本体（自带 `thinking`），不要重新构造一个只有正文的
//!   assistant 消息。验收场景必须是「开思考 + 多轮工具调用」。
//! - **assistant.content 恒写字符串**：空正文写 `""` 且不省略键，不写 `null`。
//!   响应侧 `null` / `""` / 缺键一律折成空串；`refusal` 由协议层折进正文并打 warn。
//!
//! # 未定项
//!
//! - [ ] **取消**：`CancellationToken` 接到 reqwest 的 abort，不能只靠丢弃 future
//!   —— 否则连接挂到超时、token 白烧。
//! - [ ] **重试**：429 / 5xx 要不要重试；重试时 SSE 已吐出的内容怎么处理
//!   （用户已经看到一半的字，不能简单重来）。
//! - [ ] **错误回灌**：非 2xx 原文、网络 / 分帧失败现在都只发 `llm-error` 就结束整轮，
//!   字符串没进历史。哪些值得塞回模型由 agent 循环定；**错误不分类、不定义枚举**。
//! - [ ] **专用化清理**：`anthropic/` 降成不编译存档、删
//!   `Delta::Signature` / `Thinking.signature` / `ToolDef::Custom` / `ThinkingMode`、
//!   `openai/` 改名 `deepseek/`，并清掉 `types/` 注释里残留的 Anthropic 对比。
//!   清理前 `types/` 顶部的 `#![allow(dead_code)]` 要留着。

mod deepseek;
mod protocol;
pub mod tools;
pub mod types;

pub(crate) use deepseek::{
    chat, chat_stream, get_balance, list_models, web_fetch, web_search, BalanceInfo, ModelInfo,
};

/// 官方 `GET /models` 返回的 `context_window`:`deepseek-flash` / `deepseek-v4-pro`
/// 均为 1048576(1Mi)。查证 2026-11-14:
/// https://api-docs.deepseek.com/zh-cn/api/list-models
///
/// 只接一家、当前两个模型同规格,不建 model → 窗口映射表;Chat Completions 响应
/// 也不返回窗口大小。这个常量既做「上下文用量」百分比的分母,也做压缩水位的
/// 基准(见 `agent/context/compaction.rs`);压缩判定看的是真实 `input_tokens`,
/// 与这里的口径一致。
pub(crate) const MODEL_CONTEXT_WINDOW: u32 = 1_048_576;
