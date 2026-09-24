//! 递归下降语法分析,支持:
//! ```sql
//! INSERT INTO memories [(content,tags,source,importance)] VALUES (...)
//! SELECT [*|列,...] FROM memories [WHERE 条件] [ORDER BY 列 [ASC|DESC]] [LIMIT n]
//! SEARCH '自然语言查询' [LIMIT n]
//! RELATED TO <id> | RELATED '文本' [LIMIT n]
//! UPDATE memories SET 列=值[,...] [WHERE ...]
//! DELETE FROM memories [WHERE ...]
//! CHECKPOINT / SHOW TABLES / SHOW STATUS
//! ```
//! WHERE 条件:`id = n` / `keyword = '词'` / `tag = '标签'` /
//! `content LIKE '%子串%'` / `importance > 0.5`,支持 AND / OR / NOT 与括号。
//! SEARCH / RELATED 是 AI 记忆检索语句:分词后走 BM25 打分与共现图联想。

use nebula_core::{Error, Result};

use crate::ast::{
    CmpOp, DeleteStmt, Expr, InsertStmt, Literal, OrderBy, RelatedSeed, RelatedStmt, SearchStmt,
    SelectColumn, SelectStmt, Statement, UpdateStmt,
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
            "checkpoint" => {
                self.next();
                Ok(Statement::Checkpoint)
            }
            "show" => self.show_stmt(),
            other => Err(Error::Sql(format!("unsupported statement '{other}'"))),
        }
    }

    // ------- SEARCH / RELATED(AI 检索)-------

    /// SEARCH '查询文本' [LIMIT n]
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
        Ok(Statement::Search(SearchStmt {
            query,
            limit: self.optional_limit()?,
        }))
    }

    /// RELATED TO <id> | RELATED '文本' [LIMIT n]
    fn related_stmt(&mut self) -> Result<Statement> {
        self.expect_word("related")?;
        let seed = if self.eat_word("to") {
            match self.next() {
                Token::Int(n) if n >= 0 => RelatedSeed::Id(n as u64),
                other => {
                    return Err(Error::Sql(format!(
                        "RELATED TO expects a non-negative memory id, got {other:?}"
                    )))
                }
            }
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
        Ok(Statement::Related(RelatedStmt {
            seed,
            limit: self.optional_limit()?,
        }))
    }

    fn show_stmt(&mut self) -> Result<Statement> {
        self.expect_word("show")?;
        if self.eat_word("tables") {
            Ok(Statement::ShowTables)
        } else if self.eat_word("status") {
            Ok(Statement::ShowStatus)
        } else {
            Err(Error::Sql("SHOW supports TABLES or STATUS".into()))
        }
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
        let s = p("SEARCH 'Rust 内存安全' LIMIT 5").unwrap();
        match s {
            Statement::Search(s) => {
                assert_eq!(s.query, "Rust 内存安全");
                assert_eq!(s.limit, Some(5));
            }
            _ => panic!("wrong statement"),
        }
        // 大小写不敏感、LIMIT 可选、结尾分号允许
        let s = p("search 'borrow checker';").unwrap();
        match s {
            Statement::Search(s) => {
                assert_eq!(s.query, "borrow checker");
                assert_eq!(s.limit, None);
            }
            _ => panic!("wrong statement"),
        }
    }

    #[test]
    fn related_statement() {
        let s = p("RELATED TO 42 LIMIT 3").unwrap();
        match s {
            Statement::Related(r) => {
                assert_eq!(r.seed, RelatedSeed::Id(42));
                assert_eq!(r.limit, Some(3));
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
        assert!(p("RELATED").is_err());
        assert!(p("RELATED TO -1").is_err());
        assert!(p("RELATED TO").is_err());
        assert!(p("RELATED 7").is_err());
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
