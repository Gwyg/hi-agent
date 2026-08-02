use async_openai::types::chat::{ChatCompletionTool, ChatCompletionTools, FunctionObject};
use async_trait::async_trait;
use serde::Deserialize;
use std::process::Stdio;
use std::time::Duration;

use super::bash_safety;
use super::sandbox::{project_root, resolve_path};
use super::{Action, Tool};

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

/// 默认超时(秒)
const DEFAULT_TIMEOUT: u64 = 300;
/// 单流(stdout/stderr)输出上限,防海量输出撑爆 context
const MAX_OUTPUT: usize = 64 * 1024;

#[derive(Deserialize)]
struct Args {
    command: String,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    cwd: Option<String>,
}

impl BashTool {
    pub fn new() -> Self {
        Self
    }
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
                    返回 exit_code+stdout+stderr(各截断 64KB)。超时默认 300s,超时杀进程。exit_code 非 0 是命令本身结果,自行判断。"
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
                            "description": "超时秒数,默认 300。超时杀进程"
                        },
                        "cwd": {
                            "type": "string",
                            "description": "工作目录:相对项目根、绝对路径、或 ~/开头。默认项目根"
                        }
                    },
                    "required": ["command"]
                })),
                strict: None,
            },
        })
    }

    fn assess(&self, args: &str) -> Action {
        let Ok(args) = serde_json::from_str::<Args>(args) else {
            return Action::Deny("参数解析失败".into());
        };
        bash_safety::classify(&args.command)
    }

    async fn execute(&self, args: &str) -> anyhow::Result<String> {
        let args: Args =
            serde_json::from_str(args).map_err(|e| anyhow::anyhow!("bash 参数解析失败: {e}"))?;

        let timeout_secs = args.timeout.unwrap_or(DEFAULT_TIMEOUT);
        // cwd 默认项目根;传了则解析(相对项目根/~/绝对),不做沙箱(bash 是兜底)
        let cwd = match args.cwd {
            Some(c) => resolve_path(&c)?,
            None => project_root()?,
        };

        let mut cmd = tokio::process::Command::new(SHELL);
        cmd.arg(SHELL_FLAG)
            .arg(&args.command)
            .current_dir(&cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true); // 超时/drop 时杀直接子进程(进程树 TODO:setsid+killpg/JobObject)

        let child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!("启动命令失败(shell={SHELL},cwd={}): {e}", cwd.display())
        })?;

        // tokio::process 真异步,wait_with_output 让出 worker,不阻塞 runtime
        // 超时:timeout 返回后 child future 被 drop → kill_on_drop 杀进程
        let output =
            tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output())
                .await
                .map_err(|_| anyhow::anyhow!("命令超时(>{timeout_secs}s),进程已杀"))?
                .map_err(|e| anyhow::anyhow!("命令执行失败: {e}"))?;

        let exit_code = output.status.code().unwrap_or(-1);
        // lossy 解码(Windows cmd 可能非 UTF-8),normalize \r\n → \n,截断防刷屏
        let stdout = truncate(normalize(&output.stdout));
        let stderr = truncate(normalize(&output.stderr));

        Ok(format!(
            "exit_code: {exit_code}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
        ))
    }
}

/// lossy 解码 + 统一换行(\r\n → \n)
fn normalize(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace("\r\n", "\n")
}

/// 字符安全截断到 MAX_OUTPUT 字节(不切 UTF-8 中间)
fn truncate(s: String) -> String {
    if s.len() <= MAX_OUTPUT {
        return s;
    }
    // 从 MAX_OUTPUT 往前找最近的字符边界,避免切碎多字节字符
    let mut end = MAX_OUTPUT;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n...(输出截断,共 {} 字节)", &s[..end], s.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;

    static ROOT_LOCK: Mutex<()> = Mutex::new(());

    fn setup() -> PathBuf {
        let dir = std::env::temp_dir().join("hi_agent_bash_test");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        super::super::sandbox::set_project_root(dir.clone()).unwrap();
        dir.canonicalize().unwrap()
    }

    #[tokio::test]
    async fn echo_stdout() {
        let _g = ROOT_LOCK.lock().unwrap();
        let _dir = setup();
        let tool = BashTool::new();
        let args = serde_json::json!({ "command": "echo hello" }).to_string();
        let res = tool.execute(&args).await.unwrap();
        assert!(res.contains("exit_code: 0"), "{res}");
        assert!(res.contains("hello"), "{res}");
    }

    #[tokio::test]
    async fn nonzero_exit_is_ok() {
        let _g = ROOT_LOCK.lock().unwrap();
        let _dir = setup();
        let tool = BashTool::new();
        // exit 3 是命令本身结果,应返 Ok 含 exit_code
        let args = serde_json::json!({ "command": "exit 3" }).to_string();
        let res = tool.execute(&args).await.unwrap();
        assert!(res.contains("exit_code: 3"), "{res}");
    }

    #[tokio::test]
    async fn timeout_kills() {
        let _g = ROOT_LOCK.lock().unwrap();
        let _dir = setup();
        let tool = BashTool::new();
        let args = serde_json::json!({ "command": "sleep 5", "timeout": 1 }).to_string();
        let res = tool.execute(&args).await;
        assert!(res.is_err(), "应超时");
        assert!(res.unwrap_err().to_string().contains("超时"));
    }

    #[tokio::test]
    async fn cwd_defaults_to_root() {
        let _g = ROOT_LOCK.lock().unwrap();
        let dir = setup();
        let tool = BashTool::new();
        let args = serde_json::json!({ "command": "pwd" }).to_string();
        let res = tool.execute(&args).await.unwrap();
        assert!(
            res.contains(&*dir.to_string_lossy()),
            "pwd 应为项目根: {res}"
        );
    }

    #[test]
    fn truncate_respects_char_boundary() {
        // 构造超过上限、含多字节字符的串,截断不应 panic
        let s = "中".repeat(MAX_OUTPUT); // 每字符 3 字节
        let out = truncate(s);
        assert!(out.contains("输出截断"));
    }
}
