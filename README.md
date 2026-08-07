# hi

一个运行在终端里的 AI 编程 agent（Rust + ratatui）。在任意项目目录敲 `hi` 即可启动，agent 以当前目录为工作根，帮你读写代码、执行命令。

## 安装

无需安装 Rust，装完在任意目录敲 `hi` 即可使用。

**macOS / Linux**
```bash
curl -LsSf https://github.com/Gwyg/hi-agent/releases/latest/download/hi-agent-installer.sh | sh
```

**Windows（PowerShell）**
```powershell
irm https://github.com/Gwyg/hi-agent/releases/latest/download/hi-agent-installer.ps1 | iex
```

**Windows（cmd）**
```cmd
powershell -c "irm https://github.com/Gwyg/hi-agent/releases/latest/download/hi-agent-installer.ps1 | iex"
```

安装脚本会把 `hi` 加入 PATH，重开终端后生效。

### 更新

安装器会附带独立的更新程序 `hi-agent-update`。有新版本时运行它即可自动升级：

```bash
hi-agent-update
```

（若 `hi-agent-update` 不在 PATH 中，其路径与 `hi` 相同，如 `~/.cargo/bin/hi-agent-update`。从源码构建的用户用 `git pull` + `cargo build --release` 手动更新。）

## 配置

`hi` 通过环境变量连接大模型，运行前需先配置：

| 变量 | 说明 | 默认值 |
|---|---|---|
| `API_KEY` | 模型 API 密钥（必填） | — |
| `BASE_URL` | OpenAI 兼容接口地址 | `https://api.openai.com/v1` |
| `MODEL` | 模型名 | `gpt-4o-mini` |

两种配置方式，任选其一：

**1. 环境变量**
```bash
export API_KEY="sk-..."
export BASE_URL="https://your-gateway/v1"
export MODEL="your-model"
```

**2. `.env` 文件**（放在启动 `hi` 的目录）
```dotenv
API_KEY=sk-...
BASE_URL=https://your-gateway/v1
MODEL=your-model
```

## 使用

进入任意项目目录后启动：
```bash
cd /path/to/your/project
hi
```

启动目录即 agent 的**工作根**：文件读写、命令执行都以该目录为基准并受沙箱约束。危险命令/写操作会先向你确认。

### 快捷键

| 按键 | 功能 |
|---|---|
| `Enter` | 发送消息 |
| `Shift+Enter` / `Ctrl+Enter` | 输入框内换行 |
| `←` `→` `↑` `↓` | 移动光标 |
| `PgUp` / `PgDn` | 上下翻阅历史消息 |
| `Ctrl+C` / `Ctrl+D` | 退出 |

## 进阶配置（可选）

可通过 `config.toml` 自定义沙箱白名单与权限规则，支持两级合并（项目级覆盖用户级）：

- 用户级：`~/.hi-agent/config.toml`
- 项目级：`<项目目录>/.hi-agent/config.toml`

示例：
```toml
[sandbox]
# 额外允许访问的路径
extra_allowed = ["~/some/shared/dir"]

[permissions.bash]
# 匹配到的命令直接允许 / 拒绝 / 每次询问
"git status" = "allow"
"rm *" = { ask = true }
```

日志按天滚动写入 `~/.hi-agent/log/hi-agent.log.YYYY-MM-DD`，不会污染工作目录。启动时自动清理超过保留期的旧日志（默认 7 天，可配）：

```toml
[log]
retention_days = 7   # 0 表示永不清理
```

也可用环境变量 `HI_AGENT_LOG_DIR` 自定义日志目录、`HI_AGENT_LOG_RETENTION_DAYS` 覆盖保留天数。日志级别默认 `info`，通过 `RUST_LOG` 调整（`debug`/`trace`/`warn`）。

## 从源码构建

```bash
git clone https://github.com/Gwyg/hi-agent
cd hi-agent
cargo build --release   # 产物: target/release/hi
```

## License

[MIT](./LICENSE)
