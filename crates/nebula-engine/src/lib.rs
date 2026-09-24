//! Nebula 数据库引擎:存储 + 索引 + SQL 执行 + 权限的组合。

pub mod auth;
pub mod backend;
pub mod cache;
pub mod config;
pub mod database;
pub mod executor;
pub mod format;
pub mod index;
pub mod search;

pub use auth::{FullAccess, Privilege, Session, UserDirectory};
pub use backend::{MemBackend, RankedRow, SessionBackend};
pub use cache::{DocCache, QueryCache};
pub use config::{CacheConfig, EngineConfig, SearchConfig};
pub use database::normalize_db_name;
pub use database::Database;
pub use executor::QueryResult;
