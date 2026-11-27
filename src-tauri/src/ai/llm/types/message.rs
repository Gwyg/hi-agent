//! 一条消息 = 一种角色 + 该角色自己的结构体。
//!
//! # 为什么按角色拆成枚举
//!
//! OpenAI 报文里 assistant 的 `content` 与 `tool_calls` 是**两个平级字段**,
//! tool 消息自带 `tool_call_id`,system / user 则都没有这些;思考只属于 assistant。
//! 用「role + 万能块数组」会把这些差异抹平,只能靠约定去记「哪种块能配哪个角色」。
//! 四个变体各包一个具名结构体后,`user + thinking`、`tool + tool_calls` 这类组合
//! 直接写不出来;外部也能单独持有、传递一个 `AssistantMsg`。
//!
//! # 形状偏向 OpenAI
//!
//! OpenAI 是「拆开的」(四类消息、字段各自独立),Anthropic 是「揉在一起的」
//! (只有 user/assistant,思考、工具调用、工具结果全进 content 数组)。
//! 从拆开的形状折成揉在一起的,每一步只是收集与拼接,每块该放哪都是确定的;
//! 反过来要把一条混着多种 block 的 user 拆回不同角色的消息,块的归属与顺序
//! 没有被记录,只能靠约定猜。所以通用层按 OpenAI 定,折叠归协议层一次做完。
//!
//! # `tool_calls` 为什么不进 `content`
//!
//! 只有 Anthropic 把 `tool_use` / `tool_result` 放进 `content` 数组;OpenAI 是顶层
//! `tool_calls[]` 和独立 `role:"tool"` 消息。通用层按**字段语义**存,不按某一家
//! 的摆放存:Anthropic 编码时把 `thinking` → `content` → `tool_calls` 拼成一个
//! 数组、把 tool 消息折进 user 并合并相邻同角色 —— 折叠是协议层一次性的事。
//!
//! # `content` 只有 user 是 `Vec`
//!
//! 两家协议里 system / assistant / tool 的 `content` 虽然都能写成数组,但数组里
//! **只装 text 块** —— 本质还是一段文字被切成几片,所以这三种存 `String`。
//! `UserMsg` 保留 `Vec<Content>`:图片 / 文件等内容类型目前都还没做,但留着这个
//! 位置,将来接多模态时只要在 `Content` 加一个变体、两侧协议各补一个分支,
//! 消息结构一行都不用改。空正文就是空串,线上同样写 `""`(键不省略),
//! 与中立层形状一致,不需要在通用层加 `Option` 去还原 `null`。
//!
//! 三种标量角色的流式累积随之变简单:正文分片直接 `push_str`,没有形态判断。
//!
//! 两个扩展点暂不做:Anthropic 的 system 多段缓存断点、`tool_result` 里带附件。
//! 真要支持时这三种才需要重新变成 `Vec<Content>`。
//!
//! # 不做持久化
//!
//! 对话历史怎么落盘还没设计(独立文件 / sqlite / 别的),线上报文由协议层拼。
//! 所以这里**不实现 `Serialize` / `Deserialize`**,也不预留任何磁盘形状 ——
//! 现在写只会变成没人调用的死代码。等持久化方案定了,再按真实需求加。
//!
//! # 构造入口
//!
//! 这层是中间层:协议解析拿到的是一串**原始值**(正文、工具调用、思考文本、
//! 签名),不该由它来记「`Thinking` 怎么组装、`None` 怎么填、默认值有哪些」。
//! 所以消息一律经 `Msg` 上的构造器创建 —— `system` / `user` / `assistant` /
//! `tool` 是常见形态,`assistant_with` 一次收下解析层交出的四个原始值。
//! 字段仍是 `pub`(没有不变式要护),但「交数据、由方法组装」是规范:将来给
//! `AssistantMsg` 加字段只改方法内部,所有调用方一行不动。
//!
//! # 思考
//!
//! 一轮一段、绑定整条 assistant 消息,`None` 就是没有。OpenAI 兼容端点写成消息
//! 上的平级 `reasoning_content`,Anthropic 拼成 `content` 数组第一块且要带签名。
//!
//! # `arguments` 为什么是 `String`
//!
//! 全链路(流式累积 / agent / 工具鉴权与执行 / 前端事件 / 压缩)都把参数当一段
//! **原文字符串**;线上 `arguments` 本身就是一个 JSON 字符串标量,不是对象。
//! 存 `Value` 再 `to_string()` 不保真(未开 `preserve_order` 时 key 按字典序重排、
//! 转义与空白归一、重复 key 被吞),而历史消息要原样回传服务端;流式分片更是
//! 在拼完之前根本不是合法 JSON。只有 Anthropic 的 `input` 要对象,那一次
//! `from_str` 归协议层,失败必须报错,不能静默发空对象。
//! 与 `ToolDef.parameters: Value` 区分:那份是**我们写的说明书**,这段是
//! **模型回的原文**,同名不同物。

