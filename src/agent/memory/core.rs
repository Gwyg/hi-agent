use async_openai::types::chat::ChatCompletionRequestMessage;
use crate::llm::system;

/// 核心记忆层:常驻 LLM 上下文窗口的内容,不参与压缩。
///
/// 持有:
/// - system prompt 原始文本(身份 + 工作目录 + 行为准则)
/// - 未来:关键状态块(显式持久化的事实/决策,Claude Code MEMORY.md 路线)
///
/// view 时核心层内容始终拼在消息列表最前,工作层/归档层绝不触碰。
pub struct Core {
    /// 系统提示词原始文本(现拼时打头,不预包成 Message)
    text: String,
}

impl Core {
    pub fn new() -> Self {
        Self {
            text: build_system_prompt(),
        }
    }

    /// 构建完整 system 消息:核心提示词打头 + 调用方追加的各段(摘要 / 三级召回 / 状态块)。
    ///
    /// 调用方只管把材料递进来,不关心 system 怎么拼。记忆逻辑增长时只加段,不动接口。
    /// 各段合并为单条 system(多厂商对多条 system 支持不一),空段跳过、空行分隔。
    ///
    /// 性能:切片可扫两遍——先精确求容量再填充,字节级精准、永不 realloc,末尾交封装层原样打包。
    pub fn system(&self, extra: &[&str]) -> ChatCompletionRequestMessage {
        const SEP: &str = "\n\n";
        // 第一遍:精确算容量(核心文本 + 各非空段及其分隔符)
        let extra_len: usize = extra
            .iter()
            .copied()
            .filter(|s| !s.is_empty())
            .map(|s| SEP.len() + s.len())
            .sum();
        let mut content = String::with_capacity(self.text.len() + extra_len);
        // 第二遍:填充
        content.push_str(&self.text);
        for &s in extra {
            if s.is_empty() {
                continue;
            }
            content.push_str(SEP);
            content.push_str(s);
        }
        system(&content)
    }

    // 未来:关键状态块的读写(read_state / update_state)
}

/// 构造系统提示词:身份 + 工作目录 + 行为准则
/// 工具详情不在此重复(模型从 tools schema 获取),只给高层准则
fn build_system_prompt() -> String {
    let root = crate::llm::tools::sandbox::project_root()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "未知".to_string());
    include_str!("system_prompt.md").replace("%%ROOT%%", &root)
}
