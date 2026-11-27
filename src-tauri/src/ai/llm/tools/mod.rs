use crate::ai::llm::types::ToolDef;
use async_trait::async_trait;
use indexmap::IndexMap;

mod bash;
mod edit;
mod read;
mod search_files;
/// 工具的共享支撑方法子模块(角色:为各具体工具提供可复用的基础方法)
pub mod support;
mod web_fetch;
mod web_search;
mod write;

/// 返回所有工具实例,作为注册点(新增工具:1) 建 tools/xxx.rs  2) mod xxx  3) 这里 push)
/// 新工具实现完后在此注册,未实现的保持注释不注册(避免 todo!() panic 拖垮 agent)
///
/// **顺序即缓存前缀**:DeepSeek 按 prompt 连续前缀匹配,`tools` 段紧跟 system。
/// 新增工具一律 push 到末尾,别插中间、别按开关动态增删(详见 `Toolbox` 注释)。
#[allow(unused_imports)]
use {
    bash::BashTool, edit::EditTool, read::ReadTool, search_files::SearchFilesTool,
    web_fetch::WebFetchTool, web_search::WebSearchTool, write::WriteTool,
};

pub fn all() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(ReadTool::new()),
        Box::new(SearchFilesTool::new()),
        Box::new(WriteTool::new()),
        Box::new(EditTool::new()),
        Box::new(BashTool::new()),
        Box::new(WebSearchTool::new()),
        Box::new(WebFetchTool::new()),
    ]
}

/// 工具调用的安全裁决:执行前由 tool.assess() 返回,agent_loop 据此决定动作
#[derive(Debug, PartialEq)]
pub enum Action {
    /// 直接执行,无需询问
    Allow,
    /// 需询问用户,且用户可选择"之后不再问"(会话级记忆)
    /// keys = 授权键,格式由**产出它的工具**自管:bash 是 pattern 级(`rm *`,
    ///   覆盖整类命令;高危命令已在上游 Deny/AskAlways 拦截,不会进入此处)。
    ///   键**无域前缀**——分发由 `Toolbox` 按 name 完成,name 已是一等公民。
    ///   目前只有 bash 产出这档:它的「同类」窄到一条命令形状。edit/write 不走这档,
    ///   越界用 AskAlways —— 工具级 key 一次「总是允许」会把整份白名单作废
    /// 授权检查在工具 assess 内部完成(bash: classify 内 is_granted 命中即降
    ///   Allow,根本不到此档);授权记录经 `Toolbox::grant(name, keys)` 分发回
    ///   工具——生产与消费两端都不出工具层,编排层(agent_loop)只传 keys
    AskOnce { keys: Vec<String> },
    /// 需询问用户,但每次必问、永不记住。用于高危 / 未知命令,以及不适合给永久授权的
    /// 场景(如 write/edit 越界:工具级 key 一次点击就会作废整份白名单)
    AskAlways,
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
    /// 工具名,与 definition() 里的工具名一致,供 agent 按 name 分发
    fn name(&self) -> &str;

    /// 给 LLM 的工具声明
    fn definition(&self) -> ToolDef;

    /// 预检:评估本次调用的安全等级(不执行)。args 是模型生成的 JSON 字符串。
    /// 默认 Allow(只读工具不用覆盖);bash/edit/write 等有副作用的工具应覆盖此方法
    /// 返回 Action::AskOnce 时需填 keys(需授权的子命令键,由 assess 拆子命令生成)
    ///
    /// async 的原因:判定可能要外部能力(Windows 下用 PowerShell 解析器拿命令 AST),
    /// 同步签名会把进程等待压在执行线程上。默认实现仍是纯内存,async 不引入额外调度
    async fn assess(&self, _args: &str) -> Action {
        Action::Allow
    }

    /// 记录「不再询问」授权:`Action::AskOnce` 的 keys 回传入口(前端确认弹窗的
    /// 「总是允许」→ `Toolbox::grant` → 本方法)。keys 由本工具的 assess 产出,
    /// 格式与存储**工具自管**——与 assess 的自管契约同构:bash 把 pattern 原文直接
    /// 存入授权集合(键即 pattern、无域前缀,分发由 Toolbox 按 name 完成)。返回实际
    /// 记录的键(供 UI 反馈);无授权机制的工具不覆盖,默认空实现(keys 无人认领 = 什么都不记)
    fn grant(&self, _keys: &[String]) -> Vec<String> {
        Vec::new()
    }

    /// 执行。`args` 是模型生成的 JSON 字符串(框架原样),工具自行解析成自己的参数类型
    /// 返回值形态自由(结构化 JSON 字符串或纯文本),trait 不约束。
    ///
    /// **取消 = 本 future 被 drop**,编排层不做协作式通知(见 agent 循环的 `select!`)。
    /// 因此:只持有「drop 即回收」资源的工具(文件句柄、`kill_on_drop` 子进程、Drop 守卫)
    /// 天然支持取消,无需任何代码;**产生比 future 更长寿的工作的工具必须自担责任**——
    /// 典型是 `spawn_blocking` 闭包:闭包脱离了 future,drop 杀不掉它,必须自己挂 Drop
    /// 守卫把停止信号传进去(参照 search_files 的 `StopOnDrop`),否则取消后那段工作会
    /// 继续跑到自然结束。
    async fn execute(&self, args: &str) -> anyhow::Result<String>;
}

