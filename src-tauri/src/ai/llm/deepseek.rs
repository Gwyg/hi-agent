//! 自研 DeepSeek 接口层：端点路径 + 请求/响应形状 + 5 个接口的业务语义。
//! 对话链路仍是把「消息列表」串到 `ChatResponse`，但发请求本身不在这里（见 `util/http`）。
//!
//! # 对话只给两个入口，调用方不构造请求对象
//!
//! 业务侧手上本来就只有 `Vec<Msg>`（接线后 `context` 里存的正是它），所以入口收
//! 消息列表，不收 `ChatRequest`。`model` / `api_key` / `effort` 来自全局配置、端点写死在
//! [`DEEPSEEK_BASE_URL`]、`stream` 由所选函数定死、其余参数取 `ChatRequest::new()` 的默认值；
//! 唯一还需要调用方给的是 `tools` —— 它不是配置，是「本次给模型哪些能力」的声明，
//! 只有调用方手里的 `Toolbox` 知道。压缩摘要不需要工具，所以 [`chat`] 干脆不收；
//! 它也不跟随用户选的思考强度，固定用端点默认（见 [`chat`]）。
//!
//! `ChatRequest` 因此退成**实现细节**：仍是 encode 的输入，但归本文件组装，不归调用方。
//!
//! ```text
//! agent / context → Vec<Msg> (+ tools / 回调)
//!      ↓ [本文件] send(): 读配置 → build() → encode → util::http::post → Response
//!      ↓ chat: text() → decode_full      chat_stream: SSE 逐帧 decode_frame + 累积
//!      ↓ ChatResponse
//! ```
//!
//! # 为什么是无状态函数，以及 HTTP 在哪
//!
//! 「发请求」下沉到 `util/http.rs`（连接池 + `get` / `post` / `json`），它不认识 DeepSeek，
//! 连 header 名都不认识；本文件只留端点路径、请求头怎么组、请求字段名、响应形状与 SSE
//! 解码。早期只有对话两个入口时，HTTP 写在这里没问题；加了 `/models`、`/user/balance`、
//! `/v1/tools/*` 之后再放一起就是职责越界。
//!
//! 本层仍不需要 `LlmClient` 那样的对象：连接配置是全局单例、每次读就是最新的，既没有要
//! 缓存的会话状态，也没有需要长期持有的业务对象。用对象虽然能让签名更短，但要把对象构造
//! 并传一圈，而且 `tools` 是会话级、`model` 是热更新配置，生命周期不同塞进一个对象就得靠
//! 「每轮重建」绕开；函数式每次读 `llm_config()`，热更新天然最新。
//!
//! `protocol/` 的 encode / decode 是纯翻译（可单测、不含 I/O），所以保留独立。
//! SSE 的字节层分帧（空行派发、多行 `data:` 拼接、CRLF、UTF-8 边界）交给
//! `eventsource-stream`——自己按 chunk 切会踩多字节字符边界；本文件只留 [`step`]
//! 这个纯函数过滤心跳 / 空帧，`[DONE]` 由循环 `break`（结束是动作，不产生增量）。
//!
//! [`chat`] / [`chat_stream`] 各自把 `stream` 定死为 `false` / `true`，
//! 「选了流式却发出非流式请求」这种错不再可能。
//!
//! # 与 agent 的边界
//!
//! agent 循环持 `CancellationToken`，但本层**还没接 abort**：现在取消靠 select!
//! 丢掉 future，连接会挂到超时才断。接法留到取消那一轮再做，这里不预留没人传的
//! 参数。重试（429/5xx）同理，要按「已吐出的字不能简单重来」一起定；本层不发请求重试。
//!
//! # 失败一律是一条字符串
//!
//! 这里不定义错误类型：非 2xx 的上游原文、网络 / 分帧 / 解码失败、缺 `[DONE]`
//! 的截断流，全部 `anyhow!("...")` 带出。前端显示与将来 agent 回灌用的是同一条
//! 文本，哪些错误值得塞回历史由 agent 循环决定；分类 / 重试 / 部分响应等真有
//! 消费者再加，不预留。
//!
//! # 模型列表 / 账户余额是只读辅助
//!
//! [`list_models`] 给输入框的模型菜单拉候选，[`get_balance`] 给底部状态栏拉总可用余额。
//! 两者都不进配置、不缓存、不参与对话请求；拉不到由前端静默降级，对话链路一行都不依赖它。
//!
//! # 联网检索是独立原始接口
//!
//! [`web_search`] / [`web_fetch`] 打的是 `/v1/tools/*`，**不在 `chat/completions` 里**，
//! 由 `tools/` 下两个工具调用，结果作为普通 tool 结果回灌。本层认端点路径、请求头怎么组、
//! 请求字段名与响应形状；发送与上游错误原文归 `util/http.rs`，参数校验、结果裁剪、给模型
//! 看的文本全归工具层。它们不进 `types::Msg`，也不参与对话请求的组装。
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde::Serialize;
use serde_json::Value;
use std::time::Duration;

