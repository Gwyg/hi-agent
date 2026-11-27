//! 请求模型:一次调用想干什么,不参与序列化。
//!
//! # 字段全部平铺
//!
//! 可调参数直接挂在 `ChatRequest` 上,不再包一层 `RequestOptions`:
//!
//! - 对外只多认识「请求」这一个概念,不必先造另一个对象再塞进来;
//! - 形状贴近真实请求报文(model / messages / tools / stream / ...),
//!   读代码时能直接和线上 JSON 对照;
//! - 加一个配置只改一处:加字段、`new()` 里补默认值、加一个 `with_xxx`。
//!
//! 字段描述的是**意图**,不是线上键名。线上发不发、键叫什么由 `protocol/openai/req.rs`
//! 决定:`stream_options` 是协议层按 `stream && include_usage` 推出的包裹键;
//! `thinking` / `effort` 这类 DeepSeek 开关按已定口径固定,不抬成通用字段。
//!
//! 本层**不 derive Serialize** —— 线上 JSON 由 `protocol/` 拼。这是「中立层描述语义」
//! 和「协议层描述形状」的分界。**它不是兼容层**:项目虽已锁定 DeepSeek,agent 循环、
//! 上下文压缩、工具回灌仍需借这层不认识 JSON 字段名,所以保留。

use super::message::Msg;
use super::tool::ToolDef;

/// 思考开关。三态是因为「不表态」和「明确关掉」是两件事。
///
/// **现状(2026-11-10):三态一律不发**,DeepSeek 默认开启思考。这个字段是通用化阶段的
/// 残留,专用化清理时连同 `with_thinking` 一起删;真要关思考由协议层直接发
/// `thinking:{"type":"disabled"}`,不经本层。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ThinkingMode {
    #[default]
    Auto,
    Off,
    On,
}

/// 思考力度。只收 DeepSeek 的规范档位 `low` / `high`(默认) / `max`。
/// 官方 `reasoning_effort` 完整值是 `none` / `low` / `high` / `max`,另有兼容别名
/// `minimal`→low、`medium` / `xhigh`→high、`ultra`→max(会被静默映射,不是 400)。
/// 这里刻意不发别名(给旧软件兼容用的),也不发 `none`(关思考,当前不支持)。
/// `as_str()` 是发到线上的字面量,原样输出。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    Low,
    High,
    Max,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::High => "high",
            Self::Max => "max",
        }
    }
}

/// 一次聊天请求。字段都是 `pub`,可以直接赋值;下面的 `with_*` 只是链式便利。
#[derive(Debug, Clone)]
pub struct ChatRequest {
    /// 本次调用哪个模型。
    pub model: String,
    /// 到目前为止的对话,按时间顺序。
    pub messages: Vec<Msg>,
    /// 本次可用的工具声明。协议层拿去拼成线上形状,本层不解释。
    pub tools: Vec<ToolDef>,
    /// 最大输出 token。`None` = **不显式指定**,用端点默认(DeepSeek:关思考 8K /
    /// 开思考 64K / `effort=max` 128K,上限 384K)。本层不猜默认值。
    pub max_tokens: Option<u32>,
    /// 是否流式。默认 `false`;正常对话传 `true`。
    /// 带着它而不是让传输层自己补:请求形状应由请求模型完整描述。
    pub stream: bool,
    /// 要不要响应带本次的 token 用量统计,由调用方决定,默认 `true`。
    /// 协议层按 `stream && include_usage` 推出 `stream_options` 包裹键
    /// (非流式响应本来就有 usage,不发这个键)。
    pub include_usage: bool,
    /// 采样温度。`None` = 不发,用端点默认。
    /// **DeepSeek 开思考时该参数无效**(端点接受但不生效)。
    pub temperature: Option<f32>,
    /// 思考开关,默认 `Auto`(不表态)。当前协议层一律不发,见类型注释。
    pub thinking: ThinkingMode,
    /// 思考力度。`None` = 不发(用端点默认 `high`),取值见 `Effort`。
    pub effort: Option<Effort>,
}

impl ChatRequest {
    /// 默认值全部在这里兜底:默认要 usage 统计、思考不表态、其余参数不显式指定。
    /// `include_usage` 默认 true 不能靠 `derive(Default)`(bool 默认 false),所以手写一份。
    pub fn new(model: impl Into<String>, messages: Vec<Msg>) -> Self {
        Self {
            model: model.into(),
            messages,
            tools: Vec::new(),
            max_tokens: None, // 不显式指定,用 DeepSeek 端点默认(见字段注释)
            stream: false,
            include_usage: true,
            temperature: None,
            thinking: ThinkingMode::Auto,
            effort: None,
        }
    }

    pub fn with_tools(mut self, tools: Vec<ToolDef>) -> Self {
        self.tools = tools;
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// 是否流式。默认 `false`;正常对话传 `true`,压缩摘要这类一次性请求保持默认。
    /// 传完整的值而不是 `non_stream()` 那种单向动作开关:与其他 `with_*` 同模式,
    /// 拿到一个已关掉的请求也能再开回来。
    pub fn with_stream(mut self, stream: bool) -> Self {
        self.stream = stream;
        self
    }

    /// 要不要响应带 token 用量统计。默认 `true`;不想收统计的调用方传 `false`。
    pub fn with_include_usage(mut self, include_usage: bool) -> Self {
        self.include_usage = include_usage;
        self
    }

    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
    }

    pub fn with_thinking(mut self, thinking: ThinkingMode) -> Self {
        self.thinking = thinking;
        self
    }

    pub fn with_effort(mut self, effort: Effort) -> Self {
        self.effort = Some(effort);
        self
    }
}

/// 这里几乎全是类型定义:字段、枚举、逐字段赋值的 `with_*` 都不必测
/// (那是编译器和 derive 的活)。下面两条只钉住**手写逻辑** —— 写错了不会有编译错误。
#[cfg(test)]
mod tests {
    use super::*;

    /// `new()` 手写了默认值,其中 `include_usage: true` 反直觉(bool 天然默认 false)。
    #[test]
    fn new_uses_neutral_defaults() {
        let req = ChatRequest::new("m", vec![]);
        assert!(!req.stream);
        assert!(req.include_usage);
        assert_eq!(req.thinking, ThinkingMode::Auto);
        assert!(req.max_tokens.is_none());
        assert!(req.temperature.is_none());
        assert!(req.effort.is_none());
        assert!(req.tools.is_empty());
    }

    /// `as_str()` 是手写映射,档位名要原样发到线上,拼错不会被编译器发现。
    #[test]
    fn effort_names_go_out_verbatim() {
        assert_eq!(Effort::Low.as_str(), "low");
        assert_eq!(Effort::High.as_str(), "high");
        assert_eq!(Effort::Max.as_str(), "max");
    }
}
