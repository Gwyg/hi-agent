use async_trait::async_trait;
use serde::Deserialize;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::{Duration, Instant};

use super::support::path_scope::workspace_root;
use super::Tool;
use crate::ai::llm::types::ToolDef;
use crate::util::{resolve_lexical, StopSignal};

/// 结果硬上限(防 target/ 全量遍历刷屏)
const MAX_RESULTS: usize = 100;
/// 内容搜索时单文件大小上限(跳过大文件,防 OOM/慢)
const MAX_CONTENT_FILE_SIZE: u64 = 1024 * 1024;
/// 内容匹配行截断长度(防 minified 文件长行刷屏)
const MAX_LINE_LEN: usize = 200;
/// 单次搜索的时间预算。到点不是失败:遍历停下,已找到的命中照常返回,并在脚注里
/// 说明没搜完(见 `SearchState::finish`)。所以这个值按"能等多久"取,而不是按
/// "多久能搜完"取。
const SEARCH_TIMEOUT: Duration = Duration::from_secs(120);
/// 外层 timeout 比闭包内 deadline 宽的缓冲。内层到点能带着已找到的结果正常返回,
/// 外层只兜"单个系统调用挂死";两者相等时外层几乎必然先赢,内层的部分结果白做。
const TIMEOUT_GRACE: Duration = Duration::from_secs(15);

/// 搜索文件名或文件内容(无副作用,高频只读)
/// 默认尊重 .gitignore;include_ignored=true 穿透
pub struct SearchFilesTool;

#[derive(Deserialize)]
struct Args {
    /// 搜索范围目录,默认项目根(workspace)
    #[serde(default)]
    path: Option<String>,
    /// 文件名 glob 模式,如 "*.rs"、"**/*.toml"
    #[serde(default)]
    pattern: Option<String>,
    /// 文件内容正则表达式
    #[serde(default)]
    content: Option<String>,
    /// 穿透 .gitignore(默认 false)
    #[serde(default)]
    include_ignored: bool,
}

