use std::sync::OnceLock;

mod compaction;
use compaction::CompactingHistory;

use crate::ai::llm::types::{Msg, Usage};
use crate::config::workspace_path;
use tauri::async_runtime::Mutex;

/// 一条消息的落地回调:`add` 在**入内存之前**调它,失败即整条不入内存。
///
/// 顺序不能反。恢复上下文靠「库里的条数」把压缩水位往后数,内存比库多一条,
/// 水位就会算偏,下次启动会把已压缩的消息重新装回去。所以这里宁可让本轮报错,
/// 也不接受「内存有、库里没有」的中间态。
///
/// 上下文层只认识这个签名,不认识数据库:真正的写库由 `services` 闭包实现。
///
/// 带生命周期参数:实现方通常借用外层的 `Store`,不该被迫做成 `'static`。
pub type PersistFn<'a> = dyn Fn(&Msg) -> anyhow::Result<()> + Send + Sync + 'a;

/// 会话上下文门面:对外暴露 `add` / `messages` / `record_usage` / `reset`。
///
/// 职责单一:管理"每轮喂给 LLM 的消息视图"(常驻系统提示词 + 历史摘要 + 近期原始消息)。
/// 压缩(滚动缓冲 + 后台摘要)完全在内部 `compaction` 模块完成,调用方无感知:
/// - `add` 先调落地回调写库,再追加进缓冲,并按最近一次请求的真实输入判断是否触发后台压缩;
/// - `record_usage` 回填真实 token(每次主对话请求返回后调用);
/// - `messages` 取视图前先 `settle` 落地上一轮压缩结果。
///
/// 压缩摘要直接调 `llm::chat`,不需要注入任何客户端对象:配置是全局单例,
/// 每次调用读到的都是最新的。
///
/// 注意:消息序列本身在本结构里只是**工作副本**,真相在持久化层。跨会话长期记忆
/// 属于未来的独立记忆工具,不在此上下文管理器职责内。
pub struct Context {
    /// 系统提示词(身份/准则):常驻、不参与压缩,永远是 LLM 视图的第一条消息
    system_prompt: String,
    /// 压缩式会话历史(上下文管理的一种具体实现,内部模块)
    history: CompactingHistory,
}

impl Context {
    pub fn new() -> Self {
        Self {
            system_prompt: build_system_prompt(),
            history: CompactingHistory::new(),
        }
    }

    /// 追加一条对话消息(user / assistant / tool / assistant-with-tools),统一入口。
    ///
    /// 先落库、再入内存:落库失败会整条丢弃并上抛,由调用方中止本轮。
    /// 压缩判定在内部进行,调用方无需关心。
    pub fn add(&mut self, msg: Msg, persist: &PersistFn<'_>) -> anyhow::Result<()> {
        persist(&msg)?;
        self.history.push(msg);
        Ok(())
    }

    /// 主对话请求返回后回写用量:压缩内核据此把新增消息校准成真实 token,
    /// 并用 `input_tokens` 判水位。摘要请求的用量不要传进来(那是另一个请求前缀)。
    pub fn record_usage(&mut self, usage: Usage) {
        self.history.record_usage(usage);
    }

    /// 返回发给 LLM 的消息视图:先落地上一轮触发的后台压缩,再拼
    /// 单条 system(系统提示词 + 历史摘要) + 近期原始消息。
    ///
    /// 返回值直接就是 `Vec<Msg>`,不需要 `Result` —— 消息构造不再有失败路径。
    pub async fn messages(&mut self) -> Vec<Msg> {
        let (summary, messages) = self.history.view().await;
        let mut out = Vec::with_capacity(1 + messages.len());
        out.push(self.build_system_message(summary.as_deref()));
        out.extend(messages);
        out
    }

