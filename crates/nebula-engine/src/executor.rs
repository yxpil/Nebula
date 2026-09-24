//! SQL 语句执行:面向 [`MemBackend`](crate::backend::MemBackend) 的通用编排。
//!
//! 执行管线:parse → 权限校验([`crate::auth`])→ 后端读写 → 结果渲染。
//!
//! 检索策略(类 MySQL 的"索引优先,退化全表扫描"):
//! - `keyword =` / `tag =` / `id =` 走后端内存索引;
//! - 含 `content LIKE` / `importance >` / `source =` 的条件退化为记录扫描;
//! - AND/OR/NOT 组合在候选集上做布尔运算。
//!
//! AI 检索(SEARCH / RELATED):后端实现 BM25 → 共现图扩展 → 种子余弦
//! 三层模型与查询缓存;本模块负责库范围解析(跨库必须显式 IN)。

use std::collections::BTreeSet;

use nebula_core::{MemoryId, MemoryRecord, Result};
use nebula_sql::ast::{
    CacheTarget, CmpOp, Expr, GrantObject, InsertStmt, Literal, Privilege, RelatedSeed,
    RelatedStmt, SearchStmt, SelectColumn, SelectStmt, Statement,
};

use crate::auth::{require_admin, require_priv, Session, UndoAction};
use crate::backend::{RankedRow, SessionBackend};
use crate::Database;
use crate::format::{
    fmt_importance, fmt_key_points, fmt_keywords, fmt_tags, keywords_inline,
};

/// SELECT 投影允许的列名(与 project_row 对齐)。
const KNOWN_COLUMNS: &[&str] = &[
    "id",
    "content",
    "key_points",
    "keywords",
    "keywords_inline",
    "tags",
    "source",
    "importance",
    "created_at",
    "updated_at",
];

/// SEARCH / RELATED 结果列(跨库统一带 db 列)。
const RANKED_COLUMNS: &[&str] =
    &["db", "id", "score", "content", "keywords", "tags", "importance"];

/// 语句执行结果。
#[derive(Debug, Clone)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    /// DML/DDL 的文字反馈。
    pub message: String,
    /// 受影响行数(INSERT/DELETE/UPDATE)。
    pub affected: u64,
}

impl QueryResult {
    pub fn message(message: impl Into<String>) -> Self {
        QueryResult {
            columns: Vec::new(),
            rows: Vec::new(),
            message: message.into(),
            affected: 0,
        }
    }

    /// 空的检索结果(保留列头,message 说明原因)。
    pub fn ranked_empty(message: impl Into<String>) -> Self {
        QueryResult {
            columns: RANKED_COLUMNS.iter().map(|s| (*s).to_string()).collect(),
            rows: Vec::new(),
            message: message.into(),
            affected: 0,
        }
    }

    fn table(columns: Vec<String>, rows: Vec<Vec<String>>) -> Self {
        QueryResult {
            columns,
            rows,
            message: String::new(),
            affected: 0,
        }
    }
}

/// 在会话后端上执行一条 SQL(带会话)。
pub fn run(
    host: &mut dyn SessionBackend,
    session: &mut Session,
    sql: &str,
) -> Result<QueryResult> {
    let stmt = nebula_sql::parse(sql)?;
    dispatch(host, session, &stmt)
}

/// 执行分号分隔的语句序列,逐条返回结果(任一条失败即中断)。
pub fn run_script(
    host: &mut dyn SessionBackend,
    session: &mut Session,
    sql: &str,
) -> Result<Vec<QueryResult>> {
    let stmts = nebula_sql::parse_script(sql)?;
    let mut out = Vec::with_capacity(stmts.len());
    for stmt in &stmts {
        out.push(dispatch(host, session, stmt)?);
    }
    Ok(out)
}

