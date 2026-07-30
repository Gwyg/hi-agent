use async_openai::types::chat::{ChatCompletionTool, ChatCompletionTools, FunctionObject};
use async_trait::async_trait;
use serde::Deserialize;

use super::Tool;

/// 示例工具:把传入文本原样回显,用于验证 agent 循环链路
pub struct EchoTool;

/// 工具入参契约(工具私有,不外泄)
#[derive(Deserialize)]
struct Args {
    text: String,
}

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn definition(&self) -> ChatCompletionTools {
        ChatCompletionTools::Function(ChatCompletionTool {
            function: FunctionObject {
                name: "echo".to_string(),
                description: Some("把传入文本原样回显".to_string()),
                parameters: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "text": {
                            "type": "string",
                            "description": "要回显的文本"
                        }
                    },
                    "required": ["text"]
                })),
                strict: None,
            },
        })
    }

    async fn execute(&self, args: &str) -> anyhow::Result<String> {
        let args: Args = serde_json::from_str(args)?;
        Ok(args.text)
    }
}
