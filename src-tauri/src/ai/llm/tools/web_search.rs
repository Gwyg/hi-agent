//! 联网搜索工具：把 DeepSeek `POST /v1/tools/search` 包成一个普通 tool。
//!
//! 职责边界：本文件只认「搜什么、要几条」以及「把结果整理成给模型看的一段文本」。
//! 端点路径、请求字段名、鉴权、上游错误原文都在 `ai::llm::deepseek`，这里不出现
//! `need_content` / `Authorization` 之类的 wire 细节，也不做敏感词过滤（与 bash 不做
//! 路径校验同一口径），只校验官方的请求契约。
//!
//! 结果以普通 tool 结果回灌，**永久进历史、每轮重发**，所以必须限条数、限长度。
//! 失败一律 `Err`：agent 循环已统一折成「工具执行出错: …」的 tool 结果，不中断整轮，
//! 也不需要为联网工具单独写回灌逻辑。

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use super::support::text::truncate_utf8;
use super::{Action, Tool};
use crate::ai::llm::types::ToolDef;
use crate::ai::llm::web_search as search_web;

/// 官方契约：query 为 1~128 个 Unicode 字符、不能全空白。按字符数，不是字节数。
const MAX_QUERY_CHARS: usize = 128;
/// 默认 / 上限条数。官方默认 10 条长摘要约 8k 字符，结果永久进历史，故收紧到 5 / 10。
const DEFAULT_COUNT: u32 = 5;
const MAX_COUNT: u32 = 10;
/// 单次输出总预算（字节）。
const MAX_OUTPUT_BYTES: usize = 12 * 1024;
/// 单条摘要保留上限，避免第一条就把预算吃光。
const MAX_SUMMARY_BYTES: usize = 1200;

pub struct WebSearchTool;

#[derive(Deserialize)]
struct Args {
    #[serde(default)]
    query: String,
    #[serde(default)]
    count: Option<u32>,
}

struct Call {
    query: String,
    count: u32,
}

impl WebSearchTool {
    pub fn new() -> Self {
        Self
    }
}

/// 参数校验只写一处：assess 与 execute 共用，两处不会出现宽严不一致。
fn parse(args: &str) -> anyhow::Result<Call> {
    let args: Args =
        serde_json::from_str(args).map_err(|e| anyhow::anyhow!("web_search 参数解析失败: {e}"))?;
    let query = args.query.trim().to_string();
    if query.is_empty() {
        return Err(anyhow::anyhow!("query 不能为空或全空白"));
    }
    if query.chars().count() > MAX_QUERY_CHARS {
        return Err(anyhow::anyhow!("query 超过 {MAX_QUERY_CHARS} 个字符"));
    }
    // 0 / 不传都按默认；超出上限就夹住，不让上游 400 白费一轮
    let count = args
        .count
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_COUNT)
        .min(MAX_COUNT);
    Ok(Call { query, count })
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "web_search",
            format!(
                "联网搜索网页(DeepSeek 搜索引擎),返回标题/链接/摘要。只读,不访问本地文件。\n\
                 适用:查最新文档、新闻、版本变化、外部资料;search_files 只搜本地文件,不要混用。\n\
                 不适用:已知 URL 要读正文用 web_fetch;本地文件用 read/search_files。\n\
                 query 支持 site: -site: lang: after: before: 精确短语引号与 - 排除词,直接写进 query 字符串。\n\
                 默认返回 {DEFAULT_COUNT} 条,最多 {MAX_COUNT} 条;结果会截断,需要正文时用 web_fetch(url)。"
            ),
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": format!(
                            "搜索词,1~{MAX_QUERY_CHARS} 个字符,不能全空白;可写 site: lang: after: 等算子"
                        )
                    },
                    "count": {
                        "type": "integer",
                        "description": format!("返回条数,默认 {DEFAULT_COUNT},最多 {MAX_COUNT}")
                    }
                },
                "required": ["query"]
            }),
        )
    }

    /// 合法参数一律 Allow：搜索是只读外部请求，调用即授权，不打断 agent 循环。
    /// 只挡明显非法的参数（与 read 的 assess 同形）。
    async fn assess(&self, args: &str) -> Action {
        match parse(args) {
            Ok(_) => Action::Allow,
            Err(e) => Action::Deny(e.to_string()),
        }
    }

    /// 取消 = 本 future 被 drop；reqwest 没有脱离 future 的工作，连接随之断开，
    /// 无需额外收尾（呼应 `Tool::execute` 契约）。不重试：重试是全局策略，且失败请求不计费、
    /// 重试反而可能重复计费；「哪些 4xx 别重试」由错误文本告诉模型。
    async fn execute(&self, args: &str) -> anyhow::Result<String> {
        let call = parse(args)?;
        let raw = search_web(&call.query, call.count).await.map_err(|e| {
            anyhow::anyhow!(
                "联网搜索失败: {e} —— 可换关键词后重试;408/429/5xx 稍后重试;\
                 402 需检查账户余额;其他 4xx 请调整查询条件,不要原样重试"
            )
        })?;
        Ok(format_results(&raw, &call.query))
    }
}