/// 单条语句分发:权限闸门 + 执行。
pub fn dispatch(
    host: &mut dyn SessionBackend,
    session: &mut Session,
    stmt: &Statement,
) -> Result<QueryResult> {
    let user = session.user().to_string();
    let current = session.current_db().to_string();
    match stmt {
        Statement::Begin => {
            // 不支持嵌套:重复 BEGIN 明确报错而不是静默重置。
            session.begin_txn()?;
            host.set_persistence_deferred(true);
            nebula_core::log_info!("transaction started by user '{user}'");
            Ok(QueryResult::message(
                "transaction started; changes will be held in memory until COMMIT",
            ))
        }
        Statement::Commit => exec_commit(host, session),
        Statement::Rollback => exec_rollback(host, session),
        Statement::Insert(ins) => {
            require_priv(host, &user, &current, Privilege::Write)?;
            exec_insert(host, session, &current, ins)
        }
        Statement::Select(sel) => {
            require_priv(host, &user, &current, Privilege::Read)?;
            exec_select(host, &current, sel)
        }
        Statement::Delete(del) => {
            require_priv(host, &user, &current, Privilege::Write)?;
            exec_delete(host, session, &current, del.filter.as_ref())
        }
        Statement::Update(upd) => {
            require_priv(host, &user, &current, Privilege::Write)?;
            exec_update(host, session, &current, &upd.assignments, upd.filter.as_ref())
        }
        Statement::Search(s) => {
            let dbs = resolve_dbs(host, s.dbs.as_slice(), &current)?;
            for db in &dbs {
                require_priv(host, &user, db, Privilege::Read)?;
            }
            exec_search(host, s, &dbs)
        }
        Statement::Related(r) => {
            let (seed, dbs) = resolve_related(host, r, &current)?;
            for db in &dbs {
                require_priv(host, &user, db, Privilege::Read)?;
            }
            exec_related(host, seed, &dbs, r.limit)
        }
        Statement::CreateDatabase(s) => {
            require_admin(host, &user)?;
            implicit_commit(host, session)?;
            let created = host.create_db(&s.name)?;
            if !created && !s.if_not_exists {
                return Err(nebula_core::Error::Sql(format!(
                    "database '{}' already exists",
                    s.name
                )));
            }
            Ok(QueryResult::message(format!(
                "OK, database '{}' created",
                s.name
            )))
        }
        Statement::DropDatabase(s) => {
            require_admin(host, &user)?;
            implicit_commit(host, session)?;
            if !host.db_exists(&s.name) {
                if s.if_exists {
                    return Ok(QueryResult::message(format!(
                        "OK, database '{}' does not exist (skipped)",
                        s.name
                    )));
                }
                return Err(nebula_core::Error::Sql(format!(
                    "unknown database '{}'",
                    s.name
                )));
            }
            host.drop_db(&s.name)?;
            Ok(QueryResult::message(format!(
                "OK, database '{}' dropped",
                s.name
            )))
        }
        Statement::Use(s) => {
            if !host.db_exists(&s.name) {
                return Err(nebula_core::Error::Sql(format!(
                    "unknown database '{}'",
                    s.name
                )));
            }
            require_priv(host, &user, &s.name, Privilege::Read)?;
            host.on_use(&s.name)?;
            session.set_current_db(s.name.clone());
            Ok(QueryResult::message(format!("OK, now using database '{}'", s.name)))
        }
        Statement::Attach(s) => {
            require_admin(host, &user)?;
            implicit_commit(host, session)?;
            host.attach_file(&s.path, &s.name)?;
            Ok(QueryResult::message(format!(
                "OK, file '{}' attached as database '{}'",
                s.path, s.name
            )))
        }
        Statement::Detach(s) => {
            require_admin(host, &user)?;
            implicit_commit(host, session)?;
            host.detach_db(&s.name)?;
            Ok(QueryResult::message(format!(
                "OK, database '{}' detached",
                s.name
            )))
        }
        Statement::CreateUser(s) => {
            require_admin(host, &user)?;
            implicit_commit(host, session)?;
            let created = host.create_user(&s.name, &s.password, s.if_not_exists)?;
            Ok(QueryResult::message(format!(
                "OK, user '{}' {}",
                s.name,
                if created { "created" } else { "already exists (skipped)" }
            )))
        }
        Statement::DropUser(s) => {
            require_admin(host, &user)?;
            implicit_commit(host, session)?;
            let removed = host.drop_user(&s.name, s.if_exists)?;
            Ok(QueryResult::message(format!(
                "OK, user '{}' {}",
                s.name,
                if removed { "dropped" } else { "did not exist (skipped)" }
            )))
        }
        Statement::AlterUser(s) => {
            require_admin(host, &user)?;
            implicit_commit(host, session)?;
            host.alter_user(&s.name, &s.password)?;
            Ok(QueryResult::message(format!(
                "OK, password changed for user '{}'",
                s.name
            )))
        }
        Statement::Grant(s) => {
            require_admin(host, &user)?;
            implicit_commit(host, session)?;
            host.grant(&s.user, &s.object, &s.privileges)?;
            Ok(QueryResult::message(format!(
                "OK, privileges granted to '{}'",
                s.user
            )))
        }
        Statement::Revoke(s) => {
            require_admin(host, &user)?;
            implicit_commit(host, session)?;
            host.revoke(&s.user, &s.object, &s.privileges)?;
            Ok(QueryResult::message(format!(
                "OK, privileges revoked from '{}'",
                s.user
            )))
        }
        Statement::ShowCache => {
            require_priv(host, &user, &current, Privilege::Read)?;
            Ok(QueryResult::table(
                vec!["Variable".into(), "Value".into()],
                host.cache_rows(),
            ))
        }
        Statement::ClearCache => {
            require_admin(host, &user)?;
            host.clear_caches();
            Ok(QueryResult::message(
                "OK, query cache cleared (counters reset)",
            ))
        }
        Statement::ShowHot(s) => {
            require_priv(host, &user, &current, Privilege::Read)?;
            let n = s.limit.unwrap_or(crate::config::SearchConfig::default().default_limit);
            Ok(QueryResult::table(
                vec!["id".into(), "heat".into(), "cached".into()],
                host.hot_rows(&current, n),
            ))
        }
        Statement::SetCache(s) => {
            require_admin(host, &user)?;
            let capacity = s.capacity;
            host.set_cache_capacity(s.target, capacity);
            let name = match s.target {
                CacheTarget::Query => "query",
                CacheTarget::Doc => "doc",
            };
            Ok(QueryResult::message(format!(
                "OK, {name} cache capacity set to {capacity}"
            )))
        }
        Statement::Checkpoint => {
            require_admin(host, &user)?;
            // 清晰错误优先于后端内部的 deferred 保护。
            if session.in_transaction() {
                return Err(nebula_core::Error::Sql(
                    "cannot CHECKPOINT while a transaction is active; run COMMIT or ROLLBACK first"
                        .into(),
                ));
            }
            host.checkpoint()?;
            Ok(QueryResult::message("checkpoint done"))
        }
        Statement::ShowTables => {
            require_priv(host, &user, &current, Privilege::Read)?;
            Ok(QueryResult::table(
                vec!["Tables".into()],
                vec![vec!["memories".into()]],
            ))
        }
        Statement::ShowStatus => {
            require_priv(host, &user, &current, Privilege::Read)?;
            // 追加事务状态,便于确认 USE/事务是否活动。
            let mut rows = host.status_rows();
            rows.push(vec![
                "transaction".into(),
                if session.in_transaction() {
                    "active".into()
                } else {
                    "none".into()
                },
            ]);
            Ok(QueryResult::table(
                vec!["Variable".into(), "Value".into()],
                rows,
            ))
        }
        Statement::ShowDatabases => Ok(QueryResult::table(
            vec!["Database".into()],
            host.list_dbs().into_iter().map(|n| vec![n]).collect(),
        )),
        Statement::ShowUsers => {
            require_admin(host, &user)?;
            Ok(QueryResult::table(
                vec!["User".into()],
                host.list_users().into_iter().map(|u| vec![u]).collect(),
            ))
        }
        Statement::ShowGrants(s) => {
            let target = s.user.clone().unwrap_or_else(|| user.clone());
            if target != user {
                require_admin(host, &user)?;
            }
            let rows = host
                .list_grants(&target)
                .into_iter()
                .map(|(object, privs)| {
                    vec![format!(
                        "GRANT {} ON {} TO {}",
                        format_privs(&privs),
                        format_object(&object),
                        target
                    )]
                })
                .collect();
            Ok(QueryResult::table(vec!["Grants".into()], rows))
        }
    }
}