use crate::ai::llm::protocol;
use crate::ai::llm::types::{
    ChatRequest, ChatResponse, Delta, Effort, Msg, StreamAccumulator, ToolDef,
};
use crate::config::llm_config;
use crate::util::http;

/// DeepSeek 官方端点。专做一家之后不再是可配置项，也不提供代理 / 中转覆盖；
/// 对话、模型列表、余额、联网检索共 5 个路径都拼在它后面。
const DEEPSEEK_BASE_URL: &str = "https://api.deepseek.com";

/// 业务侧公共前半段：判空 key + 拼完整 URL。**不碰 HTTP**，发出去交给 `util::http`。
/// 端点路径和「没配 Key 该怎么办」都属本层的业务知识。
fn endpoint(key: &str, path: &str, what: &str) -> anyhow::Result<String> {
    if key.is_empty() {
        return Err(anyhow::anyhow!("尚未配置 API Key，无法{what}"));
    }
    Ok(format!("{DEEPSEEK_BASE_URL}{path}"))
}

/// 组装请求头。header 名与取值（Bearer 鉴权、JSON body）都是 DeepSeek 侧的业务知识，
/// `util/http` 只按 `(名称, 值)` 原样写出；将来加自定义头只在此追加一项，不动发送层签名。
fn request_headers(key: &str, json_body: bool) -> Vec<(&str, String)> {
    let mut headers = vec![("Authorization", format!("Bearer {key}"))];
    if json_body {
        headers.push(("Content-Type", "application/json".to_string()));
    }
    headers
}

/// 配置里的生效档位 → 请求侧的 `Effort`。`high` 是官方默认，**返回 `None` 即不发
/// `reasoning_effort` 键**，让请求体与未接入思考强度时字节一致（不打扰缓存前缀）。
/// 配置域已归一化脏值，这里对未知值仍兜底为 None。
fn effort_of(value: &str) -> Option<Effort> {
    match value {
        "low" => Some(Effort::Low),
        "max" => Some(Effort::Max),
        _ => None,
    }
}

/// 组装完整请求：默认值只在 `ChatRequest::new()` 一处兜，两个入口共用。
/// `tools` 为空时 `openai/req.rs` 不发 `tools` 键（压缩摘要正好如此）。
/// `effort` 由调用方给：主对话读配置，压缩摘要恒 `None`。
fn build(
    model: &str,
    messages: Vec<Msg>,
    tools: Vec<ToolDef>,
    stream: bool,
    effort: Option<Effort>,
) -> ChatRequest {
    let req = ChatRequest::new(model, messages)
        .with_tools(tools)
        .with_stream(stream);
    match effort {
        Some(e) => req.with_effort(e),
        None => req,
    }
}

/// 两个对话入口的公共前半段：读配置 → 组装 → 编码 → 交给 `util::http` 发出去。
/// 差别只在怎么消费返回的 `Response`（流式要 `bytes_stream()`，这里绝不能读成文本）。
async fn send(
    messages: Vec<Msg>,
    tools: Vec<ToolDef>,
    stream: bool,
    effort: Option<Effort>,
) -> anyhow::Result<reqwest::Response> {
    let cfg = llm_config();
    let key = cfg.api_key();
    let url = endpoint(&key, "/chat/completions", "对话请求")?;
    let headers = request_headers(&key, true);
    let body = protocol::encode(&build(&cfg.model(), messages, tools, stream, effort))
        .map_err(|e| anyhow::anyhow!("请求体编码失败: {e}"))?;
    http::post(&url, &headers, &body, None, "对话请求").await
}

/// 非流式：压缩摘要这类一次性请求用。不带工具、`stream` 定死 `false`，
/// 思考强度也恒 `None`（用端点默认 high）——摘要只是把旧历史压成一段文字，
/// 跟随用户选的 max 会让每轮压缩都白烧一份最大思考预算。
pub(crate) async fn chat(messages: Vec<Msg>) -> anyhow::Result<ChatResponse> {
    let text = send(messages, Vec::new(), false, None)
        .await?
        .text()
        .await
        .map_err(|e| anyhow::anyhow!("读取 DeepSeek 响应失败: {e}"))?;
    protocol::decode_full(&text).map_err(|e| anyhow::anyhow!("DeepSeek 响应解码失败: {e}"))
}

