use async_openai::types::chat::{ChatCompletionTool, ChatCompletionTools, FunctionObject};
use async_trait::async_trait;
use serde::Deserialize;

use super::Tool;

/// 创建或覆盖写文件(有副作用,危险操作)
/// 已存在文件需显式 overwrite=true,强制模型区分新建 vs 覆盖,防误丢内容
pub struct WriteTool;

#[derive(Deserialize)]
struct Args {
    path: String,
    content: String,
    #[serde(default)]
    overwrite: bool,
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn definition(&self) -> ChatCompletionTools {
        ChatCompletionTools::Function(ChatCompletionTool {
            function: FunctionObject {
                name: "write".to_string(),
                description: Some(
                    "创建新文件或完全覆盖写文件。有副作用,高风险。\n\
                    适用:新建文件;完全重写文件内容。\n\
                    不适用:改文件局部用 edit(更安全,不丢其他内容);追加内容用 edit(old_string=末尾文本)。\n\
                    已存在文件覆盖会丢原内容,需 overwrite=true 确认。新建文件不需 overwrite。"
                        .to_string(),
                ),
                parameters: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "文件路径(相对项目根或绝对路径)"
                        },
                        "content": {
                            "type": "string",
                            "description": "要写入的完整内容"
                        },
                        "overwrite": {
                            "type": "boolean",
                            "description": "已存在文件是否覆盖,默认 false。false 时若文件已存在则报错"
                        }
                    },
                    "required": ["path", "content"]
                })),
                strict: None,
            },
        })
    }

    async fn execute(&self, args: &str) -> anyhow::Result<String> {
        let _args: Args = serde_json::from_str(args)?;
        todo!("实现:检查文件是否存在;存在且 overwrite=false 报错;否则写 content;返回成功信息")
    }
}