// ------- 事务:提交 / 回滚编排 -------

/// DDL 前的隐式提交:若当前有活动事务,先把它提交掉(类 MySQL 语义),
/// 避免"事务中执行不可逆 DDL"导致的回滚歧义。
fn implicit_commit(host: &mut dyn SessionBackend, session: &mut Session) -> Result<()> {
    if session.in_transaction() {
        // 丢弃 undo:提交即认可全部修改。
        session.take_txn();
        host.set_persistence_deferred(false);
        host.checkpoint()?;
        nebula_core::log_info!("transaction auto-committed before a DDL statement");
    }
    Ok(())
}

/// COMMIT 语句:认可事务中的全部修改并立即落盘。
fn exec_commit(host: &mut dyn SessionBackend, session: &mut Session) -> Result<QueryResult> {
    if session.take_txn().is_none() {
        return Err(nebula_core::Error::Sql(
            "no active transaction to commit".into(),
        ));
    }
    host.set_persistence_deferred(false);
    host.checkpoint()?;
    nebula_core::log_info!("transaction committed");
    Ok(QueryResult::message("transaction committed"))
}

/// ROLLBACK 语句:撤销事务中的全部修改。
fn exec_rollback(host: &mut dyn SessionBackend, session: &mut Session) -> Result<QueryResult> {
    rollback_txn(host, session)?;
    Ok(QueryResult::message("transaction rolled back"))
}

/// 回滚活动事务。
///
/// [`Statement::Rollback`] 与会话/连接结束时的清理共用本函数;
/// 无活动事务时返回错误(语句显式调用时)——调用方在会话清理场景
/// 可先检查 [`Session::in_transaction`]。
pub fn rollback_txn(host: &mut dyn SessionBackend, session: &mut Session) -> Result<()> {
    let Some(txn) = session.take_txn() else {
        return Err(nebula_core::Error::Sql(
            "no active transaction to roll back".into(),
        ));
    };
    // 反向应用 undo;期间持久化仍延迟,undo 的写操作不会半部落盘。
    let undos = txn.into_undos();
    if undos.is_empty() {
        nebula_core::log_info!("transaction rolled back (read-only; no changes)");
    } else {
        for action in undos.into_iter().rev() {
            apply_undo(host, action)?;
        }
        nebula_core::log_info!("transaction rolled back; pre-transaction state restored");
    }
    host.set_persistence_deferred(false);
    host.checkpoint()
}

/// 应用单条 undo(直接操作后端,不再产生新 undo)。
fn apply_undo(host: &mut dyn SessionBackend, action: UndoAction) -> Result<()> {
    match action {
        UndoAction::Inserted { db, id } => {
            host.delete_mems(&db, &[id])?;
        }
        UndoAction::Deleted { db, records } => {
            // 记录原样写回:put 新页 + 重建索引(旧页已释放也不影响)。
            for rec in records {
                host.replace_mem(&db, &rec)?;
            }
        }
        UndoAction::Replaced { db, old } => {
            host.replace_mem(&db, &old)?;
        }
    }
    Ok(())
}

/// 库列表解析:空 = 当前库;非空时去重并校验全部存在。
fn resolve_dbs(
    backend: &dyn SessionBackend,
    declared: &[String],
    current: &str,
) -> Result<Vec<String>> {
    let raw: Vec<String> = if declared.is_empty() {
        vec![current.to_string()]
    } else {
        dedup_keep(declared)
    };
    for db in &raw {
        if !backend.db_exists(db) {
            return Err(nebula_core::Error::Sql(format!(
                "unknown database '{db}'"
            )));
        }
    }
    Ok(raw)
}

/// RELATED 范围解析:裸 Id → 当前库限定名;dbs 缺省按种子库/当前库。
/// 返回 (后端种子, 生效库列表)。
fn resolve_related(
    backend: &dyn SessionBackend,
    stmt: &RelatedStmt,
    current: &str,
) -> Result<(RelatedSeed, Vec<String>)> {
    let seed = match &stmt.seed {
        RelatedSeed::Id(id) => RelatedSeed::QualifiedId(current.to_string(), *id),
        other => other.clone(),
    };
    let dbs = if stmt.dbs.is_empty() {
        match &seed {
            RelatedSeed::QualifiedId(db, _) => vec![db.clone()],
            _ => vec![current.to_string()],
        }
    } else {
        dedup_keep(stmt.dbs.as_slice())
    };
    for db in &dbs {
        if !backend.db_exists(db) {
            return Err(nebula_core::Error::Sql(format!(
                "unknown database '{db}'"
            )));
        }
    }
    // 限定种子的库必须在生效范围内
    if let RelatedSeed::QualifiedId(db, _) = &seed {
        if !dbs.iter().any(|name| name == db) {
            return Err(nebula_core::Error::Sql(format!(
                "seed database '{db}' is not in the declared IN list"
            )));
        }
    }
    Ok((seed, dbs))
}

/// 保持顺序的去重。
fn dedup_keep(items: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    items
        .iter()
        .filter(|s| seen.insert((*s).clone()))
        .cloned()
        .collect()
}

// ------- INSERT -------

