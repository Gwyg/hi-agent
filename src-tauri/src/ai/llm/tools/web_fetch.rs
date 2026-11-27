//! 网页抓取工具：把 DeepSeek `POST /v1/tools/fetch` 包成一个普通 tool。
//!
//! 只负责「抓哪个 URL」的契约校验，以及把 `{title, content}` 整理成给模型看的一段文本。
//! 端点路径、请求字段名、鉴权、错误原文都在 `ai::llm::deepseek`。不做 URL 白名单 /
//! 内网拦截（与 bash 不做路径校验同一口径），目标站点能不能抓由上游安全策略决定。
//!
//! Fetch 没有分页语义，所以**不提供 offset/limit 这类假接口**；正文超长直接截断并在
//! 尾注说明。结果以普通 tool 结果回灌、永久进历史，必须封顶。

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use super::support::text::truncate_utf8;
use super::{Action, Tool};
use crate::ai::llm::types::ToolDef;
use crate::ai::llm::web_fetch as fetch_page;

/// 官方契约：url ≤ 8192 字符，必须是含 host 的 http/https，且不能带用户名或密码。
const MAX_URL_CHARS: usize = 8192;
/// 与 `read` 对齐的单次输出体积上限：约 8k token，不会一次把整轮对话推到压缩线上。
const MAX_OUTPUT_BYTES: usize = 32 * 1024;

pub struct WebFetchTool;

#[derive(Deserialize)]
struct Args {
    #[serde(default)]
    url: String,
}

impl WebFetchTool {
    pub fn new() -> Self {
        Self
    }
}

/// 参数校验只写一处：assess 与 execute 共用。返回规范化后要发出的 URL 原文。
fn parse(args: &str) -> anyhow::Result<String> {
    let args: Args =
        serde_json::from_str(args).map_err(|e| anyhow::anyhow!("web_fetch 参数解析失败: {e}"))?;
    let url = args.url.trim().to_string();
    if url.is_empty() {
        return Err(anyhow::anyhow!("url 不能为空"));
    }
    if url.chars().count() > MAX_URL_CHARS {
        return Err(anyhow::anyhow!("url 超过 {MAX_URL_CHARS} 个字符"));
    }
    let parsed = reqwest::Url::parse(&url).map_err(|e| anyhow::anyhow!("URL 无效: {e}"))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(anyhow::anyhow!("URL 必须是 http 或 https"));
    }
    if parsed.host_str().is_none() {
        return Err(anyhow::anyhow!("URL 缺少 host"));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(anyhow::anyhow!("URL 不能携带用户名或密码"));
    }
    Ok(url)
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "web_fetch",
            format!(
                "联网抓取一个已知 URL 的网页正文(Markdown 文本)。只读,不访问本地文件。\n\
                 适用:web_search 找到链接后读详情;读指定文档页、新闻页、API 文档。\n\
                 不适用:找链接用 web_search;读本地文件用 read。\n\
                 url 必须是含 host 的 http/https 地址,不能带用户名密码。正文最多 {}KB,超出会截断;\n\
                 Fetch 不支持分页,不要试图用 offset/limit 续读,换更具体的页面或换来源。",
                MAX_OUTPUT_BYTES / 1024
            ),
            serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "要抓取的网页地址,含 host 的 http/https URL"
                    }
                },
                "required": ["url"]
            }),
        )
    }

    /// 合法 URL 一律 Allow：只读外部请求，调用即授权，不打断 agent 循环。
    async fn assess(&self, args: &str) -> Action {
        match parse(args) {
            Ok(_) => Action::Allow,
            Err(e) => Action::Deny(e.to_string()),
        }
    }

    /// 取消 = 本 future 被 drop；reqwest 没有脱离 future 的工作，连接随之断开。
    /// 上游错误一律 `Err`，由 agent 循环统一折成 tool 结果回灌，不中断整轮；不重试。
    async fn execute(&self, args: &str) -> anyhow::Result<String> {
        let url = parse(args)?;
        let raw = fetch_page(&url).await.map_err(|e| {
            anyhow::anyhow!(
                "网页抓取失败: {e} —— 可换 URL 后重试;404 表示该页无可提取正文,403 是被安全策略拒绝;\
                 408/429/5xx 稍后重试;402 需检查账户余额;其他 4xx 不要原样重试"
            )
        })?;
        format_page(&raw, &url)
    }
}

