//! 统一错误类型。所有 crate 通过 `Result<T>` 返回本错误。

use std::fmt;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Codec(String),
    Crypto(String),
    Storage(String),
    Engine(String),
    Sql(String),
    Auth(String),
    Protocol(String),
    Config(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::Codec(s) => write!(f, "codec error: {s}"),
            Error::Crypto(s) => write!(f, "crypto error: {s}"),
            Error::Storage(s) => write!(f, "storage error: {s}"),
            Error::Engine(s) => write!(f, "engine error: {s}"),
            Error::Sql(s) => write!(f, "sql error: {s}"),
            Error::Auth(s) => write!(f, "auth error: {s}"),
            Error::Protocol(s) => write!(f, "protocol error: {s}"),
            Error::Config(s) => write!(f, "config error: {s}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl Error {
    /// 无法打开数据库文件时的统一提示(多数情况是密码错误或文件损坏)。
    pub fn wrong_password() -> Self {
        Error::Auth("incorrect password or corrupted database file".into())
    }
}
