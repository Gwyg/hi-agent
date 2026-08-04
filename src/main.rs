mod agent;
mod config;
mod llm;
mod log_init;
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

    // 日志目录:env(HI_AGENT_LOG_DIR)> ~/.hi-agent/log > 当前目录 log/
    let log_dir = log_init::resolve_log_dir();
    std::fs::create_dir_all(&log_dir)?;

    // 先用最小化日志写启动期信息(配置加载后才能拿 retention_days)
    // 用 daily rolling + non-blocking,guard 必须 hold 到进程末尾
    let (nb_writer, guard) = log_init::init(log_dir.clone());
    tracing_subscriber::fmt::Subscriber::builder()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(nb_writer)
        .with_ansi(false) // 日志文件不要 ANSI 颜色码
        .init();
    tracing::info!("=== hi-agent 启动 ===");
    tracing::info!("日志目录: {}", log_dir.display());

    // 初始化项目根:env HI_AGENT_ROOT 优先,否则用 current_dir(CLI 启动目录即项目根)
    let root = match std::env::var("HI_AGENT_ROOT") {
        Ok(r) => PathBuf::from(r),
        Err(_) => std::env::current_dir()?,
    };
    sandbox::set_project_root(root)?;
    // 加载配置 + 初始化沙箱额外白名单(配置文件优先,无则用默认 temp_dir)
    config::init()?;
    let config = config::get()?;
    let retention_days = config.log.retention_days();
    // subscriber 已 init,清理日志落盘
    log_init::cleanup_old_logs(&log_dir, retention_days);
    let llm_cfg = &config.llm;
    let model = llm_cfg.model();
    let base_url = llm_cfg.base_url();
    // api_key 脱敏:只显示前 6 位 + 末 4 位
    let api_key_display = {
        let k = llm_cfg.api_key();
        if k.len() > 12 {
            format!("{}...{}", &k[..6], &k[k.len() - 4..])
        } else if k.is_empty() {
            "<未配置>".to_string()
        } else {
            "<已配置但过短,不显示>".to_string()
        }
    };
    tracing::info!("项目根: {}", sandbox::project_root()?.display());
    tracing::info!("模型: {model} | base_url: {base_url} | api_key: {api_key_display}");
    tracing::info!("日志保留天数: {}", retention_days);

    // 持有 guard 到 main 末尾,保证尾部日志 flush
    let result = tui::run().await;
    drop(guard);
    result
}