impl SearchFilesTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for SearchFilesTool {
    fn name(&self) -> &str {
        "search_files"
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "search_files",
            format!(
                "搜索本地文件:按路径 glob 或内容正则。无副作用。\n\
                适用:找某文件在哪(path 不确定);找哪些文件含某文本;列出目录下所有文件(递归,不传 pattern/content)。\n\
                不适用:读已知路径文件内容用 read;看命令输出用 bash;联网搜索不支持(本地专用)。\n\
                默认尊重 .gitignore(跳过 target/、node_modules/ 等)。include_ignored=true 会同时穿透 .gitignore 与隐藏文件规则(连 .env、.git/ 一起搜)。\n\
                pattern 匹配相对搜索目录的路径,`*` 可跨目录(`*.rs` 也能匹配 src/a.rs);content 逐行正则(输出 路径:行号:内容)。两者可同时用:先 glob 筛文件,再正则筛内容。\n\
                最多返回 {MAX_RESULTS} 条绝对路径,超出会截断并提示。"
            ),
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "搜索范围目录,默认项目根"
                    },
                    "pattern": {
                        "type": "string",
                        "description": "路径 glob 模式,相对搜索目录匹配;`*` 可跨目录(如 *.rs、src/*.toml)"
                    },
                    "content": {
                        "type": "string",
                        "description": format!(
                            "文件内容正则,逐行匹配;单文件超过 {}MB 跳过",
                            MAX_CONTENT_FILE_SIZE / (1024 * 1024)
                        )
                    },
                    "include_ignored": {
                        "type": "boolean",
                        "description": "穿透 .gitignore 与隐藏文件规则(会搜到 .env、.git/ 等),默认 false"
                    }
                },
                "required": []
            }),
        )
    }

    async fn execute(&self, args: &str) -> anyhow::Result<String> {
        // 序列化参数
        let args: Args = serde_json::from_str(args)
            .map_err(|e| anyhow::anyhow!("search_files 参数解析失败: {e}"))?;
        // 路径解析:绝对路径直通,相对路径拼工作目录根;不传就搜项目根(必须有基准)
        // 用词法解析(不 canonicalize):搜索目标可能不存在,遍历时返回空结果比报错更合适
        let base = workspace_root().ok();
        let root = match args.path {
            Some(p) => resolve_lexical(&p, base.as_deref())?,
            None => {
                base.ok_or_else(|| anyhow::anyhow!("工作目录未初始化(启动时需先确定工作区)"))?
            }
        };
        let pattern = args.pattern;
        let content_re = args.content;
        let include_ignored = args.include_ignored;

        // 编译 glob/正则
        let glob_matcher = pattern
            .as_deref()
            .map(glob::Pattern::new)
            .transpose()
            .map_err(|e| anyhow::anyhow!("glob 模式无效: {e}"))?;
        let content_matcher = content_re
            .as_deref()
            .map(regex::Regex::new)
            .transpose()
            .map_err(|e| anyhow::anyhow!("正则表达式无效: {e}"))?;
        // 检查搜索路径存在性
        if !Path::new(&root).exists() {
            return Err(anyhow::anyhow!("搜索路径不存在: {root}"));
        }
        let timeout_path = root.clone();

        // 停止标记:闭包内轮询,外层 future drop 时置位(调用方取消或超时)
        let stop = StopSignal::new();
        // 句柄:能力与 owner 相同,只是消失时不产生任何影响
        let handle = stop.clone();
        // :闭包内轮询,到点就中止,但仍带回已找到的结果(不丢掉)
        let deadline = Instant::now() + SEARCH_TIMEOUT;

        // 外层宽 TIMEOUT_GRACE:让内层先到点、带着部分结果返回,而不是被外层掐断。
        let outer = SEARCH_TIMEOUT + TIMEOUT_GRACE;
        let joined = tokio::time::timeout(
            outer,
            tokio::task::spawn_blocking(move || {
                search_sync(
                    Path::new(&root),
                    glob_matcher.as_ref(),
                    content_matcher.as_ref(),
                    include_ignored,
                    &handle,
                    deadline,
                )
            }),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "搜索超时(>{outer:?},路径 {timeout_path}):IO 长时间无响应,可能磁盘或网络盘异常;可缩小 path 重试,或告知用户检查磁盘"
            )
        })?;

        Ok(joined??)
    }
}

/// 一次搜索在遍历中累积的全部事实,外加把它拼成给模型看的文本的方法。
///
/// 所有字段的唯一用途都是"输出":正文来自 `matches`,脚注来自后面六项。所以它是
/// 一个内聚的集合("待汇报的事实"),不是杂物间——加字段前先问:输出会用到它吗?
///
/// 不拆成"计数结构体 + 结局枚举":`scanned` 会在两处重复,而这里的写入点只有四处、
/// 全在同一个函数里,一眼追得完;多一层类型反而要在两处来回对照。
#[derive(Default)]
struct SearchState {
    matches: Vec<String>,
    scanned: usize,    // 已检查的普通文件数:只描述规模,不含任何顺序含义
    unreadable: usize, // 无权限/损坏/打开失败:条目级与文件级两处汇总
    oversized: usize,  // 超过单文件上限,整份未读
    binary: usize,     // 非 UTF-8,读到一半放弃(已读部分的命中保留)
    truncated: bool,   // 命中触顶。与 `timed_out` 互斥:两者都会立即 break 出循环
    timed_out: bool,   // 到点中止:不说明的话,"无匹配"会被读成全库都没有
}

