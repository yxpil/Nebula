//! Nebula 记忆数据库 - 共享内核
//!
//! 本 crate 不依赖任何第三方库,提供:
//! - [`error`] 统一错误类型
//! - [`codec`] 手写二进制编解码(页载荷 / 记录 / 索引快照的落盘格式)
//! - [`types`] 领域类型(记忆记录、关键词、物理位置)

pub mod codec;
pub mod error;
pub mod logger;
pub mod types;

pub use error::{Error, Result};
pub use logger::{init as init_logger, LogLevel};
pub use types::{
    Keyword, MemoryId, MemoryRecord, RecordLocation, Timestamp, DEFAULT_DB, FORMAT_VERSION,
};

/// 记录 ERROR 级别日志(用法同 `format!`)。
#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::logger::log($crate::logger::LogLevel::Error, module_path!(), &format!($($arg)*))
    };
}

/// 记录 WARN 级别日志。
#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::logger::log($crate::logger::LogLevel::Warn, module_path!(), &format!($($arg)*))
    };
}

/// 记录 INFO 级别日志。
#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::logger::log($crate::logger::LogLevel::Info, module_path!(), &format!($($arg)*))
    };
}

/// 记录 DEBUG 级别日志。
#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => {
        $crate::logger::log($crate::logger::LogLevel::Debug, module_path!(), &format!($($arg)*))
    };
}
