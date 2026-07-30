# AGENTS.md

## Build & Run
- `cargo build`, `cargo run`, `cargo test` — standard Cargo workflow.
- 需 `.env` 配 `API_KEY`、`BASE_URL`、`MODEL`（缺省 `gpt-4o-mini`、`https://api.openai.com/v1`）。
- `dotenvy` 自动加载 `.env`，缺失不报错。

## Architecture
- 单二进制 crate（`src/main.rs`），无 workspace，无 `lib.rs`。
- `src/llm/`：客户端 + 消息构造 + 工具框架。
  - `client.rs` — `LlmClient`（`new`/`with_config`/`chat`/`chat_stream`）、`ChatResponse`（按 `FinishReason` 分流 `Stop`/`ToolCalls`/`Length`/`Filtered`）。
  - `message.rs` — `system`/`user`/`assistant`/`tool_result` 消息构造函数。
  - `tools/` — 工具注册分发框架；每工具一文件，`all()` 列定义、`execute()` 按 name 分发（当前留空骨架）。
- `main.rs` 仅 demo：发一条 user 消息打印响应分流。

## Conventions
- Rust edition **2024**（需 Rust 1.85+）。无 `rust-toolchain.toml` 固定版本。
- 错误用 `anyhow::Result`；消息构造用 `.expect("valid ...")`（视为不可失败）。
- 关键依赖：`async-openai` 0.41（`chat-completion` feature）、`tokio`（full）、`futures`、`serde`/`serde_json`、`dotenvy`。
- 注释中文。
- AGENTS.md 已加入 `.gitignore`，仅本地维护。
