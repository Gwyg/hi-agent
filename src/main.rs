mod agent;
mod llm;

use agent::Engine;
use llm::tools::sandbox;
use llm::{LlmClient, Toolbox};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt::Subscriber::builder()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // 初始化项目根:env HI_AGENT_ROOT 优先,否则用 current_dir(CLI 启动目录即项目根)
    let root = match std::env::var("HI_AGENT_ROOT") {
        Ok(r) => PathBuf::from(r),
        Err(_) => std::env::current_dir()?,
    };
    sandbox::set_project_root(root)?;
    // 初始化额外白名单:默认跨平台标准目录(temp/config/cache/data)
    // TODO: 后续可在此加载用户配置(~/.hi-agent/config.toml)叠加到默认白名单
    sandbox::set_extra_allowed(sandbox::default_extra_paths())?;
    tracing::info!("项目根: {}", sandbox::project_root()?.display());

    let toolbox = Toolbox::new();
    let client = LlmClient::new(toolbox.definitions());
    let mut engine = Engine::new(client, toolbox);

    // REPL:逐行读取用户输入 → engine.run_turn → 打印回复 → 继续下一轮
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    loop {
        write!(stdout, "user> ").ok();
        stdout.flush().ok();

        let mut input = String::new();
        if stdin.lock().read_line(&mut input)? == 0 {
            // EOF (Ctrl+D) → 退出
            break;
        }
        let input = input.trim();
        if input.is_empty() {
            continue;
        }

        match engine.run_turn(input).await {
            Ok(reply) => {
                writeln!(stdout, "assistant> {reply}").ok();
            }
            Err(e) => {
                tracing::error!("run_turn 失败: {e:#}");
                writeln!(stdout, "[错误] {e}").ok();
            }
        }
    }

    Ok(())
}