    /// 取走「本次压掉几条」并清零。
    ///
    /// **不落地进行中的压缩**:收尾时等后台摘要会把整轮的锁多占几秒,而这段等待
    /// 对用户没有收益——摘要没写回去,下次恢复时旧一点,顶多再压一次。
    ///
    /// 返回值 > 0 蕴含「摘要已是压缩后的新文本」:这个计数只在落地成功时累加,
    /// 所以调用方可以放心把当前摘要和历史水位一起写。
    pub fn take_compacted(&mut self) -> usize {
        self.history.take_compacted()
    }

    /// 最新摘要。空 = 从未压缩。
    pub fn summary(&self) -> Option<&str> {
        self.history.summary()
    }

    /// 清空会话(对应"新对话" / 切换工作目录)。
    /// - 重建系统提示词:重新渲染 `system_prompt.md`,把 `%%ROOT%%` 换成**当前**项目根
    ///   (切换工作目录后根已变,旧 system 会说过期路径,必须重建)。
    /// - 清空压缩式历史(消息/摘要/进行中压缩任务/待回写条数)。
    pub fn reset(&mut self) {
        self.system_prompt = build_system_prompt();
        self.history.reset();
    }

    /// 换上一段已有会话(用户在侧边栏点了某个历史会话)。
    ///
    /// `msgs` 只能是水位之后的:之前的内容已经压进 `summary`,两样一起给就是全量历史。
    /// 系统提示词同样要重建 —— 会话属于哪个工作区是数据决定的,不该假设和当前一致。
    pub fn restore(&mut self, summary: String, msgs: Vec<Msg>) {
        self.system_prompt = build_system_prompt();
        self.history.restore(Some(summary), msgs);
    }

    /// 组装 system 消息:模板末尾自带空行,有摘要时把正文直接追加在后面。
    /// 章节标题与说明已写在 system_prompt.md 内,无摘要时该节为空,system 内容恒定。
    fn build_system_message(&self, summary: Option<&str>) -> Msg {
        let mut content = self.system_prompt.clone();
        if let Some(s) = summary {
            content.push_str(s);
        }
        Msg::system(content)
    }
}

/// 构造系统提示词:身份 + 工作目录 + 行为准则 + 历史摘要节标题。
/// 从同目录 system_prompt.md 读取模板,把 %%ROOT%% 替换成当前项目根目录。
/// 模板末尾的空行是 build_system_message 直接追加摘要正文的依赖,别被编辑器裁掉。
fn build_system_prompt() -> String {
    // 同样走展示用路径:避免把 Windows `\\?\` 前缀喂给模型
    let root = workspace_path().unwrap_or_else(|| "未知".to_string());
    include_str!("system_prompt.md").replace("%%ROOT%%", &root)
}

// —— 进程级会话上下文单例 ——
// 上下文管理归上下文模块本身:持有跨 chat_stream 调用保留的实例,不依赖 Tauri 的 State 容器。
// 桌面端单会话,全局唯一实例即可。
static INSTANCE: OnceLock<Mutex<Context>> = OnceLock::new();

/// 初始化会话上下文单例(幂等:仅首次调用真正构造)。
/// 由服务层在每次对话前调用(`services::conversation::run_turn`)。
pub fn init_context() {
    INSTANCE.get_or_init(|| Mutex::new(Context::new()));
}

/// 会话上下文单例(跨命令调用保留历史)。须先经 `init_context` 初始化;
/// 调用方 `context_instance().lock().await` 取可变句柄,
/// 建议只在临界区持锁(取视图/落回复),不要把锁跨过 LLM 流式等网络 await。
pub fn context_instance() -> &'static Mutex<Context> {
    INSTANCE
        .get()
        .expect("会话上下文未初始化:请先调用 init_context")
}

/// 清空会话上下文。
/// 只有 `services::conversation::reset` 调它 —— 会话指针也要一起清,
/// 只清内存会让下一条消息继续写进上一个会话。
/// 若尚未初始化(用户未发过任何消息即点"新对话"),直接跳过——无内容可清。
pub async fn reset_context() {
    if let Some(g) = INSTANCE.get() {
        g.lock().await.reset();
    }
}
