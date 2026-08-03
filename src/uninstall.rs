//! `hi uninstall` 子命令:跨平台卸载
//!
//! 清理三类东西:
//! 1. 运行时数据 `~/.hi-agent/`(用户级配置 + 日志)
//! 2. cargo-dist 安装回执(`~/.config/hi-agent` 等)
//! 3. 二进制自身(经 self-replace,兼容 Windows「运行中不可删」)
//!
//! PATH 清理不自动处理(改 shell rc/注册表风险大于收益),仅打印提示。
//! 项目级 `<项目>/.hi-agent/` 属用户项目数据,不删。

use std::io::Write;

/// 卸载入口:确认 → 删数据/回执 → 自删二进制 → 提示 PATH
pub fn run() -> anyhow::Result<()> {
    let home = dirs::home_dir();
    let home_data = home.as_ref().map(|h| h.join(".hi-agent"));

    // cargo-dist 安装回执候选目录:
    // - unix(含 macOS)默认 ~/.config/hi-agent
    // - 平台标准配置目录(Windows: %APPDATA%\hi-agent)
    let mut receipts: Vec<std::path::PathBuf> = Vec::new();
    if let Some(h) = &home {
        receipts.push(h.join(".config").join("hi-agent"));
    }
    if let Some(c) = dirs::config_dir() {
        let p = c.join("hi-agent");
        if !receipts.contains(&p) {
            receipts.push(p);
        }
    }

    println!("即将卸载 hi,将删除:");
    println!("  - 可执行文件本身");
    if let Some(d) = &home_data {
        println!("  - 运行时数据(配置 + 日志): {}", d.display());
    }
    for d in &receipts {
        println!("  - 安装回执: {}", d.display());
    }
    print!("继续? [y/N] ");
    std::io::stdout().flush().ok();

    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim(), "y" | "Y" | "yes" | "YES") {
        println!("已取消。");
        return Ok(());
    }

    // 删运行时数据(日志 + 用户级配置,全在一个目录)
    if let Some(dir) = &home_data {
        if dir.exists() {
            match std::fs::remove_dir_all(dir) {
                Ok(()) => println!("已删除 {}", dir.display()),
                Err(e) => eprintln!("删除 {} 失败: {e}", dir.display()),
            }
        }
    }

    // 删 cargo-dist 安装回执
    for dir in &receipts {
        if dir.exists() {
            match std::fs::remove_dir_all(dir) {
                Ok(()) => println!("已删除 {}", dir.display()),
                Err(e) => eprintln!("删除 {} 失败: {e}", dir.display()),
            }
        }
    }

    // 删二进制自身:self-replace 跨平台处理(Windows 起后台进程延迟删)
    match self_replace::self_delete() {
        Ok(()) => println!("已删除可执行文件。"),
        Err(e) => eprintln!("删除可执行文件失败: {e}"),
    }

    println!();
    println!("卸载完成。若安装器曾修改 PATH,请手动清理:");
    #[cfg(windows)]
    println!("  - Windows: 从「系统环境变量」的 Path 中删除 hi 的安装目录。");
    #[cfg(not(windows))]
    println!("  - 从 ~/.zshrc / ~/.bashrc / ~/.profile 中删除包含 hi 安装目录的 PATH 行。");

    Ok(())
}
