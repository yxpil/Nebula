//! 领域类型:一条"记忆"及其索引结构。

use crate::codec::{BinaryDecode, BinaryEncode, Reader, Writer};
use crate::error::Result;

/// 记忆 ID(自增)。
pub type MemoryId = u64;
/// Unix 毫秒时间戳。
pub type Timestamp = i64;

/// 文件格式版本号(格式结构变更时递增,与配置无关)。
pub const FORMAT_VERSION: u16 = 1;

/// 带权重的关键词。
#[derive(Debug, Clone, PartialEq)]
pub struct Keyword {
    pub term: String,
    pub weight: f32,
}

impl Keyword {
    pub fn new(term: impl Into<String>, weight: f32) -> Self {
        Keyword {
            term: term.into(),
            weight,
        }
    }
}

impl BinaryEncode for Keyword {
    fn encode(&self, w: &mut Writer) {
        self.term.encode(w);
        self.weight.encode(w);
    }
}

impl BinaryDecode for Keyword {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Ok(Keyword {
            term: String::decode(r)?,
            weight: f32::decode(r)?,
        })
    }
}

/// 默认逻辑库名(单文件旧库升级后全部记录归入该库)。
pub const DEFAULT_DB: &str = "main";

/// 记录编码版本:1 = 无库名字段;2 = id 之后追加 `db`(所属逻辑库)。
pub const RECORD_VERSION: u32 = 2;

/// 一条完整记忆记录(数据页中实际落盘的内容)。
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryRecord {
    pub id: MemoryId,
    /// 所属逻辑库(单文件多库;旧版记录解码为 "main")。
    pub db: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// 重要度 0.0 ~ 1.0,影响检索排序。
    pub importance: f32,
    /// 记忆原文。
    pub content: String,
    /// 来源标记,例如 "cli" / "server" / "chat:123"。
    pub source: String,
    /// 用户标签。
    pub tags: Vec<String>,
    /// 自动提取(或人工指定)的记忆关键点。
    pub key_points: Vec<String>,
    /// 自动提取的关键词及权重。
    pub keywords: Vec<Keyword>,
}

impl MemoryRecord {
    pub fn new(content: impl Into<String>) -> Self {
        let now = now_millis();
        MemoryRecord {
            id: 0,
            db: DEFAULT_DB.to_string(),
            created_at: now,
            updated_at: now,
            importance: 0.5,
            content: content.into(),
            source: String::new(),
            tags: Vec::new(),
            key_points: Vec::new(),
            keywords: Vec::new(),
        }
    }

    /// 指定所属逻辑库。
    pub fn with_db(mut self, db: impl Into<String>) -> Self {
        self.db = db.into();
        self
    }

    pub fn with_tags(mut self, tags: Vec<String>) -> Self {
        self.tags = tags;
        self
    }

    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = source.into();
        self
    }

    pub fn with_importance(mut self, importance: f32) -> Self {
        self.importance = importance.clamp(0.0, 1.0);
        self
    }
}

impl BinaryEncode for MemoryRecord {
    fn encode(&self, w: &mut Writer) {
        // 记录版本号,便于未来格式演进;v2 起在 id 之后追加所属逻辑库 db。
        w.u32(RECORD_VERSION);
        self.id.encode(w);
        self.db.encode(w);
        self.created_at.encode(w);
        self.updated_at.encode(w);
        self.importance.encode(w);
        self.content.encode(w);
        self.source.encode(w);
        self.tags.encode(w);
        self.key_points.encode(w);
        self.keywords.encode(w);
    }
}

impl BinaryDecode for MemoryRecord {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let version = r.u32()?;
        let id = r.u64()?;
        let db = match version {
            // v1 无库名字段:旧库记录全部归入默认库
            1 => DEFAULT_DB.to_string(),
            2 => String::decode(r)?,
            other => {
                return Err(crate::error::Error::Codec(format!(
                    "unsupported memory record version {other}"
                )))
            }
        };
        Ok(MemoryRecord {
            id,
            db,
            created_at: r.i64()?,
            updated_at: r.i64()?,
            importance: r.f32()?,
            content: String::decode(r)?,
            source: String::decode(r)?,
            tags: Vec::<String>::decode(r)?,
            key_points: Vec::<String>::decode(r)?,
            keywords: Vec::<Keyword>::decode(r)?,
        })
    }
}

/// 记录在文件中的物理位置(引擎层索引使用)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordLocation {
    /// 记录链的首页页码。
    pub head_page: u64,
    /// 记录占用的页数。
    pub page_count: u32,
    /// 记录编码后的字节长度。
    pub encoded_len: u64,
}

impl RecordLocation {
    pub fn new(head_page: u64, page_count: u32, encoded_len: u64) -> Self {
        RecordLocation {
            head_page,
            page_count,
            encoded_len,
        }
    }
}

impl BinaryEncode for RecordLocation {
    fn encode(&self, w: &mut Writer) {
        self.head_page.encode(w);
        self.page_count.encode(w);
        self.encoded_len.encode(w);
    }
}

impl BinaryDecode for RecordLocation {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Ok(RecordLocation {
            head_page: r.u64()?,
            page_count: r.u32()?,
            encoded_len: r.u64()?,
        })
    }
}

/// 当前 Unix 毫秒时间戳;系统时钟异常时回退为 0。
pub fn now_millis() -> Timestamp {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as Timestamp)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{from_slice, to_vec};

    #[test]
    fn record_roundtrip() {
        let mut rec = MemoryRecord::new("Rust 的所有权机制让内存安全无需 GC。");
        rec.id = 42;
        rec.tags = vec!["rust".into(), "编程".into()];
        rec.key_points = vec!["所有权机制".into()];
        rec.keywords = vec![Keyword::new("rust", 0.9), Keyword::new("内存", 0.3)];

        let bytes = to_vec(&rec);
        let back: MemoryRecord = from_slice(&bytes).unwrap();
        assert_eq!(rec, back);
    }

    #[test]
    fn keyword_roundtrip() {
        let k = Keyword::new("数据库", 0.75);
        let back: Keyword = from_slice(&to_vec(&k)).unwrap();
        assert_eq!(k, back);
    }

    #[test]
    fn v1_record_without_db_decodes_as_main() {
        // 手工编码 v1 记录(version=1,无 db 字段),旧库无缝升级
        let mut w = Writer::new();
        w.u32(1); // version
        7u64.encode(&mut w); // id
        1_700_000_000_000i64.encode(&mut w); // created_at
        1_700_000_000_000i64.encode(&mut w); // updated_at
        0.5f32.encode(&mut w); // importance
        "旧版记忆".to_string().encode(&mut w); // content
        "cli".to_string().encode(&mut w); // source
        Vec::<String>::new().encode(&mut w); // tags
        vec!["关键点".to_string()].encode(&mut w); // key_points
        Vec::<Keyword>::new().encode(&mut w); // keywords
        let back: MemoryRecord = from_slice(&w.into_vec()).unwrap();
        assert_eq!(back.id, 7);
        assert_eq!(back.db, DEFAULT_DB, "v1 记录必须归入默认库");
        assert_eq!(back.content, "旧版记忆");
    }

    #[test]
    fn v2_record_carries_db() {
        let rec = MemoryRecord::new("新库记忆").with_db("work");
        let bytes = to_vec(&rec);
        let back: MemoryRecord = from_slice(&bytes).unwrap();
        assert_eq!(back.db, "work");
        assert_eq!(rec, back);
    }
}
