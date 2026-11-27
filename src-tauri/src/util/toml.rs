//! TOML 文档的定点编辑原语:文本进、文本出。
//!
//! 本模块只认识「文档 / 表 / 键 / 装饰」四个概念,**不认识任何配置字段**——`app.toml` 在哪、
//! 这次该改哪些键、值是什么含义,全属于调用方的知识(见 `config::persist`)。
//!
//! 与 `util::files` 同层、各管一半:
//! - `files` 负责「字节怎么安全落盘」(原子写、目录 ensured、读失败给默认值);
//! - `toml` 负责「文档怎么安全改」(缺段补齐、坏段重建、沿用旧装饰保住注释)。
//!
//! 不做 serde 结构体 ↔ 文本的映射:那是**序列化**,要知道字段含义,归各配置域
//! (`AppConfig` / `LlmConfig` 的 `Deserialize`)。这里只处理已经解析出来的语法树。

use std::io;

use toml_edit::{DocumentMut, Item, Table};

/// 写值类型:直接用 TOML 自己的类型,天然覆盖字符串 / 整数 / 浮点 / 布尔 / 数组。
///
/// 不自造 `ConfValue` 枚举,是因为那需要和 serde 结构体字段类型人工对齐;一旦漏了某个类型,
/// 就会写出「自己读不回来」的文件(读取侧反序列化失败 → 静默回退默认 → 配置丢失)。
/// 用 TOML 原生类型,这个对齐问题从根上不存在。
pub use toml_edit::Value;

/// 对 TOML 文本做一次定点修改,返回新文本。**纯函数,不碰磁盘。**
///
/// `path` 是从文档根到目标表的路径(空切片 = 顶层,`&["llm", "provider"]` = 三层);
/// `entries` 里 `None` 表示删掉该键。
///
/// 没有列进 `entries` 的节点一律不被触碰,所以注释、空行、多行数组排版、以及程序不认识的
/// key 全部原样保留——不是「合并时抢救回来」,而是根本没人碰它们。
pub fn edit_text(
    text: &str,
    path: &[&str],
    entries: &[(&str, Option<Value>)],
) -> io::Result<String> {
    let mut doc = parse_or_new(text)?;
    let table = table_at(doc.as_table_mut(), path);
    for &(key, ref val) in entries {
        set_item(table, key, val.as_ref());
    }
    Ok(doc.to_string())
}

/// 文本 → 可编辑文档。两种情况必须分开,区别是「能不能安全地写」。
///
/// 空文本 = 文件不存在或还没写过 → 从空文档起步,首次写入自然长出目标段的表头。
/// **有内容但解析失败 → 返回 `Err`,放弃写入**。这一点必须 fail-closed:若从空白文档起步,
/// 用户手写漏一个引号就会让整份文件被覆盖成只剩本次要写的那几个键——读不懂的文件被改写成
/// 更残缺的文件,错误被放大成不可逆的数据丢失。
fn parse_or_new(text: &str) -> io::Result<DocumentMut> {
    if text.trim().is_empty() {
        return Ok(DocumentMut::new());
    }
    text.parse::<DocumentMut>().map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("TOML 语法错误,已放弃写入以免覆盖原文件: {e}"),
        )
    })
}

/// 沿 `path` 逐级取得目标表,缺失或不是表的层级就地重建。
///
/// 层级存在时什么都不做;不存在时补普通表——输出才是 `[a.b]` 表头,而不是内联表
/// `a = { b = { ... } }`。某一级被写坏成标量(如 `workspace = "x"`)时整级重建:
/// 原地覆盖会保留旧 key 的 ` = ` 装饰(输出 `[workspace ]`),且不重建则直接 panic。
fn table_at<'a>(root: &'a mut Table, path: &[&str]) -> &'a mut Table {
    let mut cur = root;
    for &name in path {
        if !cur.get(name).is_some_and(Item::is_table) {
            cur.remove(name);
            cur.insert(name, Item::Table(Table::new()));
        }
        cur = cur
            .get_mut(name)
            .and_then(Item::as_table_mut)
            .expect("上一行刚确保是表");
    }
    cur
}

