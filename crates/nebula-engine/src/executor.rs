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

use crate::auth::{require_admin, require_priv, Session};
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
        Statement::Insert(ins) => {
            require_priv(host, &user, &current, Privilege::Write)?;
            exec_insert(host, &current, ins)
        }
        Statement::Select(sel) => {
            require_priv(host, &user, &current, Privilege::Read)?;
            exec_select(host, &current, sel)
        }
        Statement::Delete(del) => {
            require_priv(host, &user, &current, Privilege::Write)?;
            exec_delete(host, &current, del.filter.as_ref())
        }
        Statement::Update(upd) => {
            require_priv(host, &user, &current, Privilege::Write)?;
            exec_update(host, &current, &upd.assignments, upd.filter.as_ref())
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
            host.attach_file(&s.path, &s.name)?;
            Ok(QueryResult::message(format!(
                "OK, file '{}' attached as database '{}'",
                s.path, s.name
            )))
        }
        Statement::Detach(s) => {
            require_admin(host, &user)?;
            host.detach_db(&s.name)?;
            Ok(QueryResult::message(format!(
                "OK, database '{}' detached",
                s.name
            )))
        }
        Statement::CreateUser(s) => {
            require_admin(host, &user)?;
            let created = host.create_user(&s.name, &s.password, s.if_not_exists)?;
            Ok(QueryResult::message(format!(
                "OK, user '{}' {}",
                s.name,
                if created { "created" } else { "already exists (skipped)" }
            )))
        }
        Statement::DropUser(s) => {
            require_admin(host, &user)?;
            let removed = host.drop_user(&s.name, s.if_exists)?;
            Ok(QueryResult::message(format!(
                "OK, user '{}' {}",
                s.name,
                if removed { "dropped" } else { "did not exist (skipped)" }
            )))
        }
        Statement::AlterUser(s) => {
            require_admin(host, &user)?;
            host.alter_user(&s.name, &s.password)?;
            Ok(QueryResult::message(format!(
                "OK, password changed for user '{}'",
                s.name
            )))
        }
        Statement::Grant(s) => {
            require_admin(host, &user)?;
            host.grant(&s.user, &s.object, &s.privileges)?;
            Ok(QueryResult::message(format!(
                "OK, privileges granted to '{}'",
                s.user
            )))
        }
        Statement::Revoke(s) => {
            require_admin(host, &user)?;
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
            Ok(QueryResult::table(
                vec!["Variable".into(), "Value".into()],
                host.status_rows(),
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
    db: &str,
    filter: Option<&Expr>,
) -> Result<QueryResult> {
    let Some(expr) = filter else {
        return Err(nebula_core::Error::Sql(
            "DELETE requires a WHERE clause in this build (safety)".into(),
        ));
    };
    let ids = candidate_ids(backend, db, expr);
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
}
