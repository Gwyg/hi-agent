use async_openai::types::chat::{ChatCompletionTool, ChatCompletionTools, FunctionObject};
use async_trait::async_trait;
use serde::Deserialize;

use super::Tool;

/// 搜索文件名或文件内容(无副作用,高频只读)
/// 支持按文件名 glob 或内容正则搜索,可限定目录范围
pub struct SearchFilesTool;

#[derive(Deserialize)]
struct Args {
    /// 搜索范围目录,默认项目根
    #[serde(default)]
    path: Option<String>,
    /// 按文件名搜索(glob 模式,如 "*.rs")
    #[serde(default)]
    pattern: Option<String>,
    /// 按内容搜索(正则)
    #[serde(default)]
    content: Option<String>,
}

#[async_trait]
impl Tool for SearchFilesTool {
    fn name(&self) -> &str {
        "search_files"
    }

    fn definition(&self) -> ChatCompletionTools {
        ChatCompletionTools::Function(ChatCompletionTool {
            function: FunctionObject {
                name: "search_files".to_string(),
                description: Some(
                    "搜索本地文件:按文件名 glob 或内容正则。无副作用。\n\
                    适用:找某文件在哪(path 不确定);找哪些文件含某文本;列目录内容(pattern=*)。\n\
                    不适用:读已知路径文件内容用 read;看命令输出用 bash;联网搜索不支持(本地专用)。\n\
                    pattern 搜文件名(如 *.rs、**/*.toml);content 搜文件内正则。两者可同时用。path 限定范围,默认项目根。"
                        .to_string(),
                ),
                parameters: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "搜索范围目录,默认项目根"
                        },
                        "pattern": {
                            "type": "string",
                            "description": "文件名 glob 模式,如 *.rs 或 **/*.toml"
                        },
                        "content": {
                            "type": "string",
                            "description": "文件内容正则表达式"
                        }
                    },
                    "required": []
                })),
                strict: None,
            },
        })
    }

    async fn execute(&self, args: &str) -> anyhow::Result<String> {
        let _args: Args = serde_json::from_str(args)?;
        todo!("实现:遍历 path 下文件;pattern 过滤文件名;content 正则匹配文件内容;返回匹配文件路径+行号+匹配行")
    }
}
