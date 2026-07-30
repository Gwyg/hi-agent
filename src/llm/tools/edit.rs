use async_openai::types::chat::{ChatCompletionTool, ChatCompletionTools, FunctionObject};
use async_trait::async_trait;
use serde::Deserialize;

use super::Tool;

/// 对文件进行精确文本编辑(有副作用,中危)
/// old_string 在文件中须唯一匹配(除非 replace_all=true),否则报错
pub struct EditTool;

#[derive(Deserialize)]
struct Args {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn definition(&self) -> ChatCompletionTools {
        ChatCompletionTools::Function(ChatCompletionTool {
            function: FunctionObject {
                name: "edit".to_string(),
                description: Some(
                    "对文件做精确局部编辑(替换文本片段)。有副作用,中危。\n\
                    适用:改文件局部代码/配置;重构;修 bug。\n\
                    不适用:新建文件用 write;完全重写文件用 write(overwrite=true);删整段用 edit(old_string=段,new_string=空)。\n\
                    old_string 须精确匹配文件内容(含空格换行)。默认要求唯一匹配,多处改设 replace_all=true。找不到或多处匹配(非 replace_all)报错。"
                        .to_string(),
                ),
                parameters: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "文件路径(相对项目根或绝对路径)"
                        },
                        "old_string": {
                            "type": "string",
                            "description": "要替换的精确文本(须在文件中匹配)"
                        },
                        "new_string": {
                            "type": "string",
                            "description": "替换后的文本"
                        },
                        "replace_all": {
                            "type": "boolean",
                            "description": "是否替换所有匹配,默认 false。true 时批量替换所有 old_string"
                        }
                    },
                    "required": ["path", "old_string", "new_string"]
                })),
                strict: None,
            },
        })
    }

    async fn execute(&self, args: &str) -> anyhow::Result<String> {
        let _args: Args = serde_json::from_str(args)?;
        todo!("实现:读文件;找 old_string 匹配;replace_all=false 要求唯一,否则报错;替换后写回;返回改动信息")
    }
}
