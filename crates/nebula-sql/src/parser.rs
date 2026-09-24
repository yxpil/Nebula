//! 递归下降语法分析,支持:
//! ```sql
//! INSERT INTO memories [(content,tags,source,importance)] VALUES (...)
//! SELECT [*|列,...] FROM memories [WHERE 条件] [ORDER BY 列 [ASC|DESC]] [LIMIT n]
//! SEARCH '自然语言查询' [IN db,...] [LIMIT n]
//! RELATED TO <id|db.id> | RELATED '文本' [IN db,...] [LIMIT n]
//! UPDATE memories SET 列=值[,...] [WHERE ...]
//! DELETE FROM memories [WHERE ...]
//! CREATE DATABASE [IF NOT EXISTS] db | DROP DATABASE [IF EXISTS] db | USE db
//! ATTACH FILE 'path' AS db | DETACH db
//! CREATE USER [IF NOT EXISTS] user IDENTIFIED BY 'pw'
//! DROP USER [IF EXISTS] user | ALTER USER user IDENTIFIED BY 'pw'
//! GRANT READ|WRITE|ADMIN|ALL [PRIVILEGES] ON db|* TO user
//! REVOKE ... ON db|* FROM user
//! SHOW TABLES | STATUS | CACHE | HOT [LIMIT n] | DATABASES | USERS | GRANTS [FOR user]
//! CLEAR CACHE
//! SET CACHE query|doc <n>
//! CHECKPOINT
//! ```
//! WHERE 条件:`id = n` / `keyword = '词'` / `tag = '标签'` /
//! `content LIKE '%子串%'` / `importance > 0.5`,支持 AND / OR / NOT 与括号。
//! SEARCH / RELATED 是 AI 记忆检索语句:分词后走 BM25 打分与共现图联想,
//! 跨库检索必须用 IN 显式声明库列表(或 db.id 限定种子),否则只搜当前库。

use nebula_core::{Error, Result};

