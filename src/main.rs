mod agent;
mod llm;

use agent::Engine;
use llm::{LlmClient, Toolbox};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt::Subscriber::builder()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let toolbox = Toolbox::new();
    let client = LlmClient::new(toolbox.definitions());
    let mut engine = Engine::new(client, toolbox);

    // TODO: CLI 循环 —— 读取用户输入 → engine.run_turn → 打印回复 → 继续下一轮
    Ok(())
}