/// 一条消息。四种角色各包一个自己的结构体,结构体字段全 `pub`:没有不变式要护。
#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    System(SystemMsg),
    User(UserMsg),
    Assistant(AssistantMsg),
    Tool(ToolMsg),
}

/// 系统提示。OpenAI 是 `role:"system"` 消息,Anthropic 提到顶层 `system` 键。
/// `developer` 角色按约定也归这里。一段文本,不是块数组。
#[derive(Debug, Clone, PartialEq)]
pub struct SystemMsg {
    pub content: String,
}

/// 用户消息。只有它持 `Vec<Content>` —— 将来接图片 / 文件时加一个 `Content`
/// 变体即可,不必改消息结构。Anthropic 会把工具结果折进 user,但那是协议层的
/// 摆放,通用层不预先把 `tool_result` 当成用户内容。
#[derive(Debug, Clone, PartialEq)]
pub struct UserMsg {
    pub content: Vec<Content>,
}

/// 模型回复。思考只属于这一角色,`None` = 这一轮没有思考。
#[derive(Debug, Clone, PartialEq)]
pub struct AssistantMsg {
    pub content: String,
    /// 与 `content` 平级:OpenAI 是顶层 `tool_calls[]`,Anthropic 编码时转成
    /// content 数组里的 `tool_use` 块。
    pub tool_calls: Vec<ToolCall>,
    pub thinking: Option<Thinking>,
}

/// 工具执行结果。OpenAI 是独立 `role:"tool"` 消息,Anthropic 折进 user 的
/// `tool_result` 块;两边都靠 `call_id` 配对。
///
/// 暂不带 `is_error`:外部只有给前端看的 `ToolStatus`,失败 / 拒绝 / 取消一律写成
/// 结果文本,没有结构化状态可传;Anthropic 侧不写这个键默认 false、不会 400。
/// 等外部真的把失败状态结构化了再加,届时 OpenAI 侧明确丢弃。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolMsg {
    pub call_id: String,
    pub content: String,
}

impl Msg {
    pub fn system(text: impl Into<String>) -> Self {
        Self::System(SystemMsg {
            content: text.into(),
        })
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self::User(UserMsg {
            content: vec![Content::text(text)],
        })
    }

    pub fn user_content(content: Vec<Content>) -> Self {
        Self::User(UserMsg { content })
    }

    /// 纯文本回复。与 `system` / `user` / `tool` 一致:一个值换一条消息。
    pub fn assistant(text: impl Into<String>) -> Self {
        Self::assistant_with(text, Vec::new(), None, None)
    }

