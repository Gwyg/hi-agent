mod agent;
mod config;
mod llm;
mod tui;

use llm::tools::sandbox;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 加载环境变量
    dotenvy::dotenv().ok();
    // 初始化 tracing 日志
    tracing_subscriber::fmt::Subscriber::builder()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    // 初始化项目根:env HI_AGENT_ROOT 优先,否则用 current_dir(CLI 启动目录即项目根)
    let root = match std::env::var("HI_AGENT_ROOT") {
        Ok(r) => PathBuf::from(r),
        Err(_) => std::env::current_dir()?,
    };
    sandbox::set_project_root(root)?;
    // 加载配置 + 初始化沙箱额外白名单(配置文件优先,无则用默认 temp_dir)
    config::init()?;
    tracing::info!("项目根: {}", sandbox::project_root()?.display());

    // agent 装配移入 TUI,main 只管启动期初始化 + 加载 TUI
    tui::run().await
}