fn exec_insert(
    backend: &mut dyn SessionBackend,
    session: &mut Session,
    db: &str,
    ins: &InsertStmt,
) -> Result<QueryResult> {
    // 位置默认顺序:content, tags, source, importance
    let mut content = String::new();
    let mut tags = Vec::new();
    let mut source = String::from("sql");
    let mut importance = 0.5f32;

    let cols: Vec<&str> = if ins.columns.is_empty() {
        vec!["content", "tags", "source", "importance"]
    } else {
        ins.columns.iter().map(String::as_str).collect()
    };
    for (col, value) in cols.iter().zip(ins.values.iter()) {
        match (*col, value) {
            ("content", Literal::Str(s)) => content = s.clone(),
            ("content", _) => {
                return Err(nebula_core::Error::Sql(
                    "content must be a string literal".into(),
                ))
            }
            ("tags", Literal::Str(s)) => {
                tags = s
                    .split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect();
            }
            ("source", Literal::Str(s)) => source = s.clone(),
            ("importance", Literal::Float(f)) => importance = f.clamp(0.0, 1.0),
            ("importance", Literal::Int(i)) => {
                importance = (*i as f32).clamp(0.0, 1.0)
            }
            (other, _) => {
                return Err(nebula_core::Error::Sql(format!(
                    "unknown insert column '{other}'"
                )))
            }
        }
    }
    if content.trim().is_empty() {
        return Err(nebula_core::Error::Sql(
            "content must not be empty".into(),
        ));
    }
    let id = backend.insert_mem(db, content, tags, source, importance)?;
    // 事务中:记录撤销动作(插入失败不会走到这里)。
    if session.in_transaction() {
        session.record_undo(UndoAction::Inserted {
            db: db.to_string(),
            id,
        })?;
    }
    Ok(QueryResult {
        columns: Vec::new(),
        rows: Vec::new(),
        message: format!("OK, inserted memory {db}.{id}"),
        affected: 1,
    })
}

// ------- SELECT -------

fn exec_select(
    backend: &mut dyn SessionBackend,
    db: &str,
    sel: &SelectStmt,
) -> Result<QueryResult> {
    let columns: Vec<String> = if sel.columns.iter().any(|c| *c == SelectColumn::Star) {
        vec![
            "id".into(),
            "content".into(),
            "key_points".into(),
            "keywords".into(),
            "tags".into(),
            "source".into(),
            "importance".into(),
            "created_at".into(),
        ]
    } else {
        let mut cols = Vec::with_capacity(sel.columns.len());
        for c in &sel.columns {
            match c {
                SelectColumn::Star => cols.push("*".into()),
                SelectColumn::Field(f) => {
                    if !KNOWN_COLUMNS.contains(&f.as_str()) {
                        return Err(nebula_core::Error::Sql(format!(
                            "unknown column '{f}'"
                        )));
                    }
                    cols.push(f.clone())
                }
            }
        }
        cols
    };

    let candidates: Vec<MemoryId> = match &sel.filter {
        Some(expr) => candidate_ids(backend, db, expr),
        None => backend.all_ids(db),
    };

    let mut records: Vec<MemoryRecord> = Vec::with_capacity(candidates.len());
    for id in candidates {
        if let Some(rec) = backend.fetch_mem(db, id)? {
            records.push(rec);
        }
    }

    if let Some(order) = &sel.order_by {
        match order.column.as_str() {
            "id" => records.sort_by_key(|r| r.id),
            "created_at" => records.sort_by_key(|r| r.created_at),
            "updated_at" => records.sort_by_key(|r| r.updated_at),
            "importance" => records.sort_by(|a, b| {
                a.importance
                    .partial_cmp(&b.importance)
                    .unwrap_or(std::cmp::Ordering::Equal)
            }),
            _ => {}
        }
        if order.descending {
            records.reverse();
        }
    }
    if let Some(limit) = sel.limit {
        records.truncate(limit);
    }

    // 展示截断上限:记录中携带的关键词数;无配置上下文时以记录自身为准。
    let rows = records
        .iter()
        .map(|r| project_row(r, &columns, r.keywords.len().max(r.key_points.len())))
        .collect();
    Ok(QueryResult::table(columns, rows))
}

/// WHERE 候选 id:索引可评估走索引,否则全表扫描后逐行评估。
fn candidate_ids(backend: &mut dyn SessionBackend, db: &str, expr: &Expr) -> Vec<MemoryId> {
    if let Some(ids) = index_only_ids(backend, db, expr) {
        return ids;
    }
    let all = backend.all_ids(db);
    let mut out = Vec::new();
    for id in all {
        if let Ok(Some(rec)) = backend.fetch_mem(db, id) {
            if eval_record(expr, &rec) {
                out.push(id);
            }
        }
    }
    out
}

// ------- SEARCH / RELATED 结果物化 -------

fn exec_search(
    backend: &mut dyn SessionBackend,
    stmt: &SearchStmt,
    dbs: &[String],
) -> Result<QueryResult> {
    let cfg = crate::config::SearchConfig::default();
    let limit = stmt.limit.unwrap_or(cfg.default_limit);
    let rows = backend.search_ranked(dbs, &stmt.query, limit)?;
    if rows.is_empty() {
        return Ok(QueryResult::ranked_empty("no matching memories"));
    }
    materialize_ranked(backend, rows)
}

fn exec_related(
    backend: &mut dyn SessionBackend,
    seed: RelatedSeed,
    dbs: &[String],
    limit: Option<usize>,
) -> Result<QueryResult> {
    let cfg = crate::config::SearchConfig::default();
    let n = limit.unwrap_or(cfg.default_limit);
    let rows = backend.related_ranked(dbs, seed, n)?;
    if rows.is_empty() {
        return Ok(QueryResult::ranked_empty("no related memories"));
    }
    materialize_ranked(backend, rows)
}

fn materialize_ranked(
    backend: &mut dyn SessionBackend,
    ranked: Vec<RankedRow>,
) -> Result<QueryResult> {
    let mut rows = Vec::with_capacity(ranked.len());
    for row in ranked {
        if let Some(rec) = backend.fetch_mem(&row.db, row.id)? {
            let kw_max = rec.keywords.len();
            rows.push(vec![
                row.db,
                rec.id.to_string(),
                format!("{:.4}", row.score),
                rec.content,
                fmt_keywords(&rec.keywords, kw_max),
                fmt_tags(&rec.tags),
                fmt_importance(rec.importance),
            ]);
        }
    }
    Ok(QueryResult::table(
        RANKED_COLUMNS.iter().map(|s| (*s).to_string()).collect(),
        rows,
    ))
}

