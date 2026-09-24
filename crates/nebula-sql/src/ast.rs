//! SQL AST 定义(MySQL 风格子集,单表 `memories`)。

use nebula_core::MemoryId;

/// 顶层语句。
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Insert(InsertStmt),
    Select(SelectStmt),
    Delete(DeleteStmt),
    Update(UpdateStmt),
    /// SEARCH '自然语言查询' [LIMIT n] —— BM25 相关性检索。
    Search(SearchStmt),
    /// RELATED TO <id> [LIMIT n] / RELATED '文本' [LIMIT n] —— 联想推荐。
    Related(RelatedStmt),
    /// SHOW CACHE —— 查询缓存 / 文档缓存的容量与命中统计。
    ShowCache,
    /// CLEAR CACHE —— 清空查询缓存并清零计数。
    ClearCache,
    /// SHOW HOT [LIMIT n] —— 按读取热度展示记忆。
    ShowHot(ShowHotStmt),
    /// SET CACHE query|doc <n> —— 在线调整缓存容量。
    SetCache(SetCacheStmt),
    Checkpoint,
    ShowTables,
    ShowStatus,
}

/// SEARCH 语句:自然语言查询 + 可选条数上限。
#[derive(Debug, Clone, PartialEq)]
pub struct SearchStmt {
    /// 查询文本(分词后进入 BM25 打分)。
    pub query: String,
    /// 返回条数上限(None 时用配置默认值)。
    pub limit: Option<usize>,
}

/// RELATED 语句的种子来源。
#[derive(Debug, Clone, PartialEq)]
pub enum RelatedSeed {
    /// 以某条已有记忆为种子。
    Id(MemoryId),
    /// 以自由文本/关键词为种子。
    Text(String),
}

/// RELATED 语句:联想推荐 + 可选条数上限。
#[derive(Debug, Clone, PartialEq)]
pub struct RelatedStmt {
    pub seed: RelatedSeed,
    /// 返回条数上限(None 时用配置默认值)。
    pub limit: Option<usize>,
}

/// SHOW HOT 语句:按读取热度展示记忆 + 可选条数上限。
#[derive(Debug, Clone, PartialEq)]
pub struct ShowHotStmt {
    /// 展示条数上限(None 时用检索默认条数)。
    pub limit: Option<usize>,
}

/// SET CACHE 的调整目标。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheTarget {
    /// 查询缓存(SEARCH / RELATED 排序结果)。
    Query,
    /// 文档缓存(记忆记录 LRU)。
    Doc,
}

/// SET CACHE 语句:在线调整某一层缓存的容量。
#[derive(Debug, Clone, PartialEq)]
pub struct SetCacheStmt {
    pub target: CacheTarget,
    /// 新容量(0 = 关闭对应缓存)。
    pub capacity: usize,
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
            Statement::Search(_) => "search",
            Statement::Related(_) => "related",
            Statement::ShowCache => "show-cache",
            Statement::ClearCache => "clear-cache",
            Statement::ShowHot(_) => "show-hot",
            Statement::SetCache(_) => "set-cache",
            Statement::Checkpoint => "checkpoint",
            Statement::ShowTables => "show tables",
            Statement::ShowStatus => "show status",
        }
    }
}