/// `Value` → 给模型看的一段文本。字段按官方 Schema 宽松取值：缺键 / null / 类型不符
/// 一律跳过或折空串，不为了一份只读结果再维护一层 wire struct（与响应侧同口径）。
fn format_results(raw: &Value, query: &str) -> String {
    let Some(items) = raw.get("results").and_then(Value::as_array) else {
        return "联网搜索响应缺少 results，格式可能已变化；可换关键词重试。".to_string();
    };
    if items.is_empty() {
        // 空结果是正常的（也不计费），别让模型把它当成失败反复重试同一个 query
        return format!("联网搜索「{query}」无结果。可放宽关键词、去掉 site:/时间过滤后重试。");
    }

    let mut out = format!("联网搜索「{query}」,共 {} 条:\n\n", items.len());
    let mut shown = 0usize;
    let mut stopped = false;
    for item in items {
        // 首条无论如何要给：否则预算过小时一条都出不来，只剩一句看不懂的截断提示
        if shown > 0 && out.len() + MAX_SUMMARY_BYTES > MAX_OUTPUT_BYTES {
            stopped = true;
            break;
        }
        out.push_str(&format!(
            "[{}] {}\n",
            shown + 1,
            str_field(item, "title", "无标题")
        ));
        let url = str_field(item, "url", "");
        if !url.is_empty() {
            out.push_str(&format!("URL: {url}\n"));
        }

        // 站点 / 日期 / 权威 / 相关分只帮模型判断来源质量；null 与 -1 表示未知，整项跳过
        let mut meta = Vec::new();
        if let Some(name) = item.get("site_name").and_then(Value::as_str) {
            if !name.is_empty() {
                meta.push(format!("站点: {name}"));
            }
        }
        if let Some(date) = item.get("date").and_then(Value::as_str) {
            // RFC3339 前 10 位是日期；用 get 兜住非 ASCII 串，不在字符中间切片
            meta.push(format!("发布: {}", date.get(..10).unwrap_or(date)));
        }
        if let Some(a) = item.get("authority").and_then(Value::as_i64) {
            if a >= 0 {
                meta.push(format!("权威: {a}/4"));
            }
        }
        if let Some(s) = item.get("rerank_score").and_then(Value::as_f64) {
            meta.push(format!("相关: {s:.2}"));
        }
        if !meta.is_empty() {
            out.push_str(&meta.join(" | "));
            out.push('\n');
        }

        let summary = str_field(item, "summary", "");
        if !summary.is_empty() {
            let (text, cut) = truncate_utf8(summary, MAX_SUMMARY_BYTES);
            let mark = if cut {
                " …(本条摘要已截断)"
            } else {
                ""
            };
            out.push_str(&format!("摘要: {text}{mark}\n"));
        }
        out.push('\n');
        shown += 1;
    }

    if stopped {
        out.push_str(&format!(
            "[已显示 {shown}/{} 条,达到 {}KB 输出上限;可缩小关键词或减小 count]\n",
            items.len(),
            MAX_OUTPUT_BYTES / 1024
        ));
    }
    // need_content 固定 false，content 恒为空串；想读正文走另一个工具，边界不糊
    out.push_str("需要正文时用 web_fetch(url) 抓取对应页面。\n");
    out
}