// ------- DELETE / UPDATE -------

fn exec_delete(
    backend: &mut dyn SessionBackend,
    session: &mut Session,
    db: &str,
    filter: Option<&Expr>,
) -> Result<QueryResult> {
    let Some(expr) = filter else {
        return Err(nebula_core::Error::Sql(
            "DELETE requires a WHERE clause in this build (safety)".into(),
        ));
    };
    let ids = candidate_ids(backend, db, expr);
    // 事务中:删除前取回完整记录,供 ROLLBACK 原样写回。
    if session.in_transaction() && !ids.is_empty() {
        let mut records = Vec::with_capacity(ids.len());
        for id in &ids {
            if let Some(rec) = backend.fetch_mem(db, *id)? {
                records.push(rec);
            }
        }
        if !records.is_empty() {
            session.record_undo(UndoAction::Deleted {
                db: db.to_string(),
                records,
            })?;
        }
    }
    let count = backend.delete_mems(db, &ids)?;
    Ok(QueryResult {
        columns: Vec::new(),
        rows: Vec::new(),
        message: format!("OK, deleted {count} memories"),
        affected: count,
    })
}

fn exec_update(
    backend: &mut dyn SessionBackend,
    session: &mut Session,
    db: &str,
    assignments: &[(String, Literal)],
    filter: Option<&Expr>,
) -> Result<QueryResult> {
    let ids: Vec<MemoryId> = match filter {
        Some(expr) => candidate_ids(backend, db, expr),
        None => backend.all_ids(db),
    };
    let mut updated = 0u64;
    for id in ids {
        let Some(mut rec) = backend.fetch_mem(db, id)? else {
            continue;
        };
        // 先留存旧记录:事务中若实际改动,用于 ROLLBACK。
        let old_rec = if session.in_transaction() {
            Some(rec.clone())
        } else {
            None
        };
        let mut changed = false;
        for (col, value) in assignments {
            match (col.as_str(), value) {
                ("importance", Literal::Float(f)) => {
                    rec.importance = f.clamp(0.0, 1.0);
                    changed = true;
                }
                ("importance", Literal::Int(i)) => {
                    rec.importance = (*i as f32).clamp(0.0, 1.0);
                    changed = true;
                }
                ("tags", Literal::Str(s)) => {
                    rec.tags = s
                        .split(',')
                        .map(|t| t.trim().to_string())
                        .filter(|t| !t.is_empty())
                        .collect();
                    changed = true;
                }
                ("source", Literal::Str(s)) => {
                    rec.source = s.clone();
                    changed = true;
                }
                ("key_points", Literal::Str(s)) => {
                    rec.key_points = s
                        .split('|')
                        .map(|t| t.trim().to_string())
                        .filter(|t| !t.is_empty())
                        .collect();
                    changed = true;
                }
                (other, _) => {
                    return Err(nebula_core::Error::Sql(format!(
                        "column '{other}' is not updatable"
                    )))
                }
            }
        }
        if changed {
            backend.replace_mem(db, &rec)?;
            if let Some(old) = old_rec {
                session.record_undo(UndoAction::Replaced {
                    db: db.to_string(),
                    old,
                })?;
            }
            updated += 1;
        }
    }
    Ok(QueryResult {
        columns: Vec::new(),
        rows: Vec::new(),
        message: format!("OK, updated {updated} memories"),
        affected: updated,
    })
}

// ------- WHERE 索引/行评估 -------

fn index_only_ids(backend: &dyn SessionBackend, db: &str, expr: &Expr) -> Option<Vec<MemoryId>> {
    match expr {
        Expr::KeywordEq(t) => Some(
            backend
                .keyword_hits(db, t)
                .into_iter()
                .map(|(id, _)| id)
                .collect(),
        ),
        Expr::TagEq(t) => Some(backend.tag_hits(db, t)),
        Expr::IdEq(id) => {
            if backend.all_ids(db).contains(id) {
                Some(vec![*id])
            } else {
                Some(Vec::new())
            }
        }
        Expr::And(a, b) => {
            let x = index_only_ids(backend, db, a)?;
            let y = index_only_ids(backend, db, b)?;
            Some(intersect_sorted(x, y))
        }
        Expr::Or(a, b) => {
            let x = index_only_ids(backend, db, a)?;
            let y = index_only_ids(backend, db, b)?;
            let mut set: BTreeSet<MemoryId> = x.into_iter().collect();
            set.extend(y);
            Some(set.into_iter().collect())
        }
        Expr::Not(inner) => {
            let hit = index_only_ids(backend, db, inner)?;
            let excluded: BTreeSet<MemoryId> = hit.into_iter().collect();
            Some(
                backend
                    .all_ids(db)
                    .into_iter()
                    .filter(|id| !excluded.contains(id))
                    .collect(),
            )
        }
        Expr::ContentLike(_) | Expr::ImportanceCmp { .. } | Expr::SourceEq(_) => None,
    }
}

fn intersect_sorted(a: Vec<MemoryId>, b: Vec<MemoryId>) -> Vec<MemoryId> {
    let sb: BTreeSet<MemoryId> = b.into_iter().collect();
    let mut out: Vec<MemoryId> = a.into_iter().filter(|id| sb.contains(id)).collect();
    out.sort_unstable();
    out.dedup();
    out
}

fn eval_record(expr: &Expr, rec: &MemoryRecord) -> bool {
    match expr {
        Expr::KeywordEq(t) => rec.keywords.iter().any(|k| &k.term == t),
        Expr::TagEq(t) => rec.tags.iter().any(|tag| tag == t),
        Expr::IdEq(id) => rec.id == *id,
        Expr::SourceEq(s) => &rec.source == s,
        Expr::ContentLike(sub) => rec.content.contains(sub.as_str()),
        Expr::ImportanceCmp { op, value } => match op {
            CmpOp::Gt => rec.importance > *value,
            CmpOp::Lt => rec.importance < *value,
            CmpOp::Ge => rec.importance >= *value,
            CmpOp::Le => rec.importance <= *value,
        },
        Expr::And(a, b) => eval_record(a, rec) && eval_record(b, rec),
        Expr::Or(a, b) => eval_record(a, rec) || eval_record(b, rec),
        Expr::Not(a) => !eval_record(a, rec),
    }
}