/// `{title, content}` → 给模型看的一段文本。`title` 按官方约定缺失折空串，
/// `content` 缺键属于响应格式变化（Err），空串则是合法但无内容（Ok + 说明）。
fn format_page(raw: &Value, url: &str) -> anyhow::Result<String> {
    let Some(content) = raw.get("content").and_then(Value::as_str) else {
        return Err(anyhow::anyhow!("网页抓取响应缺少 content，格式可能已变化"));
    };
    if content.trim().is_empty() {
        return Ok(format!(
            "网页抓取成功但正文为空: {url} —— 该页可能没有可提取的文本,换一个 URL 或用 web_search 找其他来源。"
        ));
    }
    let title = raw
        .get("title")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("无标题");
    let (body, cut) = truncate_utf8(content, MAX_OUTPUT_BYTES);
    let mut out = format!("网页: {title}\nURL: {url}\n\n{body}");
    if cut {
        out.push_str(&format!(
            "\n\n[正文超过 {}KB,已截断;Fetch 不支持分页,可找更具体的页面或用 web_search 换来源]",
            MAX_OUTPUT_BYTES / 1024
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: Value) -> String {
        v.to_string()
    }

    #[test]
    fn parse_accepts_normal_url() {
        let url = "https://example.com/a?b=1#c";
        assert_eq!(
            parse(&args(serde_json::json!({ "url": url }))).unwrap(),
            url
        );
    }

    #[test]
    fn parse_rejects_invalid_contract() {
        for url in [
            "",
            "   ",
            "not a url",
            "ftp://example.com/a",
            "file:///etc/passwd",
            "https://u:p@example.com/a",
            // `http:///x` 会被 WHATWG 解析器规范化成有 host 的 `http://x/`,
            // 真正无 host 的是空 authority 这一种
            "http://",
        ] {
            assert!(
                parse(&args(serde_json::json!({ "url": url }))).is_err(),
                "应拒绝: {url}"
            );
        }
        assert!(parse(&args(serde_json::json!({}))).is_err());
        assert!(parse(&args(serde_json::json!({ "url": "a".repeat(8193) }))).is_err());
    }

    #[tokio::test]
    async fn assess_allows_valid_and_denies_invalid() {
        let tool = WebFetchTool::new();
        assert_eq!(
            tool.assess(&args(serde_json::json!({ "url": "https://example.com" })))
                .await,
            Action::Allow
        );
        assert!(matches!(
            tool.assess(&args(serde_json::json!({ "url": "ftp://example.com" })))
                .await,
            Action::Deny(_)
        ));
    }

    #[test]
    fn formats_title_url_and_body() {
        let raw = serde_json::json!({ "title": "Example", "content": "# 正文\n内容" });
        let out = format_page(&raw, "https://example.com").unwrap();
        assert!(
            out.starts_with("网页: Example\nURL: https://example.com\n\n"),
            "{out}"
        );
        assert!(out.contains("# 正文"), "{out}");
        assert!(!out.contains("已截断"), "{out}");
    }

    #[test]
    fn missing_title_falls_back() {
        let raw = serde_json::json!({ "content": "正文" });
        let out = format_page(&raw, "https://example.com").unwrap();
        assert!(out.starts_with("网页: 无标题"), "{out}");
    }

    #[test]
    fn empty_content_is_stated_not_silently_successful() {
        let raw = serde_json::json!({ "title": "Example", "content": "  " });
        let out = format_page(&raw, "https://example.com").unwrap();
        assert!(out.contains("正文为空"), "{out}");
    }

    #[test]
    fn missing_content_is_an_error() {
        assert!(format_page(&serde_json::json!({ "title": "Example" }), "u").is_err());
    }

    /// 中文正文落在多字节字符中间时不能 panic，且不给假的续读提示
    #[test]
    fn long_utf8_body_is_truncated_on_boundary() {
        let content = "中".repeat(20_000);
        let raw = serde_json::json!({ "title": "长文", "content": content });
        let out = format_page(&raw, "https://example.com").unwrap();
        assert!(out.contains("已截断"), "{out}");
        assert!(!out.contains("offset"), "{out}");
        assert!(
            out.len() < MAX_OUTPUT_BYTES + 512,
            "输出过长: {}",
            out.len()
        );
    }
}