fn str_field<'a>(item: &'a Value, key: &str, default: &'a str) -> &'a str {
    item.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: Value) -> String {
        v.to_string()
    }

    fn result(summary: &str) -> Value {
        serde_json::json!({
            "title": "标题",
            "url": "https://example.com/a",
            "snippet": "短摘要",
            "summary": summary,
            "content": "",
            "site_name": null,
            "site_icon": null,
            "date": null,
            "authority": -1,
            "rerank_score": 2.5
        })
    }

    #[test]
    fn parse_rejects_blank_and_overlong_query() {
        assert!(parse(&args(serde_json::json!({ "query": "" }))).is_err());
        assert!(parse(&args(serde_json::json!({ "query": "   " }))).is_err());
        assert!(parse(&args(serde_json::json!({}))).is_err());
        assert!(parse(&args(serde_json::json!({ "query": "中".repeat(129) }))).is_err());
        // 边界按字符数：128 个中文字符是合法的
        assert!(parse(&args(serde_json::json!({ "query": "中".repeat(128) }))).is_ok());
    }

    #[test]
    fn parse_clamps_count_and_defaults() {
        let count = |v: Value| parse(&args(v)).unwrap().count;
        assert_eq!(count(serde_json::json!({ "query": "q" })), DEFAULT_COUNT);
        assert_eq!(
            count(serde_json::json!({ "query": "q", "count": 0 })),
            DEFAULT_COUNT
        );
        assert_eq!(
            count(serde_json::json!({ "query": "q", "count": 100 })),
            MAX_COUNT
        );
        assert_eq!(count(serde_json::json!({ "query": "q", "count": 7 })), 7);
        // 类型不符由 serde 拒绝，不会悄悄当默认值
        assert!(parse(&args(serde_json::json!({ "query": "q", "count": "3" }))).is_err());
    }

    #[tokio::test]
    async fn assess_allows_valid_and_denies_invalid() {
        let tool = WebSearchTool::new();
        assert_eq!(
            tool.assess(&args(serde_json::json!({ "query": "DeepSeek" })))
                .await,
            Action::Allow
        );
        assert!(matches!(
            tool.assess(&args(serde_json::json!({ "query": "  " })))
                .await,
            Action::Deny(_)
        ));
    }

    #[test]
    fn empty_results_is_a_normal_actionable_answer() {
        let out = format_results(&serde_json::json!({ "results": [] }), "某冷门词");
        assert!(out.contains("无结果"), "{out}");
        assert!(out.contains("放宽关键词"), "{out}");
    }

    #[test]
    fn missing_results_is_explained() {
        let out = format_results(&serde_json::json!({}), "q");
        assert!(out.contains("缺少 results"), "{out}");
    }

    #[test]
    fn null_metadata_and_unknown_authority_are_skipped() {
        let raw = serde_json::json!({ "results": [result("这是摘要")] });
        let out = format_results(&raw, "q");
        assert!(out.contains("[1] 标题"), "{out}");
        assert!(out.contains("URL: https://example.com/a"), "{out}");
        assert!(out.contains("摘要: 这是摘要"), "{out}");
        assert!(!out.contains("null"), "{out}");
        assert!(!out.contains("权威"), "{out}");
        assert!(!out.contains("发布"), "{out}");
        assert!(out.contains("相关: 2.50"), "{out}");
    }

    #[test]
    fn known_metadata_is_shown_and_date_is_shortened() {
        let mut r = result("摘要");
        r["site_name"] = serde_json::json!("DeepSeek");
        r["date"] = serde_json::json!("2026-11-12T16:00:00+00:00");
        r["authority"] = serde_json::json!(3);
        let out = format_results(&serde_json::json!({ "results": [r] }), "q");
        assert!(out.contains("站点: DeepSeek"), "{out}");
        assert!(out.contains("发布: 2026-11-12"), "{out}");
        assert!(out.contains("权威: 3/4"), "{out}");
    }

    #[test]
    fn output_is_capped_and_truncation_is_stated() {
        let long = "x".repeat(MAX_SUMMARY_BYTES * 2);
        let items: Vec<Value> = (0..10).map(|_| result(&long)).collect();
        let out = format_results(&serde_json::json!({ "results": items }), "q");
        assert!(out.starts_with("联网搜索「q」,共 10 条"), "{out}");
        assert!(out.contains("[1] 标题"), "{out}");
        assert!(out.contains("本条摘要已截断"), "{out}");
        assert!(out.contains("已显示"), "{out}");
        assert!(
            out.len() < MAX_OUTPUT_BYTES + 512,
            "尾注之外不该超出预算: {}",
            out.len()
        );
    }
}