/// 流式对话：`stream` 定死 `true`，`tools` 由调用方给。同一批 `Delta` 一边逐项交给
/// `on_delta`（推前端 / 事件），一边 push 给累积器；只有见到 `[DONE]` 才收尾产出
/// 完整 `ChatResponse`。结束是动作，不产生增量。
pub(crate) async fn chat_stream(
    messages: Vec<Msg>,
    tools: Vec<ToolDef>,
    mut on_delta: impl FnMut(&Delta),
) -> anyhow::Result<ChatResponse> {
    // 只有主对话跟随用户选的档位；high 折成 None → 不发 reasoning_effort 键，
    // 请求体与未接入思考强度时字节一致，不额外打断缓存前缀。
    let effort = effort_of(&llm_config().effort());
    let resp = send(messages, tools, true, effort).await?;

    let mut events = resp.bytes_stream().eventsource();
    let mut acc = StreamAccumulator::default();
    let mut saw_done = false;
    while let Some(item) = events.next().await {
        let event = item.map_err(|e| anyhow::anyhow!("SSE 分帧失败: {e}"))?;
        let Some(data) = step(&event.data) else {
            continue;
        };
        if data == "[DONE]" {
            saw_done = true;
            break; // 结束是动作，不是增量；不要继续读后续帧
        }
        for delta in
            protocol::decode_frame(data).map_err(|e| anyhow::anyhow!("SSE 帧解码失败: {e}"))?
        {
            on_delta(&delta);
            acc.push(delta);
        }
    }
    // `eventsource-stream` 遇到底层 EOF 只返回 `None`、不报错；少了 `[DONE]` 说明流
    // 被截断，不能把半截响应当成功交给 agent。错误仍是一条字符串，接线后可直接用。
    if !saw_done {
        return Err(anyhow::anyhow!(
            "DeepSeek 流提前结束（未收到 [DONE]），本轮响应不完整"
        ));
    }
    Ok(acc.finish())
}

/// 官方 `GET /models` 的单条信息，只挑配置页展示用得到的字段。
/// `effort` 等请求侧一律不发的元数据不进这里——展示归展示，请求形状仍是我们写死的那一份。
#[derive(Debug, Serialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub context_window: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub input_modalities: Vec<String>,
}

/// 两个只读 GET 的公共路径：拼 URL → `util::http::get` → `json`。
/// `/models` 与 `/user/balance` 形状不同但取数方式一样；这里不碰任何 HTTP 细节。
async fn fetch(path: &str, what: &str) -> anyhow::Result<Value> {
    let key = llm_config().api_key();
    let url = endpoint(&key, path, what)?;
    let headers = request_headers(&key, false);
    let resp = http::get(&url, &headers, None, what).await?;
    http::json(resp, what).await
}

/// 配置页打开时拉一次模型列表：后端自读已存的 key，前端一个参数都不传。
/// 没 key / 网络失败 / 上游报错 / 列表为空都返回 `Err`，由命令层交给前端降级，
/// 不阻塞保存与对话（对话请求根本不经过这里）。
pub(crate) async fn list_models() -> anyhow::Result<Vec<ModelInfo>> {
    let raw = fetch("/models", "获取模型列表").await?;

    // 与响应侧同一套 Value 手工取值：缺键 / null / 类型不符归一成缺省，
    // 不为了一个只读列表再维护一层 wire struct。
    let models: Vec<ModelInfo> = match raw.pointer("/data").and_then(Value::as_array) {
        Some(items) => items.iter().filter_map(model_of).collect(),
        None => Vec::new(),
    };
    if models.is_empty() {
        return Err(anyhow::anyhow!("模型列表为空或格式已变化"));
    }
    Ok(models)
}

