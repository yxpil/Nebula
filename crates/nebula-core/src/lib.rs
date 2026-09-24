//! Nebula 记忆数据库 - 共享内核
//!
//! 本 crate 不依赖任何第三方库,提供:
//! - [`error`] 统一错误类型
//! - [`codec`] 手写二进制编解码(页载荷 / 记录 / 索引快照的落盘格式)
//! - [`types`] 领域类型(记忆记录、关键词、物理位置)

pub mod codec;
pub mod error;
pub mod types;

pub use error::{Error, Result};
pub use types::{
    Keyword, MemoryId, MemoryRecord, RecordLocation, Timestamp, DEFAULT_PAGE_SIZE, FORMAT_VERSION,
    MAX_CONTENT_LEN, MAX_KEY_POINTS, MAX_KEYWORDS,
};