/// 工具箱:持有工具对象,按 name 分发
/// - definitions(): 导出工具声明,供 chat_stream 发给模型
/// - assess(name, args): 按 name 取工具并鉴权,返回 Action(供编排层决定 Allow/Deny/Ask)
/// - grant(name, keys): 按 name 取工具并记录「不再询问」授权(授权键的格式与
///   存储由工具自管,工具箱只做路由;与 assess/execute 同一分发形状)
/// - execute(name, args): 按 name 取工具并执行,返回结果文本
///
/// name→工具的查找是内部细节,不对外暴露(编排层只认 name + Action + 结果)
///
/// **用 IndexMap 而非 HashMap 是缓存约束,不是风格偏好**:DeepSeek 上下文硬盘缓存
/// 按 prompt 前缀匹配,`tools` 数组紧跟 system,顺序一变后面整段前缀作废(真机
/// 命中率 96% → 只剩 system 的 39%)。HashMap 每次新建换随机种子、迭代顺序不定,
/// 而 `Toolbox` 每轮用户消息都 `new()` 一次。IndexMap 的 `values()` 恒按插入序
/// (= `all()` 的注册顺序),`get(name)` 仍是 O(1);新增工具 push 到末尾,旧前缀
/// 一个字节不变。**别换回 HashMap。**
pub struct Toolbox {
    tools: IndexMap<String, Box<dyn Tool>>,
}

impl Toolbox {
    pub fn new() -> Self {
        let tools = all()
            .into_iter()
            .map(|t| (t.name().to_string(), t))
            .collect();
        Self { tools }
    }

    /// 导出所有工具的声明,供 chat_stream 构造请求
    pub fn definitions(&self) -> Vec<ToolDef> {
        self.tools.values().map(|t| t.definition()).collect()
    }

    /// 按 name 鉴权:内部按 name 取工具并 assess(args)。未知工具返回 None。
    /// 编排层(agent 循环)据此决定 Allow/Deny/Ask,并可在 Ask 时插入"问前端"环节。
    pub async fn assess(&self, name: &str, args: &str) -> Option<Action> {
        match self.tools.get(name) {
            Some(t) => Some(t.assess(args).await),
            None => None,
        }
    }

    /// 按 name 执行:内部按 name 取工具并 execute(args)。未知工具返回 Err。
    /// 与 assess 分离,使编排层能在二者之间插入确认/授权逻辑(如 Ask 弹窗)。
    pub async fn execute(&self, name: &str, args: &str) -> anyhow::Result<String> {
        match self.tools.get(name) {
            Some(t) => t.execute(args).await,
            None => Err(anyhow::anyhow!("未知工具: {name}")),
        }
    }

    /// 按 name 分发授权记录:内部按 name 取工具并 grant(keys)。未知工具返回空列表
    /// (与 assess 的 None 对应:没有工具认领这批键,就什么都不记——调用方传错工具名
    /// 不 panic,也不会误记到别家)
    pub fn grant(&self, name: &str, keys: &[String]) -> Vec<String> {
        self.tools
            .get(name)
            .map(|t| t.grant(keys))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::llm::tools::support::bash_safety::with_isolated_grants;

    /// Toolbox::grant 分发契约:按 name 路由到工具的 grant 实现——bash 收 pattern
    /// 原文键(如 `del *`,直接存入授权集合,原样返回);无授权机制的工具(read 默认
    /// 空实现)与未知工具一律不记、返回空。守住「授权记录经 Toolbox 分发、命令层
    /// 零域知识」接线。bash 分支写全局授权集合,持锁隔离(与 matcher/powershell 的
    /// 授权用例串行)
    #[test]
    fn toolbox_grant_routes_by_tool_name() {
        let _fx = with_isolated_grants();
        let tb = Toolbox::new();
        assert_eq!(
            tb.grant("bash", &["del *".to_string()]),
            vec!["del *".to_string()]
        );
        assert!(tb.grant("read", &["del *".to_string()]).is_empty());
        assert!(tb.grant("nonexistent", &["del *".to_string()]).is_empty());
    }

    /// 工具声明顺序必须跨实例稳定:两个独立的 `Toolbox::new()` 模拟连续两轮
    /// `run_turn`,`definitions()` 的顺序都得等于 `all()` 的注册顺序。
    /// 这是 DeepSeek 前缀缓存的硬约束 —— 换回 HashMap 这里几乎必然失败
    /// (每次新建换随机种子,7 个工具有 5040 种排列)。
    #[test]
    fn definitions_order_is_stable_across_instances() {
        fn names(defs: &[ToolDef]) -> Vec<&str> {
            defs.iter()
                .map(|d| match d {
                    ToolDef::Function { name, .. } | ToolDef::Custom { name, .. } => name.as_str(),
                })
                .collect()
        }

        let a_defs = Toolbox::new().definitions();
        let b_defs = Toolbox::new().definitions();
        let a = names(&a_defs);
        let b = names(&b_defs);
        let expected = vec![
            "read",
            "search_files",
            "write",
            "edit",
            "bash",
            "web_search",
            "web_fetch",
        ];
        assert_eq!(a, expected);
        assert_eq!(a, b);
    }
}
