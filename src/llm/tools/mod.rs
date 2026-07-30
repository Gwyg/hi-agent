use async_openai::types::chat::ChatCompletionTools;
use async_trait::async_trait;
use std::collections::HashMap;

mod bash;
mod edit;
mod read;
mod search_files;
mod write;

/// 返回所有工具实例,作为注册点(新增工具:1) 建 tools/xxx.rs  2) mod xxx  3) 这里 push)
/// 新工具实现完后在此注册,未实现的保持注释不注册(避免 todo!() panic 拖垮 agent)
#[allow(unused_imports)]
use {bash::BashTool, edit::EditTool, read::ReadTool, search_files::SearchFilesTool, write::WriteTool};

pub fn all() -> Vec<Box<dyn Tool>> {
    vec![
        // Box::new(ReadTool),
        // Box::new(WriteTool),
        // Box::new(EditTool),
        // Box::new(SearchFilesTool),
        // Box::new(BashTool),
    ]
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

    /// 执行。`args` 是模型生成的 JSON 字符串(框架原样),工具自行解析成自己的参数类型
    /// 返回值形态自由(结构化 JSON 字符串或纯文本),trait 不约束
    async fn execute(&self, args: &str) -> anyhow::Result<String>;
}



/// 工具箱:持有工具对象,按 name O(1) 查找分发
/// - definitions(): 给 LlmClient 用,导出 schema 集合
/// - find(): 给 agent 用,按 name 取工具执行
pub struct Toolbox {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl Toolbox {
    pub fn new() -> Self {
        let tools = all()
            .into_iter()
            .map(|t| (t.name().to_string(), t))
            .collect();
        Self { tools }
    }

    /// 导出所有工具的 schema,供 LlmClient 构造请求
    pub fn definitions(&self) -> Vec<ChatCompletionTools> {
        self.tools.values().map(|t| t.definition()).collect()
    }

    /// 按 name 查找工具,O(1)
    pub fn find(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|t| t.as_ref())
    }
}
