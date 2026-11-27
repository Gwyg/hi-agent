//! 工具声明:应用层说「我们有哪些工具」。
//!
//! `ToolDef` 本身就是**按类型分派的枚举**:变体名承担类型,每种类型专属的
//! 字段直接写在该变体里。这样类型不会缺失、不会拼错,也不可能与载荷错配 ——
//! Function 必有 `parameters`、Custom 必有 `format`,非法组合在类型里不存在。
//! 加第三种工具时,编译器会强制两侧协议层都补 `match` 分支。
//!
//! **这里不长出 `function` / `custom` 的包裹形状** —— 那是 OpenAI 报文的 wire
//! 细节,由协议转化层拼;Anthropic 侧转成 `{name, description, input_schema}`,
//! 并跳过它不认的 Custom。协议层当前留空,解冻时按这个契约实现。
//!
//! 载荷保持 `Value`:JSON Schema / grammar 都是外部规范,各厂商支持的子集不同,
//! `types` 不读、不校验、不解释,原样转交。用 `String` 拼进 `Value` 树会变成
//! 带引号的标量,不是 JSON 对象。
//!
//! 与 `request.rs` 一致,**本层不 derive `Serialize` / `Deserialize`**:线上报文
//! 由协议层拼,对话历史怎么落盘尚未设计,不预留没人调用的磁盘形状。

use serde_json::Value;

/// 一个工具的声明。变体名即工具类型(Function / Custom)。
///
/// enum 变体字段按 Rust 规则自动公开,没法单独设私有 —— 但公开也没关系:构造出的
/// 只能是合法形状,`function()` / `custom()` 只是少写两个字段的便利构造器。
#[derive(Debug, Clone, PartialEq)]
pub enum ToolDef {
    /// 按 JSON Schema 填参数,由我们执行。
    Function {
        /// 工具名。确认弹窗、工具日志也用它当标题。
        name: String,
        /// 给模型看的说明。
        description: String,
        /// 外部规范(JSON Schema)的文档,原样转交协议层。
        parameters: Value,
    },
    /// 模型回一段文本,不按 JSON Schema 填参数。
    /// OpenAI 官方特有,多数兼容端点与 Anthropic 不认 —— 协议层决定跳过还是降级。
    Custom {
        name: String,
        description: String,
        /// `None` = 自由文本(端点默认,协议层此时不写 `format` 键);
        /// 官方另有 grammar 格式,原样透传。
        format: Option<Value>,
    },
}

impl ToolDef {
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
    ) -> Self {
        Self::Function {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }

    pub fn custom(
        name: impl Into<String>,
        description: impl Into<String>,
        format: Option<Value>,
    ) -> Self {
        Self::Custom {
            name: name.into(),
            description: description.into(),
            format,
        }
    }

    /// 公共字段不必让上层自己 `match`。or-pattern 让两个变体共用一份实现,
    /// 加新变体时编译器会强制这里补分支。
    pub fn name(&self) -> &str {
        match self {
            Self::Function { name, .. } | Self::Custom { name, .. } => name,
        }
    }

    pub fn description(&self) -> &str {
        match self {
            Self::Function { description, .. } | Self::Custom { description, .. } => description,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手写的 or-pattern,写错(例如 Custom 分支返回 description)编译器看不出来。
    #[test]
    fn common_fields_readable_from_both_variants() {
        let f = ToolDef::function("bash", "跑命令", serde_json::json!({}));
        let c = ToolDef::custom("writer", "写点东西", None);
        assert_eq!(f.name(), "bash");
        assert_eq!(f.description(), "跑命令");
        assert_eq!(c.name(), "writer");
        assert_eq!(c.description(), "写点东西");
    }
}
