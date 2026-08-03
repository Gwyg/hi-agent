mod agent;
mod config;
mod llm;
mod tui;
mod uninstall;

use llm::tools::sandbox;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // `hi uninstall`:跨平台卸载,不进入 TUI/初始化流程
    if std::env::args().nth(1).as_deref() == Some("uninstall") {
        return uninstall::run();
    }

    // 加载环境变量
    dotenvy::dotenv().ok();
    // 初始化 tracing 日志:写入 ~/.hi-agent/log/hi-agent.log(取不到家目录则退回当前目录 log/)
    // 用固定用户目录,避免 hi 在任意工作目录运行时污染用户项目
    let log_dir = dirs::home_dir()
        .map(|h| h.join(".hi-agent").join("log"))
        .unwrap_or_else(|| PathBuf::from("log"));
    std::fs::create_dir_all(&log_dir)?;
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("hi-agent.log"))?;
    tracing_subscriber::fmt::Subscriber::builder()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(move || log_file.try_clone().expect("clone log file"))
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
