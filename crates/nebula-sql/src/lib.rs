//! Nebula SQL 解析器(MySQL 风格子集)。
//!
//! 管道:SQL 文本 → [`lexer`] token 流 → [`parser`] → [`ast`]。
//! 引擎层只依赖 AST,不涉及文本。

pub mod ast;
pub mod lexer;
pub mod parser;

pub use ast::{CmpOp, DeleteStmt, Expr, InsertStmt, Literal, OrderBy, SelectColumn, SelectStmt, Statement, UpdateStmt};
pub use parser::Parser;

/// 解析单条 SQL 语句(允许结尾分号)。
pub fn parse(sql: &str) -> nebula_core::Result<Statement> {
    Parser::parse(sql)
}

/// 解析分号分隔的语句序列(空语句自动跳过;至少一条)。
pub fn parse_script(sql: &str) -> nebula_core::Result<Vec<Statement>> {
    Parser::parse_script(sql)
}
