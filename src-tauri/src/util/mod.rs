//! 通用工具模块：按工具类别分子模块（现 `files` / `toml` / 未来的 `string` / `time` / …），
//! 本文件只做**模块声明与再导出**，不存放任何实现——具体工具逻辑各自归位到对应子模块。
//!
//! 设计原则与 `config` 一致：`mod.rs` 只当门面（声明 + 再导出），实现下沉到子文件，
//! 避免「所有工具堆在一个 mod 文件里」。新增一类工具 → 新建 `util/<category>.rs` 并在此声明 + 再导出。
//!
//! 当前类别：
//! - `files`：项目数据文件的 IO 原语（应用数据根目录 + 原子整份写），
//!   被配置 / 授权记忆 / 窗口几何等「小且整份读写」的数据共用。
//!   只增不改的结构化数据不进这里，走 `store` 的 SQLite 单库。
//! - `toml`：TOML 文档的定点编辑原语（建段 / 修坏段 / 沿用装饰保住注释），文本进文本出、不碰磁盘。
//!   与 `files` 合起来才是「安全地改配置文件」，但两者都不知道配置文件长什么样。
//! - `path`：路径处理原语（展开 `~` / 归一化成比对键 / 词法解析 / 真实路径 / 去 Windows
//!   扩展前缀）。**字符串进出**：它处理路径的文本表示，不打开文件，`Path` 只在内部出现。
//!   基准由调用方传入，这里不认识任何配置。
//!   配置域与工具侧共用同一份实现——比对的必须是同一形式。可能兑现不了承诺的三个返回
//!   `Result`，纯字符映射的两个恒有输出，故仍返回 `String`。
//! - `stop`：协作式停止信号。`spawn_blocking` 里的长循环不随 future 一起 drop，
//!   超时 / 取消后要让它尽快收手，只有这一条渠道（循环轮询 + owner 的 drop 置位）。
//! - `http`：发 HTTP 请求的基础设施（连接池 + `get` / `post` / `json`）。只收完整 URL、
//!   请求头列表、已编码 body、超时，返回原始 `Response`；不认识任何厂商、端点路径、配置
//!   或 header 名，**不是** `ai/llm/tools` 下模型可调用的工具。三个函数名字太泛，不提到
//!   门面，调用方写 `util::http::get(...)`。
//! - `secret`：单一敏感值的本机密文（`enc:v1:` 前缀 + AES-256-GCM，密钥由本机标识派生）。
//!   只做「字符串进、字符串出」，不认识配置文件、不读写磁盘；用途是让被拷走的
//!   `app.toml` 解不出 api_key。
//! - `time`：时间口径（Unix 毫秒，`now_ms()`）。库里所有时间戳统一用它，
//!   不用 SQLite 的 `datetime()` 字符串——整数比较不受格式/时区影响。
//! - `id`：标识（UUID v7）。高位是毫秒时间戳，按字符串排序≈按创建时间排序，
//!   主键用它比自增整数更耐跨会话/跨设备。

pub mod files;
pub mod http;
pub mod id;
pub mod path;
pub mod secret;
pub mod stop;
pub mod time;
pub mod toml;

pub use files::{app_dir, atomic_write, read_to_string_or_default};
pub use id::new_id;
pub use path::{
    canonical_path, dir_name, expand_tilde, normalize_path, resolve_lexical, strip_verbatim,
};
pub use stop::StopSignal;
pub use time::now_ms;
// `self::` 是必须的写法:模块名 `toml` 与外部 crate `toml` 同名,不加前缀时读者(和编译器)
// 都得分神判断 `toml::` 指的是哪一个。
pub use self::toml::{edit_text, Value};
