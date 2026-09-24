//! SQL AST 定义(MySQL 风格子集,单表 `memories`)。

use nebula_core::MemoryId;

/// 顶层语句。
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Insert(InsertStmt),
    Select(SelectStmt),
    Delete(DeleteStmt),
    Update(UpdateStmt),
    /// SEARCH '自然语言查询' [IN db,...] [LIMIT n] —— BM25 相关性检索。
    Search(SearchStmt),
    /// RELATED TO <id> | RELATED '文本' [IN db,...] [LIMIT n] —— 联想推荐。
    Related(RelatedStmt),
    /// CREATE DATABASE [IF NOT EXISTS] name —— 新建逻辑库。
    CreateDatabase(CreateDatabaseStmt),
    /// DROP DATABASE [IF EXISTS] name —— 删除逻辑库。
    DropDatabase(DropDatabaseStmt),
    /// USE name —— 切换当前工作库。
    Use(UseStmt),
    /// ATTACH FILE 'path' AS name —— 挂接目录中的另一个 .ndb 文件。
    Attach(AttachStmt),
    /// DETACH name —— 取消挂接。
    Detach(DetachStmt),
    /// CREATE USER [IF NOT EXISTS] name IDENTIFIED BY 'password'。
    CreateUser(CreateUserStmt),
    /// DROP USER [IF EXISTS] name。
    DropUser(DropUserStmt),
    /// ALTER USER name IDENTIFIED BY 'newpassword'。
    AlterUser(AlterUserStmt),
    /// GRANT 权限 ON 库 TO 用户。
    Grant(GrantStmt),
    /// REVOKE 权限 ON 库 FROM 用户。
    Revoke(RevokeStmt),
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
    /// SHOW DATABASES —— 全部逻辑库。
    ShowDatabases,
    /// SHOW USERS —— 全部应用层用户。
    ShowUsers,
    /// SHOW GRANTS [FOR user] —— 用户授权。
    ShowGrants(ShowGrantsStmt),
    /// BEGIN [WORK] / START TRANSACTION —— 开始事务。
    Begin,
    /// COMMIT [WORK] —— 提交事务。
    Commit,
    /// ROLLBACK [WORK] —— 回滚事务。
    Rollback,
}

/// SEARCH 语句:自然语言查询 + 显式库列表 + 可选条数上限。
#[derive(Debug, Clone, PartialEq)]
pub struct SearchStmt {
    /// 查询文本(分词后进入 BM25 打分)。
    pub query: String,
    /// 显式声明的检索库列表;空列表 = 仅检索当前库(跨库必须显式声明)。
    pub dbs: Vec<String>,
    /// 返回条数上限(None 时用配置默认值)。
    pub limit: Option<usize>,
}

/// RELATED 语句的种子来源。
#[derive(Debug, Clone, PartialEq)]
pub enum RelatedSeed {
    /// 以当前库的某条已有记忆为种子。
    Id(MemoryId),
    /// 以指定库的某条记忆为种子(`RELATED TO db.id`)。
    QualifiedId(String, MemoryId),
    /// 以自由文本/关键词为种子。
    Text(String),
}

/// RELATED 语句:联想推荐 + 显式库列表 + 可选条数上限。
#[derive(Debug, Clone, PartialEq)]
pub struct RelatedStmt {
    pub seed: RelatedSeed,
    /// 显式声明的库列表;空列表 = 仅当前库(QualifiedId 时为其所属库)。
    pub dbs: Vec<String>,
    /// 返回条数上限(None 时用配置默认值)。
    pub limit: Option<usize>,
}

/// CREATE DATABASE 语句。
#[derive(Debug, Clone, PartialEq)]
pub struct CreateDatabaseStmt {
    pub name: String,
    pub if_not_exists: bool,
}

/// DROP DATABASE 语句。
#[derive(Debug, Clone, PartialEq)]
pub struct DropDatabaseStmt {
    pub name: String,
    pub if_exists: bool,
}

/// USE 语句:切换当前工作库。
#[derive(Debug, Clone, PartialEq)]
pub struct UseStmt {
    pub name: String,
}

/// ATTACH FILE 'path' AS name:把另一个 .ndb 文件挂进当前目录会话。
#[derive(Debug, Clone, PartialEq)]
pub struct AttachStmt {
    /// 被挂接的 .ndb 文件路径。
    pub path: String,
    /// 挂接后使用的逻辑库名。
    pub name: String,
}

/// DETACH name:取消挂接。
#[derive(Debug, Clone, PartialEq)]
pub struct DetachStmt {
    pub name: String,
}

/// CREATE USER 语句。
#[derive(Debug, Clone, PartialEq)]
pub struct CreateUserStmt {
    pub name: String,
    pub password: String,
    pub if_not_exists: bool,
}

/// DROP USER 语句。
#[derive(Debug, Clone, PartialEq)]
pub struct DropUserStmt {
    pub name: String,
    pub if_exists: bool,
}

/// ALTER USER 语句:修改密码。
#[derive(Debug, Clone, PartialEq)]
pub struct AlterUserStmt {
    pub name: String,
    pub password: String,
}

/// 按库授予的权限类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Privilege {
    /// 读:SELECT / SEARCH / RELATED / SHOW。
    Read,
    /// 写:INSERT / UPDATE / DELETE。
    Write,
    /// 管理:CREATE/DROP DATABASE、用户与授权管理。
    Admin,
}

/// 授权对象:某个逻辑库,或全部库。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantObject {
    /// 单个逻辑库。
    Db(String),
    /// 全部库(`ON *`)。
    AllDatabases,
}

/// GRANT 语句。
#[derive(Debug, Clone, PartialEq)]
pub struct GrantStmt {
    pub privileges: Vec<Privilege>,
    pub object: GrantObject,
    pub user: String,
}

/// REVOKE 语句。
#[derive(Debug, Clone, PartialEq)]
pub struct RevokeStmt {
    pub privileges: Vec<Privilege>,
    pub object: GrantObject,
    pub user: String,
}

/// SHOW GRANTS 语句(不带 FOR = 当前用户)。
#[derive(Debug, Clone, PartialEq)]
pub struct ShowGrantsStmt {
    pub user: Option<String>,
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
            Statement::CreateDatabase(_) => "create-database",
            Statement::DropDatabase(_) => "drop-database",
            Statement::Use(_) => "use",
            Statement::Attach(_) => "attach",
            Statement::Detach(_) => "detach",
            Statement::CreateUser(_) => "create-user",
            Statement::DropUser(_) => "drop-user",
            Statement::AlterUser(_) => "alter-user",
            Statement::Grant(_) => "grant",
            Statement::Revoke(_) => "revoke",
            Statement::ShowCache => "show-cache",
            Statement::ClearCache => "clear-cache",
            Statement::ShowHot(_) => "show-hot",
            Statement::SetCache(_) => "set-cache",
            Statement::Checkpoint => "checkpoint",
            Statement::ShowTables => "show tables",
            Statement::ShowStatus => "show status",
            Statement::ShowDatabases => "show-databases",
            Statement::ShowUsers => "show-users",
            Statement::ShowGrants(_) => "show-grants",
            Statement::Begin => "begin",
            Statement::Commit => "commit",
            Statement::Rollback => "rollback",
        }
    }
}
