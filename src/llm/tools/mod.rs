use async_openai::types::chat::ChatCompletionTools;
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

mod bash;
mod bash_safety;
mod edit;
mod read;
pub mod sandbox;
mod search_files;
mod write;

/// 返回所有工具实例,作为注册点(新增工具:1) 建 tools/xxx.rs  2) mod xxx  3) 这里 push)
/// 新工具实现完后在此注册,未实现的保持注释不注册(避免 todo!() panic 拖垮 agent)
#[allow(unused_imports)]
use {
    bash::BashTool, edit::EditTool, read::ReadTool, search_files::SearchFilesTool, write::WriteTool,
};

pub fn all() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(ReadTool::new()),
        Box::new(SearchFilesTool::new()),
        Box::new(WriteTool::new()),
        Box::new(EditTool::new()),
        Box::new(BashTool::new()),
    ]
}

/// 工具调用的安全裁决:执行前由 tool.assess() 返回,agent_loop 据此决定动作
pub enum Action {
    /// 直接执行,无需询问
    Allow,
    /// 需询问用户。persistable=true 时用户可选"之后不再问"(会话级记忆);
    /// persistable=false 时每次必问(高危命令)
    /// keys = 需授权的子命令键(由 assess 拆子命令生成,只含触发 Ask 的子命令)
    /// agent_loop 用 keys 做 grant_check/grant_record;persistable=false 时 keys 空
    #[allow(dead_code)] // 保留给后续 CLI 交互层(决定是否提供"不再问"选项)
    Ask {
        persistable: bool,
        keys: Vec<String>,
    },
    /// 拒绝执行,带原因(致命命令,不可覆盖)
    Deny(String),
}

/// 工具特征:一个工具 = schema 定义 + 执行逻辑,二者绑定不可分
///
/// 工具自管契约:
/// - 入参由工具内部把 JSON 字符串反序列化成自己的参数类型
/// - 返回值形态自由(结构化 JSON 字符串或纯文本),trait 不约束
/// - 参数/返回类型放工具文件内,不进 trait(避免关联类型破坏对象安全)
#[async_trait]
pub trait Tool: Send + Sync {
    /// 工具名,与 definition() 里 FunctionObject.name 一致,供 agent 按 name 分发
    fn name(&self) -> &str;

    /// 给 LLM 的 schema(包成 ChatCompletionTools::Function)
    fn definition(&self) -> ChatCompletionTools;

    /// 预检:评估本次调用的安全等级(不执行)。args 是模型生成的 JSON 字符串。
    /// 默认 Allow(只读工具不用覆盖);bash/edit/write 等有副作用的工具应覆盖此方法
    /// 返回 Action::Ask 时需填 keys(需授权的子命令键,由 assess 拆子命令生成)
    fn assess(&self, _args: &str) -> Action {
        Action::Allow
    }

    /// 执行。`args` 是模型生成的 JSON 字符串(框架原样),工具自行解析成自己的参数类型
    /// 返回值形态自由(结构化 JSON 字符串或纯文本),trait 不约束
    async fn execute(&self, args: &str) -> anyhow::Result<String>;
}

/// 工具箱:持有工具对象,按 name O(1) 查找分发
/// - definitions(): 给 LlmClient 用,导出 schema 集合
/// - find(): 给 agent 用,按 name 取工具执行
/// - grants: 会话级授权记忆(用户选 Always 后登记的 key),随 Toolbox 生命周期
///   会话隔离:每个 Toolbox 独立 grants,不串;消除全局可变状态
pub struct Toolbox {
    tools: HashMap<String, Box<dyn Tool>>,
    grants: RwLock<HashSet<String>>,
}

impl Toolbox {
    pub fn new() -> Self {
        let tools = all()
            .into_iter()
            .map(|t| (t.name().to_string(), t))
            .collect();
        Self {
            tools,
            grants: RwLock::new(HashSet::new()),
        }
    }

    /// 导出所有工具的 schema,供 LlmClient 构造请求
    pub fn definitions(&self) -> Vec<ChatCompletionTools> {
        self.tools.values().map(|t| t.definition()).collect()
    }

    /// 按 name 查找工具,O(1)
    pub fn find(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|t| t.as_ref())
    }

    /// 查会话授权:命中则跳过 emit Ask
    pub fn grant_check(&self, key: &str) -> bool {
        self.grants.read().map_or(false, |g| g.contains(key))
    }

    /// 登记授权:用户选 Always 后调
    pub fn grant_record(&self, key: String) {
        if let Ok(mut g) = self.grants.write() {
            g.insert(key);
        }
    }

    /// 撤销授权:前端 Revoke 时调(对应 Codex /approvals)
    #[allow(dead_code)]
    pub fn grant_revoke(&self, key: &str) {
        if let Ok(mut g) = self.grants.write() {
            g.remove(key);
        }
    }

    /// 列出所有已授权键(前端展示用)
    #[allow(dead_code)]
    pub fn grant_list(&self) -> Vec<String> {
        self.grants
            .read()
            .map_or(Vec::new(), |g| g.iter().cloned().collect())
    }
}