impl SearchState {
    /// 内部状态 → 交给模型看的文本:正文一条,脚注至多两条。
    ///
    /// 独立成方法不是为了省行数,而是把"措辞"从"遍历"里分离:这是全工具唯一决定
    /// 输出长什么样的地方,也是唯一需要反复推敲"模型会怎么读"的地方。
    fn finish(&self) -> String {
        let mut out = if self.matches.is_empty() {
            "无匹配".to_string()
        } else {
            self.matches.join("\n")
        };
        // 截断是唯一会让输出"悄悄变少"的地方:不只说截断,还要说清后果与下一步,
        // 否则模型会把 100 条当成全部命中、就此收手。整句大小恒定,不随目录规模增长。
        if self.truncated {
            out.push_str(&format!(
                "\n\n(结果已达上限 {MAX_RESULTS},截断:其后未继续搜索,实际命中可能远多于此;加 pattern 或收紧 content 后重搜)"
            ));
        } else if self.timed_out {
            // 超时同样会让正文说谎:正文是"无匹配"时,模型会读成"全库都没有"。
            // 所以只说两件事——看到了多少、下一步怎么走。
            //
            // 不报"停在哪":遍历不排序,那个位置在非 NTFS 上不可解释,是条推不动的线索。
            // 也不把"范围大"和"盘慢"拆成两支去猜:两分钟走了多少文件,分不清冷缓存、
            // 杀毒扫描和网络盘;猜错(把正常盘诊断成故障)比合并措辞(慢盘上多试一次
            // 缩范围)代价高,而后者只是多一轮、不会得出错误结论。
            let secs = SEARCH_TIMEOUT.as_secs();
            let note = if self.scanned == 0 {
                format!(
                    "\n\n(未搜完:超过 {secs} 秒尚未检查到任何文件,结果不可用;该路径可能不可读或在极慢的盘上,可换 path 重试)"
                )
            } else {
                format!(
                    "\n\n(未搜完:超过 {secs} 秒已检查 {} 个文件仍未搜完,结果不完整;搜索范围可能过大或磁盘较慢,建议缩小 path、加 pattern/content 过滤,或告知用户后换更快的路径重搜)",
                    self.scanned
                )
            };
            out.push_str(&note);
        }
        // 三种跳过原因分开报,模型才能判断"没搜全"的程度——合成一句"跳过 3 个"
        // 会丢掉"是权限问题还是大小问题"这个区别。全为零时不出现,不占输出。
        let mut parts = Vec::new();
        if self.unreadable > 0 {
            parts.push(format!("{} 个无法访问", self.unreadable));
        }
        if self.oversized > 0 {
            parts.push(format!(
                "{} 个超过 {}KB 未搜索",
                self.oversized,
                MAX_CONTENT_FILE_SIZE / 1024
            ));
        }
        if self.binary > 0 {
            parts.push(format!("{} 个二进制/非 UTF-8", self.binary));
        }
        if !parts.is_empty() {
            out.push('\n');
            out.push_str(&format!("(跳过 {})", parts.join("、")));
        }
        out
    }
}