/// 单条模型 → 展示结构。`id` 是唯一必需项，缺了整条跳过（没有 id 的下拉选不了）。
fn model_of(v: &Value) -> Option<ModelInfo> {
    let id = v.get("id")?.as_str()?.to_string();
    if id.is_empty() {
        return None;
    }
    // 先借用 id 兜出 name,再把 id 移进结构体
    let name = v
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(id.as_str())
        .to_string();
    Some(ModelInfo {
        id,
        name,
        context_window: v
            .get("context_window")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok()),
        max_output_tokens: v
            .get("max_output_tokens")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok()),
        input_modalities: v
            .get("input_modalities")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// 官方 `GET /user/balance` 里唯一要展示的：总可用余额 `total_balance`。
/// 赠金 / 充值拆分、`is_available` 都不展示，也就不进这个结构（不留没人用的字段）。
#[derive(Debug, Serialize)]
pub struct BalanceInfo {
    pub currency: String,
    pub total_balance: String,
}

/// 状态栏展示账户余额：后端自读已存的 key，前端零参数。
/// 失败由命令层交给前端静默降级成「—」，不弹错、不阻塞对话。
pub(crate) async fn get_balance() -> anyhow::Result<BalanceInfo> {
    let raw = fetch("/user/balance", "获取账户余额").await?;
    balance_of(&raw)
        .ok_or_else(|| anyhow::anyhow!("账户余额响应缺少 total_balance，格式可能已变化"))
}

/// 从 `balance_infos` 取第一条：真机上账户只有一条币种余额；真出现多币种也不能
/// 跨币种相加，只展示第一条。金额保留上游字符串原样（如 `"11.80"`），不转浮点。
fn balance_of(raw: &Value) -> Option<BalanceInfo> {
    let first = raw.pointer("/balance_infos/0")?;
    Some(BalanceInfo {
        currency: first.get("currency")?.as_str()?.to_string(),
        total_balance: first.get("total_balance")?.as_str()?.to_string(),
    })
}

/// 联网检索按请求超时。`util/http` 的 600s `read_timeout` 是流式回答的兜底，
/// 一次搜索 / 抓取卡十分钟不可接受；取消时本 future 被 drop，连接随之断开，无需额外收尾。
const TOOLS_TIMEOUT: Duration = Duration::from_secs(30);

/// Search / Fetch 两个原始接口的公共 POST：拼 URL → `util::http::post`(30s) → `json`。
/// 与只读 GET 的 [`fetch`] 对称，差别只在带请求体、且要按请求单独设超时。
/// 这里只碰 wire（端点、字段名）：不校验业务参数、不裁剪结果、不重试。
async fn post_tools(path: &str, body: &Value, what: &str) -> anyhow::Result<Value> {
    let key = llm_config().api_key();
    let url = endpoint(&key, path, what)?;
    let headers = request_headers(&key, true);
    let resp = http::post(&url, &headers, &body.to_string(), Some(TOOLS_TIMEOUT), what).await?;
    http::json(resp, what).await
}

/// `POST /v1/tools/search`。`need_content` 固定不传（官方默认 false）：正文交给
/// [`web_fetch`]，避免一次拉回几十 KB；`ori_question` / `localization` 本项目用不到，
/// 请求体里的 `site:` / `lang:` 等算子由模型直接写进 `query`，这里原样透传不解析。
pub(crate) async fn web_search(query: &str, count: u32) -> anyhow::Result<Value> {
    let body = serde_json::json!({ "query": query, "count": count });
    post_tools("/v1/tools/search", &body, "联网搜索").await
}

/// `POST /v1/tools/fetch`。url 的合法性由工具层用 `reqwest::Url` 校验，这里不重复。
pub(crate) async fn web_fetch(url: &str) -> anyhow::Result<Value> {
    let body = serde_json::json!({ "url": url });
    post_tools("/v1/tools/fetch", &body, "网页抓取").await
}

/// SSE `data` → trim 后的负载；`None` = 空帧或 `:` 注释行（心跳），本帧不产生增量。
///
/// 字节层分帧由 `eventsource-stream` 保证，这里只做过滤 + trim，可脱离网络单测。
/// `[DONE]` 不过滤：它要交给调用方 `break`，与「跳过这一帧」是两种动作。
fn step(data: &str) -> Option<&str> {
    let data = data.trim();
    if data.is_empty() || data.starts_with(':') {
        None
    } else {
        Some(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_joins_path_and_rejects_empty_key() {
        assert_eq!(
            endpoint("k", "/models", "获取模型列表").unwrap(),
            "https://api.deepseek.com/models"
        );
        // 对话入口也走这里：没配 Key 时不再发出一个 Bearer 为空的请求。
        assert!(endpoint("", "/chat/completions", "对话请求")
            .unwrap_err()
            .to_string()
            .contains("尚未配置 API Key"));
    }

    #[test]
    fn request_headers_adds_content_type_only_for_json_body() {
        let get = request_headers("sk-1", false);
        assert_eq!(get, vec![("Authorization", "Bearer sk-1".to_string())]);

        let post = request_headers("sk-1", true);
        assert_eq!(
            post,
            vec![
                ("Authorization", "Bearer sk-1".to_string()),
                ("Content-Type", "application/json".to_string()),
            ]
        );
    }

    #[test]
    fn build_applies_defaults_and_call_shape() {
        let req = build("m", vec![Msg::user("hi")], Vec::new(), true, None);
        assert_eq!(req.model, "m");
        assert_eq!(req.messages.len(), 1);
        assert!(req.stream);
        assert!(req.include_usage);
        assert!(req.tools.is_empty()); // 摘要场景：encode 不会发 tools 键
        assert!(req.max_tokens.is_none());
        assert!(req.temperature.is_none());
        assert!(req.effort.is_none());

        let non_stream = build("m", Vec::new(), Vec::new(), false, None);
        assert!(!non_stream.stream);
    }

    /// 只有 low / max 才发 `reasoning_effort`；high（官方默认）与脏值都不发键，
    /// 保证选默认档时请求体字节不变、不打断缓存前缀。
    #[test]
    fn effort_only_emits_non_default_levels() {
        assert_eq!(effort_of("low"), Some(Effort::Low));
        assert_eq!(effort_of("max"), Some(Effort::Max));
        assert_eq!(effort_of("high"), None);
        assert_eq!(effort_of("medium"), None);

        let low = build(
            "m",
            vec![Msg::user("hi")],
            Vec::new(),
            true,
            Some(Effort::Low),
        );
        let body: serde_json::Value =
            serde_json::from_str(&protocol::encode(&low).unwrap()).unwrap();
        assert_eq!(body["reasoning_effort"], "low");

        let high = build("m", Vec::new(), Vec::new(), true, None);
        let body = protocol::encode(&high).unwrap();
        assert!(!body.contains("reasoning_effort"));
    }

    #[test]
    fn done_is_passed_through_so_the_loop_can_break() {
        assert_eq!(step("[DONE]"), Some("[DONE]"));
        assert_eq!(step("  [DONE]  "), Some("[DONE]"));
    }

    #[test]
    fn heartbeats_and_empty_data_are_ignored() {
        assert!(step("").is_none());
        assert!(step("   ").is_none());
        assert!(step(": ping").is_none());
    }

    #[test]
    fn json_data_is_passed_through_trimmed() {
        assert_eq!(step(r#"  {"choices":[]}  "#), Some(r#"{"choices":[]}"#));
    }

    #[test]
    fn model_of_takes_only_display_fields() {
        let v = serde_json::json!({
            "id": "deepseek-flash",
            "name": "DeepSeek-V4.1-Flash",
            "context_window": 1048576,
            "max_output_tokens": 393216,
            "input_modalities": ["text", "image"],
            "effort": { "default_level": "high" }
        });
        let m = model_of(&v).expect("有 id 就该解析出来");
        assert_eq!(m.id, "deepseek-flash");
        assert_eq!(m.name, "DeepSeek-V4.1-Flash");
        assert_eq!(m.context_window, Some(1_048_576));
        assert_eq!(m.max_output_tokens, Some(393_216));
        assert_eq!(
            m.input_modalities,
            vec!["text".to_string(), "image".to_string()]
        );
    }

    #[test]
    fn model_of_skips_missing_id_and_tolerates_missing_fields() {
        assert!(model_of(&serde_json::json!({ "name": "无 id" })).is_none());
        assert!(model_of(&serde_json::json!({ "id": "" })).is_none());
        let m = model_of(&serde_json::json!({ "id": "x", "context_window": null })).unwrap();
        assert_eq!(m.name, "x"); // name 缺了退回 id
        assert_eq!(m.context_window, None);
        assert!(m.input_modalities.is_empty());
    }

    #[test]
    fn balance_of_takes_total_balance_as_plain_string() {
        let v = serde_json::json!({
            "is_available": true,
            "balance_infos": [{
                "currency": "CNY",
                "total_balance": "11.80",
                "granted_balance": "0.01",
                "topped_up_balance": "11.78"
            }]
        });
        let b = balance_of(&v).expect("官方形状该解析出来");
        assert_eq!(b.currency, "CNY");
        assert_eq!(b.total_balance, "11.80"); // 不转数字、不丢末尾 0
    }

    #[test]
    fn balance_of_rejects_missing_or_empty_infos() {
        assert!(balance_of(&serde_json::json!({ "balance_infos": [] })).is_none());
        assert!(balance_of(&serde_json::json!({})).is_none());
        assert!(balance_of(&serde_json::json!({
            "balance_infos": [{ "currency": "USD" }]
        }))
        .is_none());
    }
}
