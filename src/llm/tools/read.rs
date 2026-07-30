use async_openai::types::chat::{ChatCompletionTool, ChatCompletionTools, FunctionObject};
use async_trait::async_trait;
use serde::Deserialize;

use super::Tool;

/// 读取文件内容(无副作用,高频只读操作)
pub struct ReadTool;

#[derive(Deserialize)]
struct Args {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn definition(&self) -> ChatCompletionTools {
        ChatCompletionTools::Function(ChatCompletionTool {
            function: FunctionObject {
                name: "read".to_string(),
                description: Some(
                    "读取文件内容(带行号)。无副作用。\n\
                    适用:看代码/配置/日志/文档内容;确认文件当前状态。\n\
                    不适用:看目录有哪些文件用 search_files(pattern);搜文件内某文本用 search_files(content);看命令输出用 bash。\n\
                    大文件用 offset/limit 分页,不要一次读全。"
                        .to_string(),
                ),
                parameters: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "文件路径(相对项目根或绝对路径)"
                        },
                        "offset": {
                            "type": "integer",
                            "description": "起始行号(从1开始),默认1"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "读取行数,不填则读到文件末尾"
                        }
                    },
                    "required": ["path"]
                })),
                strict: None,
            },
        })
    }

    async fn execute(&self, args: &str) -> anyhow::Result<String> {
        let _args: Args = serde_json::from_str(args)?;
        todo!("实现:打开文件,从 offset 行起读 limit 行,返回带行号的内容;二进制文件返回提示")
    }
}