/// 同步搜索核心
fn search_sync(
    root: &Path,
    glob: Option<&glob::Pattern>,
    content: Option<&regex::Regex>,
    include_ignored: bool,
    stop: &StopSignal,
    deadline: Instant,
) -> anyhow::Result<String> {
    // ignore::WalkBuilder:ripgrep 同源,默认尊重 .gitignore/.gitignore_global/.git/info/exclude
    let walker = ignore::WalkBuilder::new(root)
        .hidden(!include_ignored) // include_ignored=false 跳隐藏文件
        .ignore(!include_ignored) // .ignore 文件
        .git_ignore(!include_ignored) // .gitignore
        .git_global(!include_ignored) // ~/.config/git/ignore
        .git_exclude(!include_ignored) // .git/info/exclude
        // 不设 sorter,沿用 readdir 原生序:walkdir 的 sorter 只拿到文件名、看不到类型,
        // 做不出 `dir`/资源管理器那种"目录一组、文件一组";自造的序既不是系统序也不是
        // 用户序,调用方照着它推理反而会错。原生序对同一目录稳定(由文件名决定),
        // 所以"命中触顶取前 100 条"照样可复现。
        .build();

    let mut state = SearchState::default();

    for entry in walker {
        // 检查外层是否超时
        if stop.is_stopped() {
            // 让线程立刻放手,不再把仓库走完。仍用 Err 返回,让调用方知道是中止而不是正常结束。
            return Err(anyhow::anyhow!("搜索已中止"));
        }
        // 检查内层 deadline 是否超时:到点就中止,但仍带回已找到的结果(不丢掉)
        if Instant::now() >= deadline {
            state.timed_out = true;
            break;
        }
        // 确保条目正常,否则跳过继续
        let entry = match entry {
            Ok(e) => e,
            Err(_) => {
                state.unreadable += 1;
                continue; // 跳过无权限/损坏的条目
            }
        };
        // 只处理普通文件,跳过目录/链接/设备等继续
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        state.scanned += 1;
        let path = entry.path();

        // 如果有文件名匹配，使用 glob 过滤文件名
        if let Some(glob) = glob {
            let rel = path.strip_prefix(root).unwrap_or(path);
            if !glob.matches_path(rel) {
                continue;
            }
        }

        // 如果没有内容匹配，直接收集路径
        let Some(content) = content else {
            if state.matches.len() >= MAX_RESULTS {
                state.truncated = true;
                break;
            }
            state.matches.push(path.display().to_string());
            continue;
        };

        // 内容搜索:单文件失败只计数,不中止全搜
        match scan_file(&path, content, &mut state.matches) {
            FileOutcome::Done => {}
            FileOutcome::Unreadable => state.unreadable += 1,
            FileOutcome::Oversized => state.oversized += 1,
            FileOutcome::Binary => state.binary += 1,
            FileOutcome::Full => {
                state.truncated = true;
                break;
            }
        }
    }

    Ok(state.finish())
}

/// 单文件内容搜索的结局。命中行已直接追加进外部 Vec(不为每文件分配一份),
/// 这里只带回"为什么没搜全"——由主循环翻译成计数或整体跳出。
enum FileOutcome {
    Done,       // 读完(命中可能为零条)
    Unreadable, // 元数据/打开失败
    Oversized,  // 超过单文件上限,未读
    Binary,     // 非 UTF-8:该文件已读部分的命中保留,其余看不到
    Full,       // 命中数触顶,应停止整个搜索
}

/// 扫描单个文件的内容命中,命中行按 `路径:行号:内容` 追加进 `out`。
fn scan_file(path: &Path, content: &regex::Regex, out: &mut Vec<String>) -> FileOutcome {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return FileOutcome::Unreadable,
    };
    if meta.len() > MAX_CONTENT_FILE_SIZE {
        // 不读,但必须计数:否则模型会把"没搜"当成"没有"
        return FileOutcome::Oversized;
    }
    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return FileOutcome::Unreadable,
    };
    // 路径只格式化一次:命中密集的文件不必每行重复拼
    let path_str = path.display().to_string();
    for (i, line) in BufReader::new(file).lines().enumerate() {
        let line = match line {
            Ok(l) => l,
            Err(_) => return FileOutcome::Binary,
        };
        if content.is_match(&line) {
            if out.len() >= MAX_RESULTS {
                return FileOutcome::Full;
            }
            out.push(format!("{path_str}:{}:{}", i + 1, truncate_line(&line)));
        }
    }
    FileOutcome::Done
}