    /// 解析层 / 流式累积器收尾用:把这一轮拿到的原始值一次交出来。
    ///
    /// 四个参数都是解析结果,调用方不碰 `Thinking`、不填任何默认值;组装和
    /// `Option` 的初始化都在方法内部。没有思考文本时 `signature` 明确丢弃
    /// (OpenAI 兼容端点本来也恒为 `None`),不会凭空造出一段空思考。
    ///
    /// Rust 没有重载,纯文本单独留 `assistant` 这个名字,与另外三种消息对齐。
    pub fn assistant_with(
        text: impl Into<String>,
        tool_calls: Vec<ToolCall>,
        thinking: Option<String>,
        signature: Option<String>,
    ) -> Self {
        Self::Assistant(AssistantMsg {
            content: text.into(),
            tool_calls,
            thinking: thinking.map(|text| Thinking { text, signature }),
        })
    }

    pub fn tool(call_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self::Tool(ToolMsg {
            call_id: call_id.into(),
            content: text.into(),
        })
    }

    /// 决定历史思考要不要回传时要看它。
    pub fn has_tool_calls(&self) -> bool {
        matches!(self, Self::Assistant(m) if !m.tool_calls.is_empty())
    }
}

/// `UserMsg::content` 里的一块。目前只有文本 —— 项目还没有图片 / 文件输入入口,
/// 图片相关的变体与构造器已删掉,不预留没人用的词汇。
///
/// 保留这个枚举和 `Vec` 是为了扩展性:将来接多模态时加一个变体,两侧协议层
/// 各补一个 `match` 分支,`Msg` / `ChatRequest` 都不用改。工具调用 / 工具结果
/// **不在这里**:它们各自是 `AssistantMsg::tool_calls` 与 `ToolMsg`。
#[derive(Debug, Clone, PartialEq)]
pub enum Content {
    /// 正文。
    Text { text: String },
}

impl Content {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}

/// 模型要调工具。独立类型,不是 `Content` 变体:OpenAI 里它与 `content` 平级。
///
/// `arguments` 存**协议原文的 JSON 字符串**:理由见文件头。OpenAI 原样发字符串,
/// Anthropic 在协议层 parse 成 input 对象;通用层不 parse,所以流式拼出来的
/// 不完整 / 非法 JSON 也能留住,由工具层自己判 Deny。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

impl ToolCall {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }
}

/// 一轮的思考。
///
/// `signature` 是 Anthropic 的 `signature_delta` 给的凭据:回传思考时必须一起带上,
/// 否则服务端拒收。OpenAI 兼容端点没有这个概念,编码时丢掉即可。
#[derive(Debug, Clone, PartialEq)]
pub struct Thinking {
    pub text: String,
    pub signature: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 只测手写的判定逻辑:形状本身由类型和构造器保证,写错编译器会报。
    #[test]
    fn has_tool_calls_only_counts_assistant() {
        assert!(!Msg::user("看下目录").has_tool_calls());
        assert!(!Msg::assistant("结论").has_tool_calls());
        let with_calls = Msg::assistant_with(
            String::new(),
            vec![ToolCall::new("c1", "bash", "{}")],
            None,
            None,
        );
        assert!(with_calls.has_tool_calls());
    }

    /// 只测手写的组装逻辑:四个原始值各落到哪个字段、无思考时签名要丢。
    #[test]
    fn assistant_with_assembles_raw_values() {
        let calls = vec![ToolCall::new("c1", "bash", "{}")];
        let msg = Msg::assistant_with(
            "结论",
            calls.clone(),
            Some("先想".into()),
            Some("sig".into()),
        );
        let Msg::Assistant(m) = &msg else {
            panic!("expected assistant");
        };
        assert_eq!(m.content, "结论"); // 思考单独挂在外层,不并进正文
        assert_eq!(m.tool_calls, calls);
        let thinking = m.thinking.as_ref().expect("thinking");
        assert_eq!(thinking.text, "先想");
        assert_eq!(thinking.signature.as_deref(), Some("sig"));
        assert!(msg.has_tool_calls());

        // 没有思考文本时签名没有落点,明确丢弃。
        let plain = Msg::assistant_with("", vec![], None, Some("sig".into()));
        let Msg::Assistant(m) = plain else {
            panic!("expected assistant");
        };
        assert!(m.thinking.is_none());
    }
}
