# hi-agent

本地优先的桌面 AI 编码助手。Tauri 2 + Vue 3 + Rust，对话历史、工具执行、文件操作都在本机完成，只在需要推理时把上下文发给模型。

> **目前只支持 DeepSeek 官方 API**（`https://api.deepseek.com/chat/completions`）。不做多厂商适配，原因见[设计取舍](#设计取舍)。

## 特性

**对话**

- 流式输出，思考过程与正文分流展示，思考块可折叠
- 思考强度三档（`low` / `high` / `max`），随请求携带
- 状态栏实时显示上下文占用与硬盘缓存命中率
- 每轮结束自动刷新账户余额

**工具调用**

七个内置工具，执行前按权限裁决：

| 工具 | 作用 |
|---|---|
| `read` | 读取文件 |
| `write` | 新建或覆盖文件 |
| `edit` | 精确字符串替换，展开后是行级红绿 diff |
| `search_files` | 按文件名或内容搜索 |
| `bash` | 执行命令 |
| `web_search` | 联网搜索 |
| `web_fetch` | 抓取网页正文 |

命令执行前要过四档裁决（Allow / AskOnce / AskAlways / Deny）：Windows 用 PowerShell AST 解析，Unix 用 tree-sitter-bash，解析不出意图的一律问人。

**持久化**

- 工作区 / 会话 / 消息三层，SQLite 单库存放
- 关掉再打开，回到上次的对话与工作目录
- 历史按**轮**分页：一条用户消息到下一个用户消息为一轮，不会把一轮劈成两半
- 长对话自动压缩：上下文超过 70% 时把早期消息摘要成一段，原消息仍留在库里可回看
- 后台清理：摘要水位之前保留 2000 条消息，每个工作区保留 20 个会话

**其他**

- 窗口失焦时任务结束，系统通知 + 提示音 + 任务栏闪烁
- API Key 加密落盘（AES-256-GCM，密钥由机器标识派生）

## 快速开始

### 直接安装

到 [Releases](https://github.com/Gwyg/hi-agent/releases) 下载最新安装包：

| 产物 | 说明 |
|---|---|
| `hi-agent_*_x64-setup.exe` | NSIS 安装包，**推荐**。中文向导，装到 `%LOCALAPPDATA%` 不需要管理员权限，卸载时可选是否连同本地数据一起删除 |
| `hi-agent_*_x64_en-US.msi` | 供企业静默部署（`msiexec /i hi-agent.msi /qn`）。向导是英文，卸载不碰 `~/.hi-agent` |

安装包没有代码签名，首次运行 Windows 会提示「已保护你的电脑」，点「更多信息 → 仍要运行」即可。

### 从源码运行

环境要求：

- Node.js 18+
- Rust 1.77+（Tauri 2 的下限）
- Windows 10/11，需要 WebView2（Win11 自带）
- 目前只在 Windows 上开发和验证，macOS / Linux 未验证

```bash
npm install
npm run tauri dev
```

### 打包

```bash
npm run tauri build
```

产物在 `src-tauri/target/release/bundle/`，同时产出 NSIS 与 MSI。首次编译要几分钟（release 开了 LTO，`codegen-units = 1`）。

发版走 tag：版本号以 `src-tauri/tauri.conf.json` 为准，推一个与之一致的 tag 即可由 GitHub Actions 出包并建 draft release。

```bash
git tag v0.1.0 && git push origin v0.1.0
```

安装器本身在 `src-tauri/nsis/`：`hooks.nsh` 补了「卸载时清理 `~/.hi-agent`」（模板自带的复选框只认 `%APPDATA%\<bundle-id>`，够不到我们的数据），`header.bmp` / `sidebar.bmp` 是向导的图。图标源是 `public/logo.svg`，改完用 `npx tauri icon public/logo.svg` 重新生成全套。

> 注意：`tauri dev` 和直接双击 `target/release/hi-agent.exe` 都不会弹系统通知 —— Windows toast 需要注册 AUMID，只有装安装包后才有。开发模式下任务完成时用提示音和任务栏闪烁替代。

### 配置

首次启动点标题栏右侧的齿轮，填入 DeepSeek API Key。Key 只存在本机 `~/.hi-agent/app.toml`，落盘前加密，不上传。

## 数据位置

全部在本机：

```
~/.hi-agent/
├── app.toml           配置（含加密后的 API Key、当前工作区与会话指针）
├── data/hi-agent.db   对话数据库（SQLite，WAL 模式）
├── logs/              运行日志
└── workspace/         默认工作目录
```

删掉这个目录等同于恢复出厂设置。用 NSIS 包卸载时勾选「删除应用数据」做的就是这件事；MSI 卸载不会动它。

## 项目结构

```
src/                   前端（Vue 3 + TypeScript）
├── api/               IPC 类型定义与命令封装
├── components/        界面组件
├── composables/       状态（对话 / 会话 / 工作区 / 配置）
├── router/
├── styles/
├── utils/
└── views/

src-tauri/src/         后端（Rust）
├── commands/          IPC 入口，只做参数校验与事件转发
├── services/          业务流程（对话、会话、工作区）
├── ai/                LLM 客户端、协议编解码、工具、上下文压缩
│   ├── agent/         一轮对话的循环与上下文管理
│   └── llm/           DeepSeek 客户端、协议层、工具实现
├── store/             SQLite 存储层（引擎 + 仓储）
│   ├── engine/        连接、事务、迁移
│   ├── sql/           schema（版本化迁移）
│   └── *.rs           三张表的仓储
├── config/            配置读写
└── util/              HTTP、加密等底层工具
```

分层是单向的：`commands → services → 领域`，同层之间互不感知。有副作用的写入口收敛在一处（配置走 `config::persist`，数据库走 `store::tx`）。

## 设计取舍

**为什么只接 DeepSeek。** 各家私有扩展（思考字段形状、历史思考回传规则、工具参数格式）没有统一规范，也无法从请求或配置推断，只能靠维护一张会过期的表。对「一次只配一个端点」的桌面应用，这个成本大于收益。所以协议层是 DeepSeek 专用的，不做分派、不建方言表。上游返回 400 时原样回显错误原文，这是用户唯一能自救的路径。

**为什么自研协议层。** 带 tools 时 DeepSeek 要求把 `reasoning_content` 原样回传，多数 OpenAI 兼容库会把它当野字段丢掉，导致第二轮工具调用直接 400。这一条是 load-bearing 的，绕不开。

**为什么摘要不拿来当聊天记录显示。** 摘要会改写、合并用户原话。界面展示永远读原始消息，哪怕它们已经被挤出上下文窗口。摘要只给模型看。

**为什么按轮分页而不是按行数。** 按行截断会把一轮对话劈开：上一页末尾只剩工具卡片，下一页开头找不到对应的问题。所以分页边界落在用户消息上。

## 已知限制

- 只支持 DeepSeek，换模型需要改协议层
- 流式回复期间侧边栏禁用 —— 整轮持着上下文锁，任务后台化还没做
- 历史里的工具卡片不显示耗时和失败状态，这两项不落库
- 未接入自动更新
- 安装包没有代码签名，首次运行会被 SmartScreen 拦一次，需要手动点「仍要运行」

## 开发

```bash
npm run type-check          # 前端类型检查
npm run build               # 类型检查 + 打包

cd src-tauri
cargo test --lib            # 单元测试
cargo clippy                # lint
cargo fmt --check           # 格式检查
```

提交前建议都跑一遍。项目没有前端测试基建，前端逻辑靠类型检查和人工验证。
