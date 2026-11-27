//! 通用 HTTP 发送层：只负责「把一次请求发出去、把响应原样交回来」。
//!
//! 不认识任何业务：不知道域名、端点路径、API Key 从哪来、响应 JSON 里有哪些字段，
//! 也不认识任何 header 名 —— 请求头由调用方整份给，本层只按 `(名称, 值)` 原样写出。
//! 完整 URL、请求头、已编码的 body、按请求超时全由调用方给（当前是 `ai::llm::deepseek`）。
//! 这样「发请求」不绑在任何一家厂商上，`deepseek.rs` 只留报文形状与业务语义。
//!
//! # 为什么 get / post 返回 Response 而不是 Value
//!
//! `chat_stream` 要 `bytes_stream()` 逐帧读 SSE，在这里把 body 读成 `text()` 流就没了。
//! 需要 JSON 的调用方再接一个 [`json`]，其余原样拿走。
//!
//! 只提供 GET / POST：项目只有这两种，没有调用方的方法不留。body 一律是调用方已编码好的
//! 字符串（字段名归调用方），不做 form / multipart。headers 用 `(名称, 值)` 列表整份传入，
//! 将来换鉴权方式、加自定义头都不用改本层签名。

use reqwest::{Client, Method, Response};
use serde_json::Value;
use std::sync::LazyLock;
use std::time::Duration;

/// 连接池全局单例，多轮工具循环复用 TLS 握手。
///
/// `read_timeout` 是 reqwest 两阶段共用的值：等响应头是一次性上限，收到头之后按帧间 idle
/// 重置。取 10 分钟是「正常结果绝不因超时丢失」的宽松兜底，不是业务计时器；某类请求要更严
/// 的墙钟上限，由调用方传 `timeout`（当前联网检索是 30s）。不用总时长 `timeout()` 兜底，
/// 那样长时间流式响应会被误杀。
static CLIENT: LazyLock<Client> = LazyLock::new(|| {
    Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(600))
        .build()
        .expect("构造 HTTP 客户端失败")
});

/// `get` / `post` 共用的发送实现。`what` 只是调用方给的错误标签，本层不解释。
/// headers 由调用方整份给出，本层按名字原样写出，不预设也不补充任何一个。
/// 非 2xx 时读完错误体再返回：上游原文是调用方判断该怎么改的唯一依据，不压成状态码。
async fn send(
    method: Method,
    url: &str,
    headers: &[(&str, String)],
    body: Option<&str>,
    timeout: Option<Duration>,
    what: &str,
) -> anyhow::Result<Response> {
    let mut req = CLIENT.request(method, url);
    for (name, value) in headers {
        req = req.header(*name, value.as_str());
    }
    if let Some(body) = body {
        req = req.body(body.to_owned());
    }
    if let Some(t) = timeout {
        req = req.timeout(t);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("{what}失败: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        // 连错误体都读不出来时，把这个原因也写进同一条字符串，别让消息尾变成空白。
        let text = resp
            .text()
            .await
            .unwrap_or_else(|e| format!("<错误体读取失败: {e}>"));
        return Err(anyhow::anyhow!("{what}失败: HTTP {status}: {text}"));
    }
    Ok(resp)
}

/// GET：`headers` 整份由调用方给，本层不补任何头。
pub async fn get(
    url: &str,
    headers: &[(&str, String)],
    timeout: Option<Duration>,
    what: &str,
) -> anyhow::Result<Response> {
    send(Method::GET, url, headers, None, timeout, what).await
}

/// POST：`body` 是调用方已编码好的字符串，字段名、`Content-Type` 等请求头都由调用方决定。
pub async fn post(
    url: &str,
    headers: &[(&str, String)],
    body: &str,
    timeout: Option<Duration>,
    what: &str,
) -> anyhow::Result<Response> {
    send(Method::POST, url, headers, Some(body), timeout, what).await
}

/// 读完整响应体并解成 `Value`。只做「文本 → Value」，不认识任何字段。
pub async fn json(resp: Response, what: &str) -> anyhow::Result<Value> {
    let text = resp
        .text()
        .await
        .map_err(|e| anyhow::anyhow!("{what}响应读取失败: {e}"))?;
    serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{what}结果解码失败: {e}"))
}