/// 行投影;关键词/关键点按记录自身的数量展示(渲染层无提取配置)。
fn project_row(rec: &MemoryRecord, columns: &[String], _cap: usize) -> Vec<String> {
    columns
        .iter()
        .map(|col| match col.as_str() {
            "id" => rec.id.to_string(),
            "content" => rec.content.clone(),
            "key_points" => fmt_key_points(&rec.key_points, rec.key_points.len()),
            "keywords" => fmt_keywords(&rec.keywords, rec.keywords.len()),
            "keywords_inline" => keywords_inline(&rec.keywords, rec.keywords.len()),
            "tags" => fmt_tags(&rec.tags),
            "source" => rec.source.clone(),
            "importance" => fmt_importance(rec.importance),
            "created_at" => crate::format::fmt_time(rec.created_at),
            "updated_at" => crate::format::fmt_time(rec.updated_at),
            other => format!("<unknown column '{other}'>"),
        })
        .collect()
}

fn format_privs(privs: &[Privilege]) -> String {
    privs
        .iter()
        .map(|p| format!("{p:?}").to_uppercase())
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_object(object: &GrantObject) -> String {
    match object {
        GrantObject::Db(name) => name.clone(),
        GrantObject::AllDatabases => "*".into(),
    }
}

impl Database {
    /// 单文件便捷执行:内置管理员会话(Database 自身为全权用户目录)。
    pub fn execute(&mut self, sql: &str) -> Result<QueryResult> {
        let mut session = Session::admin();
        crate::executor::run(self, &mut session, sql)
    }

    /// 带会话执行(服务端/REPL;Database 自身的全权目录兜底,
    /// 真正多用户目录应使用 Cluster)。
    pub fn execute_session(
        &mut self,
        sql: &str,
        session: &mut Session,
    ) -> Result<QueryResult> {
        crate::executor::run(self, session, sql)
    }

    /// 单文件脚本便捷执行。
    pub fn execute_script(&mut self, sql: &str) -> Result<Vec<QueryResult>> {
        let mut session = Session::admin();
        crate::executor::run_script(self, &mut session, sql)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{Privilege, UserDirectory};
    use crate::backend::MemBackend;
    use crate::Database;

    fn tmp(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("nebula_exec_{}_{}.ndb", name, std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    const PW: &str = "executor-test-password";

    fn db(name: &str) -> Database {
        Database::create(&tmp(name), PW, 4096).unwrap()
    }

    #[test]
    fn crud_roundtrip() {
        let mut d = db("crud");
        let r = d
            .execute("INSERT INTO memories VALUES ('Rust 所有权规则', 'rust,lang', 'cli', 0.8)")
            .unwrap();
        assert!(r.message.contains("main.1"));
        let r = d.execute("SELECT * FROM memories WHERE keyword = 'rust'").unwrap();
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0][0], "1");
        // UPDATE + DELETE
        d.execute("UPDATE memories SET importance = 0.2 WHERE id = 1").unwrap();
        let r = d.execute("SELECT importance FROM memories WHERE id = 1").unwrap();
        assert_eq!(r.rows[0][0], "0.20");
        d.execute("DELETE FROM memories WHERE id = 1").unwrap();
        assert_eq!(d.execute("SELECT id FROM memories").unwrap().rows.len(), 0);
    }

    #[test]
    fn search_current_db_only_by_default() {
        let mut d = db("searchscope");
        d.create_database("work").unwrap();
        let mut session = Session::admin();
        run(&mut d, &mut session,
            "INSERT INTO memories VALUES ('Rust 内存安全', 'rust', 't', 0.9)").unwrap();
        run(&mut d, &mut session, "USE work").unwrap();
        run(&mut d, &mut session,
            "INSERT INTO memories (content) VALUES ('Rust 工作记录')").unwrap();
        // 当前库是 work:只搜 work 得 1 条
        let r = run(&mut d, &mut session, "SEARCH 'rust' LIMIT 5").unwrap();
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0][0], "work");
        // 显式双库聚合
        let r = d.execute("SEARCH 'rust' IN main, work LIMIT 5").unwrap();
        assert_eq!(r.rows.len(), 2);
    }

    #[test]
    fn related_with_qualified_seed() {
        let mut d = db("relqual");
        d.create_database("work").unwrap();
        let mut session = Session::admin();
        run(&mut d, &mut session,
            "INSERT INTO memories VALUES ('Rust 所有权与借用规则', 'rust', 't', 0.9)").unwrap();
        run(&mut d, &mut session, "USE work").unwrap();
        run(&mut d, &mut session,
            "INSERT INTO memories (content,source) VALUES ('Rust 项目周会', 't')").unwrap();
        // db.id 限定种子,跨两库联想
        let r = run(&mut d, &mut session,
            "RELATED TO main.1 IN main, work LIMIT 3").unwrap();
        assert!(!r.rows.is_empty());
        // 种子自身不出现
        assert!(!r.rows.iter().any(|row| row[0] == "main" && row[1] == "1"));
        // 切回 main:裸 id 不带 IN = 仅当前库;main 库已无其它文档 → 空
        run(&mut d, &mut session, "USE main").unwrap();
        let r = run(&mut d, &mut session, "RELATED TO 1 LIMIT 3").unwrap();
        assert!(r.rows.is_empty(), "未声明 IN 时不应搜到 work 库文档");
    }

    #[test]
    fn use_changes_session_db() {
        let mut d = db("use");
        d.create_database("work").unwrap();
        let mut session = Session::admin();
        crate::executor::run(&mut d, &mut session, "USE work").unwrap();
        assert_eq!(session.current_db(), "work");
        let r = crate::executor::run(&mut d, &mut session, "SELECT id FROM memories")
            .unwrap();
        assert!(r.rows.is_empty());
    }

    #[test]
    fn permission_denied_with_restricted_directory() {
        // 受限主机:数据操作委托 Database,用户目录只给 bob 的 main.Read
        use nebula_sql::ast::{CacheTarget, GrantObject, RelatedSeed};

        struct RestrictedHost {
            db: Database,
        }
        impl MemBackend for RestrictedHost {
            fn list_dbs(&self) -> Vec<String> {
                self.db.list_dbs()
            }
            fn db_exists(&self, db: &str) -> bool {
                self.db.db_exists(db)
            }
            fn create_db(&mut self, db: &str) -> Result<bool> {
                self.db.create_db(db)
            }
            fn drop_db(&mut self, db: &str) -> Result<()> {
                self.db.drop_db(db)
            }
            fn attach_file(&mut self, p: &str, n: &str) -> Result<()> {
                self.db.attach_file(p, n)
            }
            fn detach_db(&mut self, n: &str) -> Result<()> {
                self.db.detach_db(n)
            }
            fn on_use(&mut self, db: &str) -> Result<()> {
                self.db.on_use(db)
            }
            fn insert_mem(
                &mut self,
                db: &str,
                c: String,
                t: Vec<String>,
                s: String,
                i: f32,
            ) -> Result<MemoryId> {
                self.db.insert_mem(db, c, t, s, i)
            }
            fn fetch_mem(&mut self, db: &str, id: MemoryId) -> Result<Option<MemoryRecord>> {
                self.db.fetch_mem(db, id)
            }
            fn replace_mem(&mut self, db: &str, r: &MemoryRecord) -> Result<()> {
                self.db.replace_mem(db, r)
            }
            fn delete_mems(&mut self, db: &str, ids: &[MemoryId]) -> Result<u64> {
                self.db.delete_mems(db, ids)
            }
            fn keyword_hits(&self, db: &str, term: &str) -> Vec<(MemoryId, f32)> {
                self.db.keyword_hits(db, term)
            }
            fn tag_hits(&self, db: &str, tag: &str) -> Vec<MemoryId> {
                self.db.tag_hits(db, tag)
            }
            fn all_ids(&self, db: &str) -> Vec<MemoryId> {
                self.db.all_ids(db)
            }
            fn search_ranked(
                &mut self,
                dbs: &[String],
                q: &str,
                l: usize,
            ) -> Result<Vec<RankedRow>> {
                self.db.search_ranked(dbs, q, l)
            }
            fn related_ranked(
                &mut self,
                dbs: &[String],
                s: RelatedSeed,
                l: usize,
            ) -> Result<Vec<RankedRow>> {
                self.db.related_ranked(dbs, s, l)
            }
            fn cache_rows(&self) -> Vec<Vec<String>> {
                self.db.cache_rows()
            }
            fn clear_caches(&mut self) {
                self.db.clear_caches()
            }
            fn set_cache_capacity(&mut self, t: CacheTarget, c: usize) {
                self.db.set_cache_capacity(t, c)
            }
            fn hot_rows(&self, db: &str, l: usize) -> Vec<Vec<String>> {
                self.db.hot_rows(db, l)
            }
            fn status_rows(&self) -> Vec<Vec<String>> {
                self.db.status_rows()
            }
            fn checkpoint(&mut self) -> Result<()> {
                self.db.checkpoint()
            }
            fn set_persistence_deferred(&mut self, deferred: bool) {
                self.db.set_persistence_deferred(deferred)
            }
        }

        struct Restricted;
        impl UserDirectory for Restricted {
            fn user_exists(&self, u: &str) -> bool {
                u == "bob"
            }
            fn has_priv(&self, u: &str, db: &str, p: Privilege) -> bool {
                u == "bob" && db == "main" && p == Privilege::Read
            }
            fn is_admin(&self, _u: &str) -> bool {
                false
            }
            fn create_user(&mut self, _: &str, _: &str, _: bool) -> Result<bool> {
                Err(nebula_core::Error::Auth("x".into()))
            }
            fn drop_user(&mut self, _: &str, _: bool) -> Result<bool> {
                Err(nebula_core::Error::Auth("x".into()))
            }
            fn alter_user(&mut self, _: &str, _: &str) -> Result<()> {
                Err(nebula_core::Error::Auth("x".into()))
            }
            fn grant(
                &mut self,
                _: &str,
                _: &GrantObject,
                _: &[Privilege],
            ) -> Result<()> {
                Err(nebula_core::Error::Auth("x".into()))
            }
            fn revoke(
                &mut self,
                _: &str,
                _: &GrantObject,
                _: &[Privilege],
            ) -> Result<()> {
                Err(nebula_core::Error::Auth("x".into()))
            }
            fn list_users(&self) -> Vec<String> {
                vec![]
            }
            fn list_grants(&self, _: &str) -> Vec<(GrantObject, Vec<Privilege>)> {
                vec![]
            }
        }

        let mut host = RestrictedHost { db: db("denied") };
        let _session = Session::new("bob", "main");
        let mut users = Restricted;
        // Read 权限允许:数据查询路径(全 id 扫描)可正常执行
        require_priv(&users, "bob", "main", Privilege::Read).unwrap();
        assert!(host.all_ids("main").is_empty());
        // Write 拒绝
        assert!(require_priv(&users, "bob", "main", Privilege::Write).is_err());
        // 管理语句拒绝
        assert!(require_admin(&users, "bob").is_err());
        // 不存在的库
        assert!(!host.db_exists("ghost"));
    }

    #[test]
    fn show_databases_and_cache_meta() {
        let mut d = db("showdb");
        d.create_database("work").unwrap();
        let r = d.execute("SHOW DATABASES").unwrap();
        assert_eq!(
            r.rows.iter().map(|x| x[0].clone()).collect::<Vec<_>>(),
            vec!["main", "work"]
        );
        let r = d.execute("SHOW CACHE").unwrap();
        assert!(r.rows.iter().any(|x| x[0] == "query_cache_capacity"));
        let r = d.execute("SHOW STATUS").unwrap();
        assert!(r.rows.iter().any(|x| x[0] == "databases"));
    }

    // ---------- 事务 ----------

    #[test]
    fn transaction_rollback_restores_insert_update_delete() {
        let mut d = db("txn_rb");
        let mut s = Session::admin();
        // 事务前的已提交基线:id=1, importance=0.5
        run(
            &mut d,
            &mut s,
            "INSERT INTO memories VALUES ('旧内容', 'k1', 't1', 0.5)",
        )
        .unwrap();

        run(&mut d, &mut s, "BEGIN").unwrap();
        run(
            &mut d,
            &mut s,
            "INSERT INTO memories (content) VALUES ('事务内新增')",
        )
        .unwrap(); // id=2
        run(&mut d, &mut s, "UPDATE memories SET importance = 0.1 WHERE id = 1").unwrap();
        run(&mut d, &mut s, "DELETE FROM memories WHERE id = 1").unwrap();
        // 事务内:改动对当前会话可见(只剩 id=2)。
        assert_eq!(
            run(&mut d, &mut s, "SELECT id FROM memories").unwrap().rows.len(),
            1
        );

        run(&mut d, &mut s, "ROLLBACK").unwrap();
        assert!(!s.in_transaction());
        // 全部恢复:新增消失,被删/改的旧记录原样回来。
        let r = run(&mut d, &mut s, "SELECT id, importance FROM memories").unwrap();
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0][0], "1");
        assert_eq!(r.rows[0][1], "0.50");
    }

    #[test]
    fn transaction_commit_persists_to_disk() {
        let mut d = db("txn_commit");
        let path = d.path().to_path_buf();
        let mut s = Session::admin();
        run(&mut d, &mut s, "BEGIN").unwrap();
        run(
            &mut d,
            &mut s,
            "INSERT INTO memories (content) VALUES ('已提交内容')",
        )
        .unwrap();
        run(&mut d, &mut s, "COMMIT").unwrap();
        assert!(!s.in_transaction());
        assert_eq!(
            run(&mut d, &mut s, "SELECT id FROM memories").unwrap().rows.len(),
            1
        );
        d.close().unwrap();
        // 重开数据库:COMMIT 的修改必须仍在。
        let mut d2 = Database::open(&path, PW).unwrap();
        assert_eq!(
            d2.execute("SELECT id FROM memories").unwrap().rows.len(),
            1
        );
    }

    #[test]
    fn transaction_control_statements_reject_invalid_use() {
        let mut d = db("txn_ctrl");
        let mut s = Session::admin();
        // 无活动事务:提交/回滚都应报错。
        assert!(run(&mut d, &mut s, "COMMIT").is_err());
        assert!(run(&mut d, &mut s, "ROLLBACK").is_err());
        // 嵌套事务不允许。
        run(&mut d, &mut s, "BEGIN").unwrap();
        assert!(run(&mut d, &mut s, "BEGIN").is_err());
        // 事务中不能手工 checkpoint(会绕过延迟持久化保护)。
        assert!(run(&mut d, &mut s, "CHECKPOINT").is_err());
        // 收尾:回滚后恢复正常状态。
        run(&mut d, &mut s, "ROLLBACK").unwrap();
        assert!(!s.in_transaction());
    }

    #[test]
    fn ddl_implicitly_commits_open_transaction() {
        let mut d = db("txn_ddl");
        let mut s = Session::admin();
        run(
            &mut d,
            &mut s,
            "INSERT INTO memories (content) VALUES ('基线')",
        )
        .unwrap();
        run(&mut d, &mut s, "BEGIN").unwrap();
        run(
            &mut d,
            &mut s,
            "INSERT INTO memories (content) VALUES ('事务内写入')",
        )
        .unwrap();
        // DDL 无法用 undo 撤销(涉及建库物理操作):执行前隐式提交。
        run(&mut d, &mut s, "CREATE DATABASE work").unwrap();
        assert!(!s.in_transaction(), "DDL 后事务应已自动提交");
        // 两条记录均已落盘可见。
        assert_eq!(
            run(&mut d, &mut s, "SELECT id FROM memories").unwrap().rows.len(),
            2
        );
        // 事务已不存在:回滚报错,改动不会被撤销。
        assert!(run(&mut d, &mut s, "ROLLBACK").is_err());
    }

    #[test]
    fn uncommitted_changes_are_invisible_after_crash_reopen() {
        let mut d = db("txn_crash");
        let path = d.path().to_path_buf();
        let mut s = Session::admin();
        run(
            &mut d,
            &mut s,
            "INSERT INTO memories VALUES ('基线', 'k', 't', 0.5)",
        )
        .unwrap();
        d.close().unwrap();

        // 场景一:事务未提交时"进程崩溃"。
        // drop 只会尽力 checkpoint,而 deferred 状态下 checkpoint 被拒,
        // catalog 从未提交 —— 等价于进程异常退出。
        let mut d = Database::open(&path, PW).unwrap();
        run(&mut d, &mut s, "BEGIN").unwrap();
        run(
            &mut d,
            &mut s,
            "INSERT INTO memories (content) VALUES ('未提交内容')",
        )
        .unwrap();
        drop(d);
        let mut d2 = Database::open(&path, PW).unwrap();
        assert_eq!(
            d2.execute("SELECT id FROM memories").unwrap().rows.len(),
            1,
            "崩溃重开后未提交数据必须不可见"
        );

        // 场景二对照:COMMIT 之后即使进程崩溃,数据也必须保留。
        let mut s2 = Session::admin();
        run(&mut d2, &mut s2, "BEGIN").unwrap();
        run(
            &mut d2,
            &mut s2,
            "INSERT INTO memories (content) VALUES ('已提交内容')",
        )
        .unwrap();
        run(&mut d2, &mut s2, "COMMIT").unwrap();
        drop(d2);
        let mut d3 = Database::open(&path, PW).unwrap();
        assert_eq!(
            d3.execute("SELECT id FROM memories").unwrap().rows.len(),
            2,
            "COMMIT 后崩溃重开数据必须仍在"
        );
    }
}
