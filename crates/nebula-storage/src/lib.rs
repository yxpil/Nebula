//! Nebula 页式加密存储引擎。
//!
//! # 文件格式总览
//!
//! ```text
//! ┌─────────┬──────────────────────────────────────────────┐
//! │ page 0  │ 文件头页:明文 magic/version/page_size/salt    │
//! │         │ + 密文 verifier/page_count(认证解密)           │
//! ├─────────┼──────────────────────────────────────────────┤
//! │ page 1  │ 目录页:next_record_id/free 链/快照根/计数      │
//! ├─────────┼──────────────────────────────────────────────┤
//! │ page 2+ │ 数据页(记录链)/ 快照页(索引链)/ 空闲页(链)      │
//! └─────────┴──────────────────────────────────────────────┘
//! ```
//! 所有非头页整页 ChaCha20-Poly1305 加密,随机 nonce 前置,
//! AAD = 页码 || 页角色,防页搬移与篡改。

pub mod catalog;
pub mod memory_file;
pub mod page;
pub mod pager;
pub mod records;
pub mod snapshot;

pub use memory_file::{MemoryFile, OpenInfo};
pub use pager::{DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE, MIN_PAGE_SIZE};