/// 按字符安全截断行(避免切在 UTF-8 多字节字符中间 panic)
/// 取前 MAX_LINE_LEN 个字符,超长加 "..."
fn truncate_line(line: &str) -> String {
    // 字节数不超上限,字符数必然也不超(1 字节/字符是下界):O(1) 返回,不碰内容
    if line.len() <= MAX_LINE_LEN {
        return line.to_string();
    }
    // 不能先 chars().count() 再 take:那要把整行扫一遍才知道该不该截,而压缩成
    // 单行的 minified 文件可能有几 MB。这里只需要"第 201 个字符起于哪个字节",
    // nth 扫到即停,代价与 MAX_LINE_LEN 成正比,与行长无关。
    let Some((cut, _)) = line.char_indices().nth(MAX_LINE_LEN) else {
        // 字节超了但字符没超(多字节字符):原样返回
        return line.to_string();
    };
    let mut out = String::with_capacity(cut + 3);
    out.push_str(&line[..cut]);
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // 传绝对路径:词法解析对绝对路径直通,不读全局 PROJECT_ROOT,测试互不干扰
    fn tmp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 没搜到的文件必须如实计数:否则模型会把"没搜"当成"没有"
    #[tokio::test]
    async fn reports_files_it_skipped() {
        let dir = tmp_dir("hi_agent_search_skip_test");
        // 超过大小上限:内容里有关键字,但不会被读
        let mut big = String::from("needle\n");
        while big.len() as u64 <= MAX_CONTENT_FILE_SIZE {
            big.push_str("padding padding padding\n");
        }
        std::fs::write(dir.join("big.log"), big).unwrap();
        // 二进制:首行即无效 UTF-8
        std::fs::write(dir.join("blob.bin"), [0xff, 0xfe, b'\n', 0xff]).unwrap();

        let args = serde_json::json!({ "path": dir, "content": "needle" }).to_string();
        let out = SearchFilesTool::new().execute(&args).await.unwrap();

        assert!(out.starts_with("无匹配"), "不应命中: {out}");
        assert!(out.contains("1 个超过"), "应报告超大文件: {out}");
        assert!(out.contains("1 个二进制"), "应报告二进制文件: {out}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 三种跳过原因分开列、无跳过时不出现——合成一句"跳过 3 个"会丢掉原因区别
    #[test]
    fn skip_note_lists_reasons_separately() {
        let out = SearchState {
            unreadable: 2,
            oversized: 1,
            binary: 3,
            ..SearchState::default()
        }
        .finish();
        assert!(out.contains("2 个无法访问"), "{out}");
        assert!(out.contains("1 个超过"), "{out}");
        assert!(out.contains("3 个二进制"), "{out}");
        assert!(out.contains('、'), "多条应顿号分隔: {out}");

        // 无跳过时脚注整段不出现:多一行空"(跳过 )"就是纯噪音
        let out = SearchState::default().finish();
        assert_eq!(out, "无匹配", "无跳过不应产生任何脚注: {out}");
    }

    /// 收尾有三种形态(正常 / 截断 / 超时),这里钉住各自的脚注文案与拼接位置
    #[test]
    fn finish_appends_footnotes() {
        // 正常结束:只有正文。此时"无匹配"才是可信的绝对结论,多加一个字都是噪音
        assert_eq!(with_hits().finish(), "a.rs");
        assert_eq!(SearchState::default().finish(), "无匹配");

        // 截断:空行 + 后果 + 下一步,且必须与"无匹配"区分开
        let out = SearchState {
            truncated: true,
            ..with_hits()
        }
        .finish();
        assert!(out.starts_with("a.rs\n\n(结果已达上限"), "{out}");
        assert!(out.contains("实际命中可能远多于此"), "{out}");

        // 超时且一条都没走到:不能说"无匹配",要说明尚未检查
        let out = SearchState {
            timed_out: true,
            ..SearchState::default()
        }
        .finish();
        assert!(out.starts_with("无匹配\n\n(未搜完"), "{out}");
        assert!(out.contains("尚未检查到任何文件"), "{out}");

        // 跳过统计永远在最后一行,不与前面的脚注混行
        let out = SearchState {
            truncated: true,
            unreadable: 1,
            ..with_hits()
        }
        .finish();
        assert!(out.ends_with("(跳过 1 个无法访问)"), "{out}");
    }

    /// 一条命中 + 其余字段全默认的状态,供上面几条用例按需覆写
    fn with_hits() -> SearchState {
        SearchState {
            matches: vec!["a.rs".to_string()],
            ..SearchState::default()
        }
    }

    /// 钉死 glob 的真实语义:`*` 跨目录(与 description 的说明一致)
    #[tokio::test]
    async fn glob_star_matches_nested_paths() {
        let dir = tmp_dir("hi_agent_search_glob_test");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "hello").unwrap();
        std::fs::write(dir.join("top.rs"), "hello").unwrap();
        std::fs::write(dir.join("top.toml"), "hello").unwrap();

        let args = serde_json::json!({ "path": dir, "pattern": "*.rs" }).to_string();
        let out = SearchFilesTool::new().execute(&args).await.unwrap();

        assert_eq!(out.lines().count(), 2, "两层 .rs 都应命中: {out}");
        assert!(!out.contains("toml"), "glob 应过滤掉 .toml: {out}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 超时不能退化成一句"无匹配":必须带上"未搜完"标志,否则模型会把没看完当成没有
    #[test]
    fn timed_out_search_reports_incomplete() {
        let dir = tmp_dir("hi_agent_search_timeout_test");
        std::fs::write(dir.join("a.txt"), "needle").unwrap();

        // deadline 立即过期:遍历到第一条就中止
        let out = search_sync(
            &dir,
            None,
            None,
            false,
            &StopSignal::new(),
            std::time::Instant::now(),
        )
        .unwrap();

        assert!(out.starts_with("无匹配"), "超时且未命中: {out}");
        assert!(out.contains("未搜完"), "应标记未搜完: {out}");
        assert!(
            out.contains("尚未检查到任何文件"),
            "应说明一条都没走到: {out}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 超时脚注:报量、报后果、给下一步;不报顺序位置
    #[test]
    fn timeout_note_reports_what_is_missing() {
        let out = SearchState {
            timed_out: true,
            scanned: 1042,
            ..SearchState::default()
        }
        .finish();
        assert!(out.starts_with("无匹配\n\n(未搜完"), "{out}");
        assert!(out.contains("1042"), "应报已检查的文件数: {out}");
        assert!(out.contains("结果不完整"), "必须说明结论不完整: {out}");
        assert!(
            out.contains("范围可能过大"),
            "应保留「范围大/盘慢」两种可能: {out}"
        );
        assert!(out.contains("缩小 path"), "应给出收窄建议: {out}");

        // 一个文件都没走到(极慢盘/搜索根本身不可读)
        let out = SearchState {
            timed_out: true,
            ..SearchState::default()
        }
        .finish();
        assert!(out.contains("尚未检查到任何文件"), "{out}");

        // 不留任何"按顺序停在哪"的措辞:顺序不可解释,给了反而误导
        assert!(!out.contains("停在"), "不应报顺序位置: {out}");
    }

    /// 截断按字符数算(不是字节数),且不能切在多字节字符中间
    #[test]
    fn truncate_line_counts_chars_not_bytes() {
        let long = "a".repeat(MAX_LINE_LEN + 50);
        assert_eq!(
            truncate_line(&long).len(),
            MAX_LINE_LEN + 3,
            "应保留 200 字符 + ..."
        );

        // 正好等于上限:不截断、不加省略号
        let exact = "a".repeat(MAX_LINE_LEN);
        assert_eq!(truncate_line(&exact), exact);

        // 多字节:字节数已超上限但字符数未超,必须原样返回(按字节切会 panic 或误截)
        let wide = "中".repeat(MAX_LINE_LEN); // 600 字节 / 200 字符
        assert!(wide.len() > MAX_LINE_LEN);
        assert_eq!(truncate_line(&wide), wide);

        // 多字节且字符数超上限:按字符切,切点必须落在字符边界上
        let wide_long = "中".repeat(MAX_LINE_LEN + 10);
        let out = truncate_line(&wide_long);
        assert_eq!(out.chars().count(), MAX_LINE_LEN + 3, "200 个汉字 + 三点");
        assert!(out.ends_with("..."), "{out}");
    }
}
