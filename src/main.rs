mod llm;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    let client = llm::LlmClient::new(llm::Toolbox::new().definitions());
    let messages = vec![llm::user("你好")];

    let response = client.chat(messages).await?;
    match response {
        llm::ChatResponse::Stop(msg) => println!("Assistant: {:?}", msg.content),
        llm::ChatResponse::ToolCalls(msg) => {
            println!("需要调用工具: {:?}", msg.tool_calls)
        }
        llm::ChatResponse::Length(msg) => println!("被截断: {:?}", msg.content),
        llm::ChatResponse::Filtered(msg) => println!("被过滤: {:?}", msg.content),
    }

    Ok(())
}
