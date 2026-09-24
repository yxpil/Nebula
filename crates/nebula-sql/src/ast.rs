//! SQL AST 定义(MySQL 风格子集,单表 `memories`)。

/// 顶层语句。
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Insert(InsertStmt),
    Select(SelectStmt),
    Delete(DeleteStmt),
    Update(UpdateStmt),
    Checkpoint,
    ShowTables,
    ShowStatus,
}

/// INSERT INTO memories [(列...)] VALUES (值...)
#[derive(Debug, Clone, PartialEq)]
pub struct InsertStmt {
    /// 列名(小写);空表示按默认顺序(content, tags, source, importance)。
    pub columns: Vec<String>,
    /// 与 columns 对齐的值。
    pub values: Vec<Literal>,
}

/// SELECT 输出列。
#[derive(Debug, Clone, PartialEq)]
pub enum SelectColumn {
    Star,
    Field(String),
}

/// SELECT 语句。
#[derive(Debug, Clone, PartialEq)]
pub struct SelectStmt {
    pub columns: Vec<SelectColumn>,
    pub filter: Option<Expr>,
    pub order_by: Option<OrderBy>,
    pub limit: Option<usize>,
}

/// ORDER BY 字段 [ASC|DESC]。
#[derive(Debug, Clone, PartialEq)]
pub struct OrderBy {
    pub column: String,
    pub descending: bool,
}

/// DELETE FROM memories [WHERE ...]
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteStmt {
    pub filter: Option<Expr>,
}

/// UPDATE memories SET 列=值... [WHERE ...]
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateStmt {
    pub assignments: Vec<(String, Literal)>,
    pub filter: Option<Expr>,
}

/// 字面量。
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Str(String),
    Int(i64),
    Float(f32),
}

/// WHERE 表达式(仅支持索引可利用与扫描谓词)。
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// keyword = '词'
    KeywordEq(String),
    /// tag = '标签'
    TagEq(String),
    /// id = 123
    IdEq(u64),
    /// source = 'cli'
    SourceEq(String),
    /// content LIKE '%子串%'
    ContentLike(String),
    /// importance > 0.7 等
    ImportanceCmp { op: CmpOp, value: f32 },
    /// key_points LIKE / source =? 预留扩展位
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
}

/// 比较运算符(importance 用)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Gt,
    Lt,
    Ge,
    Le,
}

impl std::fmt::Display for CmpOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            CmpOp::Gt => ">",
            CmpOp::Lt => "<",
            CmpOp::Ge => ">=",
            CmpOp::Le => "<=",
        })
    }
}

impl Statement {
    /// 语句类型名(服务端返回用)。
    pub fn kind(&self) -> &'static str {
        match self {
            Statement::Insert(_) => "insert",
            Statement::Select(_) => "select",
            Statement::Delete(_) => "delete",
            Statement::Update(_) => "update",
            Statement::Checkpoint => "checkpoint",
            Statement::ShowTables => "show tables",
            Statement::ShowStatus => "show status",
        }
    }
}
