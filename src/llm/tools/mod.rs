use async_openai::types::chat::ChatCompletionTools;

// 每个工具一个文件(如 weather.rs),包含:
//   - definition(): 返回 ChatCompletionTools(给 LLM 的 schema,包成 Function variant)
//   - execute(args): 实际执行逻辑
// 新增工具:1) 建 tools/xxx.rs  2) 在这里 mod xxx  3) 注册到 all() 和 execute()
// mod weather;

/// 返回所有工具的定义,供 LlmClient 构造时自动加载
pub fn all() -> Vec<ChatCompletionTools> {
    vec![
        // weather::definition(),
    ]
}

// 根据 tool name 分发执行,供 agent 循环调用
// args 是模型生成的 JSON 字符串,需反序列化成具体工具的参数类型
// pub async fn execute(name: &str, args: &str) -> anyhow::Result<String> {
//     match name {
//         "get_weather" => weather::execute(serde_json::from_str(args)?).await,
//         _ => anyhow::bail!("unknown tool: {name}"),
//     }
// }
