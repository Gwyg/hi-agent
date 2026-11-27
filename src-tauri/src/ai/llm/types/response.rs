//! 一次调用的产出,以及响应侧的共用词汇。
//!
//! 这一层只描述「这次调用给了什么」,**不存线上报文**:`id` / `created` / `model` /
//! `choices` / `logprobs` / `stop_sequence` 这些传输元数据归 transport,不进通用层。
//!
//! 消息本体就是 `Msg` —— 与历史、请求共用同一份。`ChatResponse` 不重复它的任何
//! 字段,也不转发访问器;另外两项是这次调用自己的信息:怎么结束的、花了多少。
//! 分流策略(截断要不要重试、过滤要不要提示)归上层,这里只交出原始事实。
//!
//! `refusal` 不单列:按已定约定,由协议层折进 `message` 的正文并自行打 warn。
//! 思考与签名落在 `Msg::Assistant.thinking`,由 `Msg::assistant_with` 组装。
//!
//! `FinishReason` / `Usage` 描述「一次调用怎么结束、花了多少」,是响应侧的共用
//! 词汇,不是流式专有 —— `stream.rs` 的 `Delta` / `StreamAccumulator` 反向引用
//! 这两个类型。本文件不定义分片 / 累积逻辑,那些归 `stream.rs`。
//!
//! 与 `types/` 整层一致:不 derive `Serialize` / `Deserialize` —— 线上报文由
//! 协议层拼,持久化方案未定。

use super::message::Msg;

/// 一次聊天调用的结果。字段全 `pub`:没有不变式要护,协议层解析完一次填好。
#[derive(Debug, Clone, PartialEq)]
pub struct ChatResponse {
    /// 模型这一轮说的话。可直接放进上下文,无需再转化一次。
    pub message: Msg,
    /// 这次调用怎么结束的。`Length` / `ContentFilter` 无法从消息反推,必须显式带出;
    /// 协议缺这个键 / 整条流都没给时由解码层与累积器归一成 `Unknown`,
    /// 不在这里用 `Option` 把负担推给调用方。
    pub finish_reason: FinishReason,
    /// token 消耗。端点整份没给就是 `None`,不编造全 0 的用量;
    /// 给了一部分时,未单列的细分字段为 `0`。总量 = `input + output`,不另存。
    pub usage: Option<Usage>,
}

/// 结束原因。
///
/// `Other` 是**给了但值不规范**的兜底:各厂商会返回规范外的值
/// (如 `insufficient_system_resource`),用封闭枚举会让一整条流解析失败 ——
/// 一个状态值不该具备中断对话的能力。
///
/// `Unknown` 是**压根没给**:非流式响应缺 `finish_reason` 键,或流式整条流
/// 没收到 `Delta::Finish`(被截断 / 兼容端点省略)。它与 `Other` 语义不同,
/// 所以不塞进 `Other("unknown")` 这种字符串魔法值。两者都只是交出事实,
/// 要不要报错终止由上层决定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    ContentFilter,
    /// 协议没提供结束原因。
    Unknown,
    Other(String),
}

/// 一次调用的 token 用量。总量 = `input_tokens + output_tokens`,不另存字段。
///
/// `reasoning_tokens` / `cached_tokens` 分别是输出 / 输入的**子集**,不是加项 ——
/// 协议里的 `*_details` 也只是对主数的成分标注,不能重复累加。
///
/// 两侧统一按 OpenAI 口径归一(与 `types/` 整体偏 OpenAI 一致):
/// - `input_tokens` 是实际消耗的输入总量,**已含缓存读/写**。OpenAI 直接取
///   `prompt_tokens`;Anthropic 要把 `input_tokens` + `cache_read_input_tokens` +
///   `cache_creation_input_tokens` 相加后填入,否则总量会少算缓存部分。
/// - `output_tokens` 是模型生成的总量,**已含思考**。OpenAI 取
///   `completion_tokens`,Anthropic 取 `output_tokens`。
/// - `cached_tokens` 取 OpenAI 的 `cached_tokens` / Anthropic 的
///   `cache_read_input_tokens`;Anthropic 的缓存写入量并入 `input_tokens`,不单列。
/// - `reasoning_tokens` 取 OpenAI 的 `completion_tokens_details.reasoning_tokens`;
///   Anthropic 不单列,恒 `0`。
///
/// `0` 一律表示「没有 / 该端点没单列」,不区分「用量为零」和「未提供」。流式里
/// Anthropic 把 input 放在 `message_start`、output 放在 `message_delta`,两帧各带
/// 一半,累积器按字段覆盖(`if v > 0 { set }`),不能整份替换。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// `output_tokens` 的其中一部分:模型内部推理消耗的 token。
    pub reasoning_tokens: u32,
    /// `input_tokens` 的其中一部分:命中提示词缓存读取的 token。
    pub cached_tokens: u32,
}

impl Usage {
    /// 总量 = `input_tokens + output_tokens`。不存字段,也不让调用方自己相加:
    /// 这个口径属于 `Usage`,将来归一方式变了只改这一处。与其他「预留访问器」
    /// 不同,它是**派生值计算**,不是把内部字段转发给外部。
    pub fn total_tokens(&self) -> u32 {
        self.input_tokens + self.output_tokens
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手写的派生口径:细分字段是主数的子集,总量不能把它们重复累加。
    #[test]
    fn total_tokens_only_adds_the_two_main_counts() {
        let usage = Usage {
            input_tokens: 1212,
            output_tokens: 34,
            reasoning_tokens: 19,
            cached_tokens: 1000,
        };
        assert_eq!(usage.total_tokens(), 1246);
    }
}
