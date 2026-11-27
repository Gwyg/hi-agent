//! 标识符：UUID v7。
//!
//! 选 v7 而不是 v4：v7 的高位是毫秒时间戳，所以**按字符串排序 ≈ 按创建时间排序**。
//! 主键用 TEXT，B 树按这个顺序插入就是近似顺序写，页分裂比随机 v4 少得多；
//! 排查问题时看一眼 id 前缀也能知道大概什么时候创建的。
//!
//! 为什么不在库层用 `INTEGER PRIMARY KEY AUTOINCREMENT`：id 要在前端、工具调用、
//! 日志之间来回传，自增整数一旦跨会话/跨设备就撞号；UUID 全局唯一，删掉半张表也不会重用。
//! 代价是每行多几个字节，对这个量级的数据无所谓。

use uuid::Uuid;

/// 生成一个新的标识符（36 字符带连字符的规范形式）。
// 仓储尚未接入，眼下没有消费者；接入阶段（会话 / 消息 ID）后删掉这个 allow。
#[allow(dead_code)]
pub fn new_id() -> String {
    Uuid::now_v7().to_string()
}

#[cfg(test)]
mod tests {
    use super::new_id;

    #[test]
    fn looks_like_a_v7_uuid() {
        let id = new_id();
        assert_eq!(id.len(), 36, "{id}");
        assert_eq!(id.as_bytes()[14], b'7', "版本位应为 7: {id}");
        assert!(
            matches!(id.as_bytes()[19], b'8' | b'9' | b'a' | b'b'),
            "变体位不对: {id}"
        );
    }

    #[test]
    fn is_unique() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            assert!(seen.insert(new_id()), "生成了重复 id");
        }
    }
}
