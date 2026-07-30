mod llm;

use async_openai::types::chat::ChatCompletionMessageToolCalls;
use llm::{ChatResponse, LlmClient, Toolbox};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    let toolbox = Toolbox::new();
    let client = LlmClient::new(toolbox.definitions());

    let mut messages = vec![
        llm::system("你是一个助手,可以用 read 工具读取文件内容。"),
        llm::user("读一下 src/main.rs 看看内容"),
    ];

    loop {
        let response = client.chat(std::mem::take(&mut messages)).await?;
        match response {
            ChatResponse::Stop(msg) => {
                println!("Assistant: {:?}", msg.content);
                break;
            }
            ChatResponse::ToolCalls(msg) => {
                let tool_calls = msg.tool_calls.clone().unwrap_or_default();
                messages.push(llm::assistant_with_tool_calls(tool_calls.clone()));

                for call in &tool_calls {
                    let (id, name, args_json) = match call {
                        ChatCompletionMessageToolCalls::Function(f) => {
                            (f.id.clone(), f.function.name.clone(), f.function.arguments.clone())
                        }
                        ChatCompletionMessageToolCalls::Custom(c) => {
                            (c.id.clone(), c.custom_tool.name.clone(), c.custom_tool.input.clone())
                        }
                    };
                    let content = match toolbox.find(&name) {
                        Some(t) => match t.execute(&args_json).await {
                            Ok(s) => s,
                            Err(e) => format!(r#"{{"error":"{e}"}}"#),
                        },
                        None => format!(r#"{{"error":"unknown tool: {name}"}}"#),
                    };
                    println!("--- 工具 {name} 调用 ---");
                    println!("args: {args_json}");
                    println!("result: {content}");
                    println!("----------------------");
                    messages.push(llm::tool_result(&id, &content));
                }
                continue;
            }
            ChatResponse::Length(msg) | ChatResponse::Filtered(msg) => {
                println!("异常结束: {:?}", msg.content);
                break;
            }
        }
    }

    Ok(())
}
