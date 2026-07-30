use async_openai::types::chat::{ChatCompletionTool, ChatCompletionTools, FunctionObject};
use async_trait::async_trait;
use serde::Deserialize;

use super::Tool;

/// 执行 shell 命令(有副作用,高危,万能兜底)
/// 支持超时和工作目录,返回退出码+stdout+stderr
pub struct BashTool;

// 平台相关的 shell 入口(编译期分流,各平台二进制只含对应分支)
#[cfg(unix)]
const SHELL: &str = "sh";
#[cfg(unix)]
const SHELL_FLAG: &str = "-c";
#[cfg(windows)]
const SHELL: &str = "cmd";
#[cfg(windows)]
const SHELL_FLAG: &str = "/C";

#[derive(Deserialize)]
struct Args {
    command: String,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    cwd: Option<String>,
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn definition(&self) -> ChatCompletionTools {
        // 用 cfg! 在 description 注入当前编译目标平台,告知模型该生成哪种语法的命令
        let platform = if cfg!(target_os = "windows") {
            "Windows (cmd /C)。命令用 Windows 语法(dir/type/findstr,%VAR% 取环境变量)"
        } else if cfg!(target_os = "macos") {
            "macOS (POSIX sh)。命令用 Unix 语法(ls/cat/grep,$VAR 取环境变量)"
        } else {
            "Unix-like (POSIX sh)。命令用 Unix 语法(ls/cat/grep,$VAR 取环境变量)"
        };

        ChatCompletionTools::Function(ChatCompletionTool {
            function: FunctionObject {
                name: "bash".to_string(),
                description: Some(format!(
                    "执行 shell 命令。有副作用,高风险,万能兜底。\n\
                    当前系统: {platform}\n\
                    适用:跑测试/构建/脚本;装依赖;git 操作;调 CLI 工具(rg/gh/npm);看命令输出(ls/ps/date);网络请求(curl)。\n\
                    不适用:读文件内容用 read(更结构化);改文件用 edit/write(更安全可控);搜代码用 search_files(无副作用)。\n\
                    优先用专用工具(read/write/edit/search_files),它们更安全、输出更结构化、权限更好配。bash 是兜底,专用工具做不到时再用。\n\
                    返回 exit_code+stdout+stderr。超时默认 30s。高危命令(rm -rf/sudo)需用户确认。"
                )),
                parameters: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "要执行的 shell 命令"
                        },
                        "timeout": {
                            "type": "integer",
                            "description": "超时秒数,默认 30。超时杀进程"
                        },
                        "cwd": {
                            "type": "string",
                            "description": "工作目录,默认项目根"
                        }
                    },
                    "required": ["command"]
                })),
                strict: None,
            },
        })
    }

    async fn execute(&self, args: &str) -> anyhow::Result<String> {
        let _args: Args = serde_json::from_str(args)?;
        // 用 SHELL/SHELL_FLAG 启进程(平台分流在 const 层已定):
        //   let mut cmd = tokio::process::Command::new(SHELL);
        //   cmd.arg(SHELL_FLAG).arg(&args.command);
        //   ... timeout/cwd/stdout/stderr 收集
        todo!("实现:用 SHELL/SHELL_FLAG 起子进程跑 command;timeout 超时杀;cwd 设工作目录;返回 exit_code+stdout+stderr 结构化 JSON")
    }
}