use crate::ast::{
    AlterUserStmt, AttachStmt, CacheTarget, CmpOp, CreateDatabaseStmt, CreateUserStmt,
    DeleteStmt, DetachStmt, DropDatabaseStmt, DropUserStmt, Expr, GrantObject, GrantStmt,
    InsertStmt, Literal, OrderBy, Privilege, RelatedSeed, RelatedStmt, RevokeStmt, SearchStmt,
    SelectColumn, SelectStmt, SetCacheStmt, ShowGrantsStmt, ShowHotStmt, Statement, UpdateStmt,
};
use crate::lexer::Token;

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    pub fn parse(sql: &str) -> Result<Statement> {
        let tokens = crate::lexer::Lexer::new(sql).tokenize()?;
        let mut p = Parser { tokens, pos: 0 };
        let stmt = p.statement()?;
        // 允许结尾分号
        if p.peek() == &Token::Semi {
            p.pos += 1;
        }
        if p.peek() != &Token::Eof {
            return Err(Error::Sql(format!(
                "unexpected trailing tokens near position {}",
                p.pos
            )));
        }
        Ok(stmt)
    }

    /// 解析分号分隔的语句序列(空语句/连续分号自动跳过)。
    pub fn parse_script(sql: &str) -> Result<Vec<Statement>> {
        let tokens = crate::lexer::Lexer::new(sql).tokenize()?;
        let mut p = Parser { tokens, pos: 0 };
        let mut out = Vec::new();
        loop {
            while p.peek() == &Token::Semi {
                p.pos += 1;
            }
            if p.peek() == &Token::Eof {
                break;
            }
            out.push(p.statement()?);
            // 语句后至少一个分号或直接结尾;其它 token 视为语法错误
            if p.peek() == &Token::Eof {
                break;
            }
            if p.peek() != &Token::Semi {
                return Err(Error::Sql(format!(
                    "unexpected trailing tokens near position {}",
                    p.pos
                )));
            }
        }
        if out.is_empty() {
            return Err(Error::Sql("empty script".into()));
        }
        Ok(out)
    }

    fn peek(&self) -> &Token {
        self.tokens.get(self.pos).unwrap_or(&Token::Eof)
    }

    fn next(&mut self) -> Token {
        let t = self.tokens.get(self.pos).cloned().unwrap_or(Token::Eof);
        self.pos += 1;
        t
    }

    fn expect_word(&mut self, w: &str) -> Result<()> {
        match self.next() {
            Token::Word(x) if x == w => Ok(()),
            other => Err(Error::Sql(format!(
                "expected keyword '{w}', got {other:?}"
            ))),
        }
    }

    fn eat_word(&mut self, w: &str) -> bool {
        if let Token::Word(x) = self.peek() {
            if x == w {
                self.pos += 1;
                return true;
            }
        }
        false
    }

    fn eat(&mut self, t: &Token) -> bool {
        if self.peek() == t {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    // ------- 语句分发 -------

    fn statement(&mut self) -> Result<Statement> {
        let word = match self.peek() {
            Token::Word(w) => w.clone(),
            other => {
                return Err(Error::Sql(format!(
                    "expected statement keyword, got {other:?}"
                )))
            }
        };
        match word.as_str() {
            "insert" => self.insert_stmt(),
            "select" => self.select_stmt(),
            "search" => self.search_stmt(),
            "related" => self.related_stmt(),
            "delete" => self.delete_stmt(),
            "update" => self.update_stmt(),
            "create" => self.create_stmt(),
            "drop" => self.drop_stmt(),
            "use" => self.use_stmt(),
            "attach" => self.attach_stmt(),
            "detach" => self.detach_stmt(),
            "grant" => self.grant_stmt(),
            "revoke" => self.revoke_stmt(),
            "alter" => {
                self.expect_word("alter")?;
                self.expect_word("user")?;
                self.alter_user_stmt()
            }
            "clear" => self.clear_stmt(),
            "set" => self.set_stmt(),
            "checkpoint" => {
                self.next();
                Ok(Statement::Checkpoint)
            }
            "show" => self.show_stmt(),
            other => Err(Error::Sql(format!("unsupported statement '{other}'"))),
        }
    }

    // ------- SEARCH / RELATED(AI 检索)-------

    /// SEARCH '查询文本' [IN db,...] [LIMIT n]
    fn search_stmt(&mut self) -> Result<Statement> {
        self.expect_word("search")?;
        let query = match self.next() {
            Token::Str(s) if !s.trim().is_empty() => s,
            other => {
                return Err(Error::Sql(format!(
                    "SEARCH expects a quoted query string, got {other:?}"
                )))
            }
        };
        let dbs = self.optional_db_list()?;
        Ok(Statement::Search(SearchStmt {
            query,
            dbs,
            limit: self.optional_limit()?,
        }))
    }

    /// RELATED TO <id|db.id> | RELATED '文本' [IN db,...] [LIMIT n]
    fn related_stmt(&mut self) -> Result<Statement> {
        self.expect_word("related")?;
        let seed = if self.eat_word("to") {
            self.parse_related_seed_id()?
        } else {
            match self.next() {
                Token::Str(s) if !s.trim().is_empty() => RelatedSeed::Text(s),
                other => {
                    return Err(Error::Sql(format!(
                        "RELATED expects TO <id> or a quoted seed text, got {other:?}"
                    )))
                }
            }
        };
        let dbs = self.optional_db_list()?;
        Ok(Statement::Related(RelatedStmt {
            seed,
            dbs,
            limit: self.optional_limit()?,
        }))
    }

    /// 解析 RELATED TO 的 id 种子:支持 `db.id` 限定名(带库)与裸 id(当前库)。
    fn parse_related_seed_id(&mut self) -> Result<RelatedSeed> {
        let save = self.pos;
        // db.id:Word → Dot → Int
        if let Token::Word(db) = self.next() {
            if self.eat(&Token::Dot) {
                return match self.next() {
                    Token::Int(n) if n >= 0 => {
                        Ok(RelatedSeed::QualifiedId(db, n as u64))
                    }
                    other => Err(Error::Sql(format!(
                        "RELATED TO '{db}.' expects a non-negative id, got {other:?}"
                    ))),
                };
            }
        }
        // 不是限定名:回退,按裸 id 解析
        self.pos = save;
        match self.next() {
            Token::Int(n) if n >= 0 => Ok(RelatedSeed::Id(n as u64)),
            other => Err(Error::Sql(format!(
                "RELATED TO expects a non-negative memory id, got {other:?}"
            ))),
        }
    }

    /// 可选的 IN db1, db2 显式库列表(跨库检索必须显式声明)。
    fn optional_db_list(&mut self) -> Result<Vec<String>> {
        if !self.eat_word("in") {
            return Ok(Vec::new());
        }
        let mut dbs = Vec::new();
        loop {
            match self.next() {
                Token::Word(w) => dbs.push(w),
                other => {
                    return Err(Error::Sql(format!(
                        "IN expects a database name, got {other:?}"
                    )))
                }
            }
            if self.eat(&Token::Comma) {
                continue;
            }
            break;
        }
        if dbs.is_empty() {
            return Err(Error::Sql("IN requires at least one database".into()));
        }
        Ok(dbs)
    }

    // ------- 逻辑库 DDL -------

    /// CREATE DATABASE [IF NOT EXISTS] name | CREATE USER ...
    fn create_stmt(&mut self) -> Result<Statement> {
        self.expect_word("create")?;
        if self.eat_word("database") {
            let if_not_exists = self.eat_if_not_exists()?;
            let name = self.parse_ident("database name")?;
            Ok(Statement::CreateDatabase(CreateDatabaseStmt {
                name,
                if_not_exists,
            }))
        } else if self.eat_word("user") {
            let if_not_exists = self.eat_if_not_exists()?;
            let name = self.parse_ident("user name")?;
            let password = self.parse_identified_by()?;
            Ok(Statement::CreateUser(CreateUserStmt {
                name,
                password,
                if_not_exists,
            }))
        } else {
            Err(Error::Sql(
                "CREATE supports DATABASE or USER in this subset".into(),
            ))
        }
    }

    /// DROP DATABASE [IF EXISTS] name | DROP USER [IF EXISTS] name
    fn drop_stmt(&mut self) -> Result<Statement> {
        self.expect_word("drop")?;
        if self.eat_word("database") {
            let if_exists = self.eat_if_exists()?;
            let name = self.parse_ident("database name")?;
            Ok(Statement::DropDatabase(DropDatabaseStmt { name, if_exists }))
        } else if self.eat_word("user") {
            let if_exists = self.eat_if_exists()?;
            let name = self.parse_ident("user name")?;
            Ok(Statement::DropUser(DropUserStmt { name, if_exists }))
        } else {
            Err(Error::Sql("DROP supports DATABASE or USER in this subset".into()))
        }
    }

    /// USE name:切换当前工作库。
    fn use_stmt(&mut self) -> Result<Statement> {
        self.expect_word("use")?;
        let name = self.parse_ident("database name")?;
        Ok(Statement::Use(crate::ast::UseStmt { name }))
    }

    /// ATTACH FILE 'path' AS name
    fn attach_stmt(&mut self) -> Result<Statement> {
        self.expect_word("attach")?;
        self.expect_word("file")?;
        let path = match self.next() {
            Token::Str(s) if !s.trim().is_empty() => s,
            other => {
                return Err(Error::Sql(format!(
                    "ATTACH FILE expects a quoted file path, got {other:?}"
                )))
            }
        };
        self.expect_word("as")?;
        let name = self.parse_ident("database name")?;
        Ok(Statement::Attach(AttachStmt { path, name }))
    }

    /// DETACH name
    fn detach_stmt(&mut self) -> Result<Statement> {
        self.expect_word("detach")?;
        let name = self.parse_ident("database name")?;
        Ok(Statement::Detach(DetachStmt { name }))
    }

    /// ALTER USER name IDENTIFIED BY 'newpassword'
    fn alter_user_stmt(&mut self) -> Result<Statement> {
        let name = self.parse_ident("user name")?;
        let password = self.parse_identified_by()?;
        Ok(Statement::AlterUser(AlterUserStmt { name, password }))
    }

    /// 解析 `IDENTIFIED BY 'password'` 子句并返回密码。
    fn parse_identified_by(&mut self) -> Result<String> {
        self.expect_word("identified")?;
        self.expect_word("by")?;
        match self.next() {
            Token::Str(s) if !s.is_empty() => Ok(s),
            Token::Str(_) => Err(Error::Sql("password must not be empty".into())),
            other => Err(Error::Sql(format!(
                "IDENTIFIED BY expects a quoted password, got {other:?}"
            ))),
        }
    }

    fn eat_if_not_exists(&mut self) -> Result<bool> {
        if self.eat_word("if") {
            self.expect_word("not")?;
            self.expect_word("exists")?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn eat_if_exists(&mut self) -> Result<bool> {
        if self.eat_word("if") {
            self.expect_word("exists")?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// 读取一个标识符(库名/用户名;裸词,小写形式已由 lexer 归一化)。
    fn parse_ident(&mut self, what: &str) -> Result<String> {
        match self.next() {
            Token::Word(w) => Ok(w),
            other => Err(Error::Sql(format!("expected {what}, got {other:?}"))),
        }
    }

    // ------- 授权 GRANT / REVOKE -------

    /// GRANT priv,... ON object TO user
    fn grant_stmt(&mut self) -> Result<Statement> {
        self.expect_word("grant")?;
        let privileges = self.parse_privilege_list()?;
        let object = self.parse_grant_object()?;
        self.expect_word("to")?;
        let user = self.parse_ident("user name")?;
        Ok(Statement::Grant(GrantStmt {
            privileges,
            object,
            user,
        }))
    }

    /// REVOKE priv,... ON object FROM user
    fn revoke_stmt(&mut self) -> Result<Statement> {
        self.expect_word("revoke")?;
        let privileges = self.parse_privilege_list()?;
        let object = self.parse_grant_object()?;
        self.expect_word("from")?;
        let user = self.parse_ident("user name")?;
        Ok(Statement::Revoke(RevokeStmt {
            privileges,
            object,
            user,
        }))
    }

    /// 权限列表:READ / WRITE / ADMIN / ALL [PRIVILEGES](ALL 展开为全部三种)。
    fn parse_privilege_list(&mut self) -> Result<Vec<Privilege>> {
        let mut privs = Vec::new();
        loop {
            match self.next() {
                Token::Word(w) if w == "read" => privs.push(Privilege::Read),
                Token::Word(w) if w == "write" => privs.push(Privilege::Write),
                Token::Word(w) if w == "admin" => privs.push(Privilege::Admin),
                Token::Word(w) if w == "all" => {
                    self.eat_word("privileges");
                    privs.extend([Privilege::Read, Privilege::Write, Privilege::Admin]);
                }
                other => {
                    return Err(Error::Sql(format!(
                        "expected privilege READ/WRITE/ADMIN/ALL, got {other:?}"
                    )))
                }
            }
            if self.eat(&Token::Comma) {
                continue;
            }
            break;
        }
        if privs.is_empty() {
            return Err(Error::Sql("GRANT requires at least one privilege".into()));
        }
        Ok(privs)
    }

    /// 授权对象:ON <db 名> 或 ON *(全部库)。
    fn parse_grant_object(&mut self) -> Result<GrantObject> {
        self.expect_word("on")?;
        if self.eat(&Token::Star) {
            return Ok(GrantObject::AllDatabases);
        }
        let name = self.parse_ident("database name after ON")?;
        Ok(GrantObject::Db(name))
    }

    // ------- SHOW / CLEAR / SET -------

    fn show_stmt(&mut self) -> Result<Statement> {
        self.expect_word("show")?;
        if self.eat_word("tables") {
            Ok(Statement::ShowTables)
        } else if self.eat_word("status") {
            Ok(Statement::ShowStatus)
        } else if self.eat_word("cache") {
            Ok(Statement::ShowCache)
        } else if self.eat_word("hot") {
            Ok(Statement::ShowHot(ShowHotStmt {
                limit: self.optional_limit()?,
            }))
        } else if self.eat_word("databases") {
            Ok(Statement::ShowDatabases)
        } else if self.eat_word("users") {
            Ok(Statement::ShowUsers)
        } else if self.eat_word("grants") {
            let user = if self.eat_word("for") {
                Some(self.parse_ident("user name")?)
            } else {
                None
            };
            Ok(Statement::ShowGrants(ShowGrantsStmt { user }))
        } else {
            Err(Error::Sql(
                "SHOW supports TABLES, STATUS, CACHE, HOT, DATABASES, USERS or GRANTS".into(),
            ))
        }
    }

    /// CLEAR CACHE:清空查询缓存。
    fn clear_stmt(&mut self) -> Result<Statement> {
        self.expect_word("clear")?;
        self.expect_word("cache")?;
        Ok(Statement::ClearCache)
    }

    /// SET CACHE query|doc <n>:在线调整缓存容量。
    fn set_stmt(&mut self) -> Result<Statement> {
        self.expect_word("set")?;
        if self.eat_word("password") {
            // SET PASSWORD ... 不支持,密码统一用 ALTER USER 修改。
            return Err(Error::Sql(
                "use ALTER USER <name> IDENTIFIED BY '<password>' to change passwords".into(),
            ));
        }
        self.expect_word("cache")?;
        let target = match self.next() {
            Token::Word(w) if w == "query" => CacheTarget::Query,
            Token::Word(w) if w == "doc" => CacheTarget::Doc,
            other => {
                return Err(Error::Sql(format!(
                    "SET CACHE expects target 'query' or 'doc', got {other:?}"
                )))
            }
        };
        let capacity = match self.next() {
            Token::Int(n) if n >= 0 => n as usize,
            other => {
                return Err(Error::Sql(format!(
                    "SET CACHE capacity expects a non-negative int, got {other:?}"
                )))
            }
        };
        Ok(Statement::SetCache(SetCacheStmt { target, capacity }))
    }

    // ------- INSERT -------

    fn insert_stmt(&mut self) -> Result<Statement> {
        self.expect_word("insert")?;
        self.expect_word("into")?;
        self.expect_word("memories")?;

        let mut columns = Vec::new();
        if self.eat(&Token::LParen) {
            loop {
                match self.next() {
                    Token::Word(col) => columns.push(col),
                    other => return Err(Error::Sql(format!("bad column name {other:?}"))),
                }
                if self.eat(&Token::Comma) {
                    continue;
                }
                if self.eat(&Token::RParen) {
                    break;
                }
                return Err(Error::Sql("expected ',' or ')' in column list".into()));
            }
        }
        self.expect_word("values")?;
        let values = self.value_list()?;
        if !columns.is_empty() && columns.len() != values.len() {
            return Err(Error::Sql(format!(
                "column count ({}) does not match value count ({})",
                columns.len(),
                values.len()
            )));
        }
        Ok(Statement::Insert(InsertStmt { columns, values }))
    }

    fn value_list(&mut self) -> Result<Vec<Literal>> {
        self.eat(&Token::LParen);
        let mut out = Vec::new();
        loop {
            out.push(self.literal()?);
            if self.eat(&Token::Comma) {
                continue;
            }
            if self.eat(&Token::RParen) {
                break;
            }
            return Err(Error::Sql("expected ',' or ')' in VALUES".into()));
        }
        Ok(out)
    }

    fn literal(&mut self) -> Result<Literal> {
        match self.next() {
            Token::Str(s) => Ok(Literal::Str(s)),
            Token::Int(i) => Ok(Literal::Int(i)),
            Token::Float(f) => Ok(Literal::Float(f)),
            other => Err(Error::Sql(format!("expected literal, got {other:?}"))),
        }
    }

    // ------- SELECT -------

    fn select_stmt(&mut self) -> Result<Statement> {
        self.expect_word("select")?;
        let mut columns = Vec::new();
        if self.eat(&Token::Star) {
            columns.push(SelectColumn::Star);
        } else {
            loop {
                match self.next() {
                    Token::Word(col) => columns.push(SelectColumn::Field(col)),
                    other => return Err(Error::Sql(format!("bad select column {other:?}"))),
                }
                if self.eat(&Token::Comma) {
                    continue;
                }
                break;
            }
        }
        self.expect_word("from")?;
        self.expect_word("memories")?;

        let filter = self.optional_where()?;
        let order_by = self.optional_order_by()?;
        let limit = self.optional_limit()?;
        Ok(Statement::Select(SelectStmt {
            columns,
            filter,
            order_by,
            limit,
        }))
    }

    fn optional_where(&mut self) -> Result<Option<Expr>> {
        if !self.eat_word("where") {
            return Ok(None);
        }
        Ok(Some(self.expr()?))
    }

    /// OR 是最低优先级。
    fn expr(&mut self) -> Result<Expr> {
        let mut left = self.and_expr()?;
        while self.eat_word("or") {
            let right = self.and_expr()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn and_expr(&mut self) -> Result<Expr> {
        let mut left = self.not_expr()?;
        while self.eat_word("and") {
            let right = self.not_expr()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn not_expr(&mut self) -> Result<Expr> {
        if self.eat_word("not") {
            let inner = self.primary_expr()?;
            return Ok(Expr::Not(Box::new(inner)));
        }
        self.primary_expr()
    }

    fn primary_expr(&mut self) -> Result<Expr> {
        if self.eat(&Token::LParen) {
            let inner = self.expr()?;
            if !self.eat(&Token::RParen) {
                return Err(Error::Sql("expected ')'".into()));
            }
            return Ok(inner);
        }
        self.comparison()
    }

    fn comparison(&mut self) -> Result<Expr> {
        let col = match self.next() {
            Token::Word(w) => w,
            other => return Err(Error::Sql(format!("expected column name, got {other:?}"))),
        };
        match col.as_str() {
            "keyword" | "tag" | "id" | "content" | "importance" | "key_point" | "source"
            | "created_at" | "updated_at" => {}
            other => {
                return Err(Error::Sql(format!(
                    "unknown column '{other}' (expected keyword/tag/id/content/importance/...)"
                )))
            }
        }

        if self.eat_word("like") {
            let pattern = match self.next() {
                Token::Str(s) => s,
                other => return Err(Error::Sql(format!("LIKE expects string, got {other:?}"))),
            };
            return match col.as_str() {
                "content" => Ok(Expr::ContentLike(like_pattern_to_substring(&pattern)?)),
                other => Err(Error::Sql(format!(
                    "LIKE is only supported on content, not '{other}'"
                ))),
            };
        }

        if self.eat_word("is") {
            self.eat_word("not");
            self.expect_word("null")?;
            return Err(Error::Sql("IS NULL is not supported in this subset".into()));
        }

        // 比较运算符(仅 importance)
        if let Some(op) = self.cmp_op() {
            match col.as_str() {
                "importance" => {
                    let value = self.float_value()?;
                    return Ok(Expr::ImportanceCmp { op, value });
                }
                _ => {
                    return Err(Error::Sql(format!(
                        "column '{col}' does not support comparison operators (only 'importance > < >= <=' does)"
                    )))
                }
            }
        }

        // 必须命中 =
        if !self.eat(&Token::Eq) {
            return Err(Error::Sql(format!(
                "expected '=' after column '{col}'"
            )));
        }
        let value = self.literal()?;
        match (col.as_str(), value) {
            ("keyword", Literal::Str(s)) => Ok(Expr::KeywordEq(s)),
            ("tag", Literal::Str(s)) => Ok(Expr::TagEq(s)),
            ("id", Literal::Int(i)) if i >= 0 => Ok(Expr::IdEq(i as u64)),
            ("source", Literal::Str(s)) => Ok(Expr::SourceEq(s)),
            ("keyword", _) | ("tag", _) | ("source", _) => {
                Err(Error::Sql("keyword/tag/source need a string value".into()))
            }
            ("id", _) => Err(Error::Sql("id needs a non-negative integer value".into())),
            _ => Err(Error::Sql(format!(
                "column '{col}' supports only '=' with string values"
            ))),
        }
    }

    fn cmp_op(&mut self) -> Option<CmpOp> {
        if self.eat(&Token::Gt) {
            Some(CmpOp::Gt)
        } else if self.eat(&Token::Lt) {
            Some(CmpOp::Lt)
        } else if self.eat(&Token::Ge) {
            Some(CmpOp::Ge)
        } else if self.eat(&Token::Le) {
            Some(CmpOp::Le)
        } else {
            None
        }
    }

    fn float_value(&mut self) -> Result<f32> {
        match self.next() {
            Token::Float(f) => Ok(f),
            Token::Int(i) => Ok(i as f32),
            other => Err(Error::Sql(format!("expected number, got {other:?}"))),
        }
    }

    // ------- ORDER BY / LIMIT -------

    fn optional_order_by(&mut self) -> Result<Option<OrderBy>> {
        if !self.eat_word("order") {
            return Ok(None);
        }
        self.expect_word("by")?;
        let column = match self.next() {
            Token::Word(w) => w,
            other => return Err(Error::Sql(format!("expected column, got {other:?}"))),
        };
        if !matches!(
            column.as_str(),
            "id" | "created_at" | "updated_at" | "importance"
        ) {
            return Err(Error::Sql(format!(
                "cannot ORDER BY '{column}' (supported: id/created_at/updated_at/importance)"
            )));
        }
        let descending = if self.eat_word("desc") {
            true
        } else {
            self.eat_word("asc");
            false
        };
        Ok(Some(OrderBy {
            column,
            descending,
        }))
    }

    fn optional_limit(&mut self) -> Result<Option<usize>> {
        if !self.eat_word("limit") {
            return Ok(None);
        }
        match self.next() {
            Token::Int(n) if n >= 0 => Ok(Some(n as usize)),
            other => Err(Error::Sql(format!("LIMIT expects non-negative int, got {other:?}"))),
        }
    }

    // ------- DELETE -------

    fn delete_stmt(&mut self) -> Result<Statement> {
        self.expect_word("delete")?;
        self.expect_word("from")?;
        self.expect_word("memories")?;
        let filter = self.optional_where()?;
        Ok(Statement::Delete(DeleteStmt { filter }))
    }

    // ------- UPDATE -------

    fn update_stmt(&mut self) -> Result<Statement> {
        self.expect_word("update")?;
        self.expect_word("memories")?;
        self.expect_word("set")?;
        let mut assignments = Vec::new();
        loop {
            let col = match self.next() {
                Token::Word(w) => w,
                other => return Err(Error::Sql(format!("expected column, got {other:?}"))),
            };
            if !matches!(col.as_str(), "importance" | "tags" | "source" | "key_points") {
                return Err(Error::Sql(format!(
                    "column '{col}' is not updatable (supported: importance/tags/source/key_points)"
                )));
            }
            if !self.eat(&Token::Eq) {
                return Err(Error::Sql("expected '=' in SET".into()));
            }
            let value = self.literal()?;
            assignments.push((col, value));
            if self.eat(&Token::Comma) {
                continue;
            }
            break;
        }
        let filter = self.optional_where()?;
        Ok(Statement::Update(UpdateStmt {
            assignments,
            filter,
        }))
    }
}

/// 把 LIKE 模式(仅支持 %...% 首尾包裹)转成子串。
fn like_pattern_to_substring(pattern: &str) -> Result<String> {
    let p = pattern.trim();
    if !(p.starts_with('%') && p.ends_with('%') && p.len() >= 2) {
        return Err(Error::Sql(
            "LIKE pattern must be wrapped in '%' (only '%substring%' is supported)".into(),
        ));
    }
    let inner = &p[1..p.len() - 1];
    if inner.contains('%') || inner.contains('_') {
        return Err(Error::Sql(
            "LIKE wildcards inside the pattern are not supported".into(),
        ));
    }
    if inner.is_empty() {
        return Err(Error::Sql("empty LIKE substring".into()));
    }
    Ok(inner.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(sql: &str) -> Result<Statement> {
        Parser::parse(sql)
    }

    #[test]
    fn insert_with_columns() {
        let s = p("INSERT INTO memories (content, tags, importance) VALUES ('记得还书', '生活,待办', 0.8)").unwrap();
        match s {
            Statement::Insert(i) => {
                assert_eq!(i.columns, vec!["content", "tags", "importance"]);
                assert_eq!(i.values.len(), 3);
                assert_eq!(i.values[2], Literal::Float(0.8));
            }
            _ => panic!("wrong statement"),
        }
    }

    #[test]
    fn insert_positional() {
        let s = p("insert into memories values ('内容', 'rust', 'cli', 0.5);").unwrap();
        match s {
            Statement::Insert(i) => {
                assert!(i.columns.is_empty());
                assert_eq!(i.values.len(), 4);
            }
            _ => panic!("wrong statement"),
        }
    }

    #[test]
    fn select_star_where_and() {
        let s = p("SELECT * FROM memories WHERE keyword = 'rust' AND importance > 0.5 ORDER BY created_at DESC LIMIT 10").unwrap();
        match s {
            Statement::Select(sel) => {
                assert_eq!(sel.columns, vec![SelectColumn::Star]);
                assert!(matches!(sel.filter, Some(Expr::And(..))));
                let ob = sel.order_by.unwrap();
                assert_eq!(ob.column, "created_at");
                assert!(ob.descending);
                assert_eq!(sel.limit, Some(10));
            }
            _ => panic!("wrong statement"),
        }
    }

    #[test]
    fn select_columns_and_or_parens() {
        let s = p("SELECT id, content FROM memories WHERE (keyword = 'a' OR tag = 'b') AND content LIKE '%子串%'").unwrap();
        match s {
            Statement::Select(sel) => {
                assert_eq!(sel.columns.len(), 2);
                assert!(matches!(sel.filter, Some(Expr::And(..))));
            }
            _ => panic!("wrong statement"),
        }
    }

    #[test]
    fn delete_and_update() {
        assert!(matches!(
            p("DELETE FROM memories WHERE id = 7").unwrap(),
            Statement::Delete(_)
        ));
        let s = p("UPDATE memories SET importance = 0.9, tags = 'x' WHERE id = 1").unwrap();
        match s {
            Statement::Update(u) => {
                assert_eq!(u.assignments.len(), 2);
                assert_eq!(u.filter, Some(Expr::IdEq(1)));
            }
            _ => panic!("wrong statement"),
        }
    }

    #[test]
    fn meta_statements() {
        assert_eq!(p("CHECKPOINT").unwrap(), Statement::Checkpoint);
        assert_eq!(p("SHOW TABLES").unwrap(), Statement::ShowTables);
        assert_eq!(p("show status;").unwrap(), Statement::ShowStatus);
    }

    #[test]
    fn search_statement() {
        let s = p("SEARCH 'Rust 内存安全' IN main, work LIMIT 5").unwrap();
        match s {
            Statement::Search(s) => {
                assert_eq!(s.query, "Rust 内存安全");
                assert_eq!(s.dbs, vec!["main", "work"]);
                assert_eq!(s.limit, Some(5));
            }
            _ => panic!("wrong statement"),
        }
        // 不带 IN:当前库,库列表为空
        let s = p("search 'borrow checker';").unwrap();
        match s {
            Statement::Search(s) => {
                assert_eq!(s.query, "borrow checker");
                assert!(s.dbs.is_empty());
                assert_eq!(s.limit, None);
            }
            _ => panic!("wrong statement"),
        }
    }

    #[test]
    fn related_statement() {
        // 裸 id:当前库
        let s = p("RELATED TO 42 LIMIT 3").unwrap();
        match s {
            Statement::Related(r) => {
                assert_eq!(r.seed, RelatedSeed::Id(42));
                assert!(r.dbs.is_empty());
                assert_eq!(r.limit, Some(3));
            }
            _ => panic!("wrong statement"),
        }
        // db.id 限定名
        let s = p("RELATED TO work.7 IN main, work LIMIT 2").unwrap();
        match s {
            Statement::Related(r) => {
                assert_eq!(r.seed, RelatedSeed::QualifiedId("work".into(), 7));
                assert_eq!(r.dbs, vec!["main", "work"]);
                assert_eq!(r.limit, Some(2));
            }
            _ => panic!("wrong statement"),
        }
        let s = p("RELATED '所有权 生命周期'").unwrap();
        match s {
            Statement::Related(r) => {
                assert_eq!(r.seed, RelatedSeed::Text("所有权 生命周期".into()));
                assert_eq!(r.limit, None);
            }
            _ => panic!("wrong statement"),
        }
    }

    #[test]
    fn search_related_errors() {
        assert!(p("SEARCH 'rust' 'extra'").is_err());
        assert!(p("SEARCH 42").is_err());
        assert!(p("SEARCH 'rust' LIMIT -1").is_err());
        assert!(p("SEARCH 'rust' IN").is_err());
        assert!(p("SEARCH 'rust' IN 5").is_err());
        assert!(p("RELATED").is_err());
        assert!(p("RELATED TO -1").is_err());
        assert!(p("RELATED TO").is_err());
        assert!(p("RELATED 7").is_err());
        assert!(p("RELATED TO work.x").is_err());
    }

    #[test]
    fn database_ddl_statements() {
        match p("CREATE DATABASE work").unwrap() {
            Statement::CreateDatabase(s) => {
                assert_eq!(s.name, "work");
                assert!(!s.if_not_exists);
            }
            _ => panic!(),
        }
        match p("create database if not exists my_db;").unwrap() {
            Statement::CreateDatabase(s) => {
                assert_eq!(s.name, "my_db");
                assert!(s.if_not_exists);
            }
            _ => panic!(),
        }
        match p("DROP DATABASE IF EXISTS work").unwrap() {
            Statement::DropDatabase(s) => {
                assert_eq!(s.name, "work");
                assert!(s.if_exists);
            }
            _ => panic!(),
        }
        match p("USE Work").unwrap() {
            Statement::Use(s) => assert_eq!(s.name, "work"),
            _ => panic!(),
        }
        match p("ATTACH FILE 'C:/data/extra.ndb' AS extra").unwrap() {
            Statement::Attach(s) => {
                assert_eq!(s.path, "C:/data/extra.ndb");
                assert_eq!(s.name, "extra");
            }
            _ => panic!(),
        }
        assert_eq!(
            p("DETACH extra").unwrap(),
            Statement::Detach(DetachStmt {
                name: "extra".into()
            })
        );
    }

    #[test]
    fn database_ddl_errors() {
        assert!(p("CREATE").is_err());
        assert!(p("CREATE TABLE x").is_err());
        assert!(p("CREATE DATABASE").is_err());
        assert!(p("CREATE DATABASE 5x").is_err());
        assert!(p("DROP DATABASE").is_err());
        assert!(p("DROP").is_err());
        assert!(p("USE").is_err());
        assert!(p("ATTACH FILE").is_err());
        assert!(p("ATTACH FILE 'x.ndb'").is_err());
        assert!(p("DETACH").is_err());
    }

    #[test]
    fn user_management_statements() {
        match p("CREATE USER alice IDENTIFIED BY 'secret'").unwrap() {
            Statement::CreateUser(s) => {
                assert_eq!(s.name, "alice");
                assert_eq!(s.password, "secret");
                assert!(!s.if_not_exists);
            }
            _ => panic!(),
        }
        match p("create user if not exists bob identified by 'pw';").unwrap() {
            Statement::CreateUser(s) => assert!(s.if_not_exists && s.name == "bob"),
            _ => panic!(),
        }
        match p("DROP USER IF EXISTS alice").unwrap() {
            Statement::DropUser(s) => assert!(s.if_exists && s.name == "alice"),
            _ => panic!(),
        }
        match p("ALTER USER alice IDENTIFIED BY 'newpw'").unwrap() {
            Statement::AlterUser(s) => {
                assert_eq!(s.name, "alice");
                assert_eq!(s.password, "newpw");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn user_management_errors() {
        assert!(p("CREATE USER alice").is_err());
        assert!(p("CREATE USER alice IDENTIFIED BY ''").is_err());
        assert!(p("DROP USER").is_err());
        assert!(p("ALTER USER").is_err());
        assert!(p("ALTER USER alice").is_err());
    }

    #[test]
    fn grant_revoke_statements() {
        match p("GRANT READ ON work TO alice").unwrap() {
            Statement::Grant(s) => {
                assert_eq!(s.privileges, vec![Privilege::Read]);
                assert_eq!(s.object, GrantObject::Db("work".into()));
                assert_eq!(s.user, "alice");
            }
            _ => panic!(),
        }
        // ALL 展开三种权限;ON * = 全部库
        match p("GRANT ALL PRIVILEGES ON * TO bob").unwrap() {
            Statement::Grant(s) => {
                assert_eq!(
                    s.privileges,
                    vec![Privilege::Read, Privilege::Write, Privilege::Admin]
                );
                assert_eq!(s.object, GrantObject::AllDatabases);
            }
            _ => panic!(),
        }
        match p("REVOKE WRITE, ADMIN ON work FROM alice").unwrap() {
            Statement::Revoke(s) => {
                assert_eq!(s.privileges, vec![Privilege::Write, Privilege::Admin]);
                assert_eq!(s.user, "alice");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn grant_revoke_errors() {
        assert!(p("GRANT").is_err());
        assert!(p("GRANT READ").is_err());
        assert!(p("GRANT READ ON").is_err());
        assert!(p("GRANT READ ON work").is_err());
        assert!(p("GRANT FOO ON work TO alice").is_err());
        assert!(p("REVOKE READ ON * FROM").is_err());
    }

    #[test]
    fn show_extended_statements() {
        assert_eq!(p("SHOW DATABASES").unwrap(), Statement::ShowDatabases);
        assert_eq!(p("show users;").unwrap(), Statement::ShowUsers);
        match p("SHOW GRANTS").unwrap() {
            Statement::ShowGrants(s) => assert_eq!(s.user, None),
            _ => panic!(),
        }
        match p("SHOW GRANTS FOR alice").unwrap() {
            Statement::ShowGrants(s) => assert_eq!(s.user, Some("alice".into())),
            _ => panic!(),
        }
    }

    #[test]
    fn cache_statements() {
        assert_eq!(p("SHOW CACHE").unwrap(), Statement::ShowCache);
        assert_eq!(p("show cache;").unwrap(), Statement::ShowCache);
        assert_eq!(p("CLEAR CACHE").unwrap(), Statement::ClearCache);
        assert_eq!(p("clear cache;").unwrap(), Statement::ClearCache);
        assert_eq!(p("SHOW HOT").unwrap(), Statement::ShowHot(ShowHotStmt { limit: None }));
        match p("show hot limit 5;").unwrap() {
            Statement::ShowHot(s) => assert_eq!(s.limit, Some(5)),
            _ => panic!("wrong statement"),
        }
        match p("SET CACHE query 128").unwrap() {
            Statement::SetCache(s) => {
                assert_eq!(s.target, CacheTarget::Query);
                assert_eq!(s.capacity, 128);
            }
            _ => panic!("wrong statement"),
        }
        match p("set cache doc 0").unwrap() {
            Statement::SetCache(s) => {
                assert_eq!(s.target, CacheTarget::Doc);
                assert_eq!(s.capacity, 0);
            }
            _ => panic!("wrong statement"),
        }
        // 语句类型名
        assert_eq!(Statement::ShowCache.kind(), "show-cache");
        assert_eq!(Statement::ClearCache.kind(), "clear-cache");
        assert_eq!(
            Statement::ShowHot(ShowHotStmt { limit: None }).kind(),
            "show-hot"
        );
        assert_eq!(
            Statement::SetCache(SetCacheStmt {
                target: CacheTarget::Query,
                capacity: 1
            })
            .kind(),
            "set-cache"
        );
    }

    #[test]
    fn cache_statement_errors() {
        assert!(p("SHOW").is_err());
        assert!(p("SHOW HOT 5").is_err(), "SHOW HOT 的条数必须带 LIMIT");
        assert!(p("CLEAR").is_err());
        assert!(p("CLEAR CACHES").is_err());
        assert!(p("SET").is_err());
        assert!(p("SET CACHE").is_err());
        assert!(p("SET CACHE hot 10").is_err(), "hot 不是缓存目标");
        assert!(p("SET CACHE query -1").is_err());
        assert!(p("SET CACHE query 1.5").is_err());
        assert!(p("SET CACHE query").is_err());
        assert!(p("SET CACHE query 10 extra").is_err());
    }

    #[test]
    fn errors() {
        assert!(p("SELECT FROM memories").is_err());
        assert!(p("INSERT INTO memories VALUES ('a'").is_err());
        assert!(p("SELECT * FROM memories WHERE bogus = 1").is_err());
        assert!(p("SELECT * FROM memories WHERE content LIKE 'no-percent'").is_err());
        assert!(p("UPDATE memories SET id = 3").is_err());
        assert!(p("DELETE FROM books").is_err());
    }
}