/// 写或删一个键。`None` = 删键(回落默认),不是写空串。
///
/// 注释在 toml_edit 里不是独立节点,而是挂在邻居的 decor 上,**两处注释归属不同**:
/// - 行尾注释 `path = "x"   # 我的项目` → 挂在 **Value** 的 `decor().suffix`;
/// - 键上方那行独占注释 `# 项目目录\npath = "x"` → 挂在 **Key** 的 `decor().prefix`。
///
/// 所以已有标量值必须**原地换 Value**,不能 `insert` 新条目:insert 会连 Key 一起换成
/// `Key::new(key)`,Key 的 decor 归零,上方那行注释就被吞掉(行尾注释因为随 Value 走反而还在)。
fn set_item(table: &mut Table, key: &str, val: Option<&Value>) {
    match val {
        None => {
            // 删键会连 Key 一起删,键上方的注释随之消失——注释描述的就是这个键,符合预期。
            table.remove(key);
        }
        Some(v) => {
            // 值是从调用方借来的,而写入需要所有权,故克隆一份。
            let mut v = v.clone();
            if table.get(key).is_some_and(Item::is_value) {
                let old = table
                    .get_mut(key)
                    .and_then(|i| i.as_value_mut())
                    .expect("刚判断过是值");
                // 继承旧 Value 的装饰(值前空格 + 值后空白与行尾注释),Key 原封不动。
                *v.decor_mut() = old.decor().clone();
                *old = v;
            } else {
                // 没有旧值,或旧值是个表(用户写成 `[workspace.path]` / `path = { .. }`)。
                // 新条目的装饰留空即可:渲染器有默认值(`key` 前后各一个空格,见 toml_edit 的
                // DEFAULT_KEY_DECOR / DEFAULT_VALUE_DECOR),显式 set_prefix(" ") 是重复劳动。
                // 后者属于坏配置,整条重建、注释不保,可接受。
                table.insert(key, Item::Value(v));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用户手写的文件:注释、多行数组排版、未知 key、程序不认识的段。
    const SAMPLE: &str = "\
# 顶部说明,别动

[workspace]
path = \"D:/old\"          # 旧项目
extra_allowed = [
  \"~/scratch\",           # 草稿区
  \"D:/shared\",
]
sensitive_suffixes = [\".env\"]

[llm]
model = \"deepseek-chat\"    # 常用模型

[unknown_section]
whatever = 1
";

    /// 只碰声明的键:注释、多行数组、未知段全部原样,且不产生多余表头
    #[test]
    fn edit_touches_only_declared_key() {
        let out = edit_text(
            SAMPLE,
            &["workspace"],
            &[("path", Some(Value::from("D:/new")))],
        )
        .expect("示例为合法 TOML");

        assert!(out.contains(r#"path = "D:/new""#), "新值没写进去: {out}");
        assert!(out.contains("# 旧项目"), "行尾注释被吞: {out}");
        assert!(out.contains("# 顶部说明,别动"), "顶部注释被吞: {out}");
        // 多行数组排版原样(这是旧全量写回会压成一行的那个例子)
        assert!(out.contains("extra_allowed = [\n"), "多行数组被压缩: {out}");
        assert!(out.contains("# 草稿区"), "数组行内注释被吞: {out}");
        // 用户手写的清单一个字都没动
        assert!(
            out.contains("sensitive_suffixes = [\".env\"]"),
            "清单被改: {out}"
        );
        // 其他段完全不动
        assert!(
            out.contains(r#"model = "deepseek-chat"    # 常用模型"#),
            "llm 段被改: {out}"
        );
        assert!(out.contains("[unknown_section]"), "未知段被删: {out}");
        // 不该冒出重复表头或空表
        assert_eq!(out.matches("[workspace]").count(), 1, "表头重复: {out}");
    }

    /// 段/键缺失时能补出来,且形状是用户预期的一行 `key = "v"`
    /// (不是内联表 `workspace = { path = "..." }`)
    #[test]
    fn edit_creates_missing_section() {
        let out = edit_text("", &["workspace"], &[("path", Some(Value::from("D:/new")))])
            .expect("空文本从空文档起步");
        assert_eq!(
            out, "[workspace]\npath = \"D:/new\"\n",
            "空文件补段形状不对: {out}"
        );
    }

    /// 段被写坏(`workspace = "x"`)时重建成表,而不是 panic
    #[test]
    fn edit_repairs_broken_section() {
        let out = edit_text(
            "workspace = \"x\"\n",
            &["workspace"],
            &[("path", Some(Value::from("D:/new")))],
        )
        .expect("坏值不是语法错误,能解析");
        assert!(out.contains("[workspace]"), "没重建成表: {out}");
        assert!(out.contains(r#"path = "D:/new""#), "新值没写进去: {out}");
    }

    /// `None` = 删键,不是写空串
    #[test]
    fn edit_removes_key_on_none() {
        let out = edit_text(SAMPLE, &["llm"], &[("model", None)]).expect("示例为合法 TOML");
        assert!(!out.contains("model"), "键没删掉: {out}");
        assert!(out.contains("[llm]"), "整段不该被删: {out}");
    }

    /// 文本有语法错 → 拒绝写入;若这里放行,用户漏一个引号就会丢掉整份文件
    #[test]
    fn edit_refuses_broken_text() {
        let broken = "[workspace]\npath = \"D:/x\n"; // 缺右引号
        let err = edit_text(
            broken,
            &["workspace"],
            &[("path", Some(Value::from("D:/new")))],
        )
        .expect_err("语法错误必须返回 Err");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            err.to_string().contains("语法错误"),
            "错误信息不可读: {err}"
        );
    }

    /// 多个键一次写完,且只碰本段的键
    #[test]
    fn edit_writes_multiple_keys_at_once() {
        let out = edit_text(
            SAMPLE,
            &["llm"],
            &[
                ("model", Some(Value::from("new"))),
                ("base_url", Some(Value::from("https://example.com"))),
                ("api_key", Some(Value::from("sk-2"))),
            ],
        )
        .expect("示例为合法 TOML");

        assert!(out.contains(r#"model = "new""#), "model 没换: {out}");
        // 行尾注释跟着这一行走:值换了,装饰沿用
        assert!(out.contains("# 常用模型"), "行尾注释被吞: {out}");
        assert!(
            out.contains(r#"base_url = "https://example.com""#),
            "base_url 没写: {out}"
        );
        assert!(out.contains(r#"api_key = "sk-2""#), "api_key 没写: {out}");
        assert!(
            out.contains(r#"path = "D:/old"          # 旧项目"#),
            "workspace 段被改: {out}"
        );
    }

    /// 多层段:路径切片表达任意深度,中间层缺失时逐级补齐
    #[test]
    fn edit_supports_nested_section() {
        let out = edit_text(
            SAMPLE,
            &["llm", "provider"],
            &[("name", Some(Value::from("openai")))],
        )
        .expect("示例为合法 TOML");

        assert!(out.contains("[llm.provider]"), "没建出嵌套表: {out}");
        assert!(out.contains(r#"name = "openai""#), "值没写进去: {out}");
        // 原有的 [llm] 段内容不受影响
        assert!(
            out.contains(r#"model = "deepseek-chat""#),
            "llm 段被改: {out}"
        );
    }

    /// 值类型不止字符串:整数、布尔按 TOML 原生类型写出
    #[test]
    fn edit_writes_non_string_values() {
        let out = edit_text(
            "",
            &["server"],
            &[
                ("timeout", Some(Value::from(30))),
                ("enabled", Some(Value::from(true))),
            ],
        )
        .expect("空文本从空文档起步");

        assert!(out.contains("timeout = 30"), "整数写成了字符串: {out}");
        assert!(out.contains("enabled = true"), "布尔写错: {out}");
    }

    /// Windows 路径的引号与转义由 `toml_edit` 决定:含反斜杠时它宁可改用字面量串 `'...'`
    /// (无需转义),也不冒险拼出非法基本串(`"C:\Users"` 里的 `\U` 是非法转义)。
    /// 所以这里不钉死具体写法,只钉**往返一致**:写出去的内容必须能被原样读回来。
    /// ——否则下次启动读取时解析失败会静默回退默认值,用户的配置全丢。
    #[test]
    fn edit_round_trips_windows_path() {
        let out = edit_text(
            "[workspace]\npath = 'C:\\Users\\x\\proj'\n",
            &["workspace"],
            &[("path", Some(Value::from(r"C:\Users\y\proj")))],
        )
        .expect("样例必为合法 TOML");

        let back: toml::Value = toml::from_str(&out).expect("写出的内容必须能被读回");
        assert_eq!(
            back["workspace"]["path"].as_str(),
            Some(r"C:\Users\y\proj"),
            "往返后值变了: {out}"
        );
    }

    /// 注释可以出现在任何位置:文件头、键上方、键下方、表尾、文件尾。
    /// 它们都不是独立节点,而是挂在邻居的 decor 上,所以一次写入不许移动任何一个。
    #[test]
    fn edit_keeps_comments_anywhere() {
        let src = "\
# 文件头注释,不属于任何表

[workspace]
# 目标键上方(挂 Key 的 decor)
path = \"D:/old\"          # 目标键行尾(挂 Value 的 decor)
# 下一个键上方
extra_allowed = [\"a\"]     # 数组行尾
# 本段表尾

[llm]
# model 上方
model = \"x\"

# 文件尾
";
        let out = edit_text(
            src,
            &["workspace"],
            &[("path", Some(Value::from("D:/new")))],
        )
        .expect("样例为合法 TOML");

        // 目标键上方那行:以前会被 insert 换掉 Key 而吞掉,是本测试的重点
        assert!(
            out.contains("# 目标键上方(挂 Key 的 decor)"),
            "键上方注释被吞: {out}"
        );
        // 行尾注释跟着这一行走:值换了,同一行的缩进 + 注释原样
        assert!(
            out.contains(r#"path = "D:/new"          # 目标键行尾(挂 Value 的 decor)"#),
            "行尾注释或缩进被改: {out}"
        );
        for needle in [
            "# 文件头注释,不属于任何表",
            "# 下一个键上方",
            "# 数组行尾",
            "# 本段表尾",
            "# model 上方",
            "# 文件尾",
        ] {
            assert!(out.contains(needle), "注释 `{needle}` 被吞: {out}");
        }
        // 注释不参与语义,但写入结果必须仍能被解析回来
        let back: toml::Value = toml::from_str(&out).expect("写出的内容必须能被读回");
        assert_eq!(back["workspace"]["path"].as_str(), Some("D:/new"));
    }

    /// 只认 `path` 指向的那一层,根级同名键不动
    #[test]
    fn edit_ignores_root_level_same_key() {
        let out = edit_text(
            "path = 'C:\\Users\\x\\proj'\n",
            &["workspace"],
            &[("path", Some(Value::from("D:/new")))],
        )
        .expect("样例必为合法 TOML");

        assert!(
            out.contains(r#"path = 'C:\Users\x\proj'"#),
            "根级 path 被误改: {out}"
        );
    }
}
