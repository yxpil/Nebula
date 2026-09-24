//! SQL 语句执行:INSERT / SELECT / DELETE / UPDATE / 元语句。
//!
//! 检索策略(类 MySQL 的"索引优先,退化全表扫描"):
//! - `keyword =` / `tag =` / `id =` 走内存索引;
//! - 含 `content LIKE` / `importance >` / `source =` 的条件退化为记录扫描;
//! - AND/OR/NOT 组合在候选集上做布尔运算。

use std::collections::BTreeSet;

use nebula_core::{MemoryId, MemoryRecord, Result};
use nebula_sql::ast::{CmpOp, Expr, Literal, SelectColumn, Statement};

use crate::database::Database;
use crate::format::{
    fmt_importance, fmt_key_points, fmt_keywords, fmt_tags, fmt_time, keywords_inline,
};
use crate::index::MemoryIndex;

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
}

impl Database {
    /// 执行一条 SQL,返回可渲染结果。
    pub fn execute(&mut self, sql: &str) -> Result<QueryResult> {
        let stmt = nebula_sql::parse(sql)?;
        self.exec_stmt(&stmt)
    }

    /// 执行分号分隔的语句序列,逐条返回结果(任一条失败即中断并报错)。
    pub fn execute_script(&mut self, sql: &str) -> Result<Vec<QueryResult>> {
        let stmts = nebula_sql::parse_script(sql)?;
        let mut out = Vec::with_capacity(stmts.len());
        for stmt in &stmts {
            out.push(self.exec_stmt(stmt)?);
        }
        Ok(out)
    }

    fn exec_stmt(&mut self, stmt: &Statement) -> Result<QueryResult> {
        match stmt {
            Statement::Insert(ins) => self.exec_insert(ins.clone()),
            Statement::Select(sel) => self.exec_select(sel),
            Statement::Delete(del) => self.exec_delete(del.filter.as_ref()),
            Statement::Update(upd) => self.exec_update(&upd.assignments, upd.filter.as_ref()),
            Statement::Checkpoint => {
                self.checkpoint()?;
                Ok(QueryResult::message("checkpoint done"))
            }
            Statement::ShowTables => Ok(QueryResult {
                columns: vec!["Tables".into()],
                rows: vec![vec!["memories".into()]],
                message: String::new(),
                affected: 0,
            }),
            Statement::ShowStatus => {
                let info = self.file.info();
                let rows = vec![
                    vec!["memories".into(), self.index.len().to_string()],
                    vec!["next_id".into(), self.file.next_record_id().to_string()],
                    vec!["page_size".into(), info.page_size.to_string()],
                    vec!["page_count".into(), info.page_count.to_string()],
                    vec!["free_pages".into(), info.free_pages.to_string()],
                    vec!["checkpoint_seq".into(), info.checkpoint_seq.to_string()],
                    vec!["dirty_records".into(), self.file.dirty_records().to_string()],
                ];
                Ok(QueryResult {
                    columns: vec!["Variable".into(), "Value".into()],
                    rows,
                    message: String::new(),
                    affected: 0,
                })
            }
        }
    }

    // ------- INSERT -------

    fn exec_insert(&mut self, ins: nebula_sql::ast::InsertStmt) -> Result<QueryResult> {
        // 位置默认顺序:content, tags, source, importance
        let mut content = String::new();
        let mut tags = Vec::new();
        let mut source = String::from("sql");
        let mut importance = 0.5f32;

        let cols = if ins.columns.is_empty() {
            vec!["content", "tags", "source", "importance"]
        } else {
            ins.columns.iter().map(|s| s.as_str()).collect::<Vec<_>>()
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
                ("importance", Literal::Int(i)) => importance = (*i as f32).clamp(0.0, 1.0),
                (other, _) => {
                    return Err(nebula_core::Error::Sql(format!(
                        "unknown insert column '{other}'"
                    )))
                }
            }
        }
        if content.trim().is_empty() {
            return Err(nebula_core::Error::Sql("content must not be empty".into()));
        }
        if content.chars().count() > self.cfg.max_content_len {
            return Err(nebula_core::Error::Sql(format!(
                "content exceeds limit of {} chars",
                self.cfg.max_content_len
            )));
        }

        let id = self.insert_memory(content, tags, source, importance)?;
        Ok(QueryResult {
            columns: Vec::new(),
            rows: Vec::new(),
            message: format!("OK, inserted memory id={id}"),
            affected: 1,
        })
    }

    // ------- SELECT -------

    fn exec_select(&mut self, sel: &nebula_sql::ast::SelectStmt) -> Result<QueryResult> {
        // 选择列
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

        // 候选集
        let candidates: Vec<MemoryId> = match &sel.filter {
            Some(expr) => self.candidate_ids(expr),
            None => self.index.all_ids(),
        };

        // 取记录
        let mut records: Vec<MemoryRecord> = Vec::with_capacity(candidates.len());
        for id in candidates {
            if let Some(rec) = self.fetch_record(id)? {
                records.push(rec);
            }
        }

        // 排序
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

        // 展示截断上限取自提取配置(每条记忆实际提取多少就展示多少)。
        let ex_cfg = self.extractor.config().clone();
        let rows = records
            .iter()
            .map(|r| project_row(r, &columns, &ex_cfg))
            .collect();

        Ok(QueryResult {
            columns,
            rows,
            message: String::new(),
            affected: 0,
        })
    }

    /// 计算 WHERE 表达式的候选 id 集。含扫描谓词时退化为全表扫描。
    fn candidate_ids(&mut self, expr: &Expr) -> Vec<MemoryId> {
        if let Some(ids) = index_only_ids(&self.index, expr) {
            return ids;
        }
        // 全表扫描:逐条评估
        let all = self.index.all_ids();
        let mut records: Vec<(MemoryId, MemoryRecord)> = Vec::new();
        for id in all {
            if let Ok(Some(rec)) = self.fetch_record(id) {
                records.push((id, rec));
            }
        }
        records
            .into_iter()
            .filter(|(_, r)| eval_record(expr, r))
            .map(|(id, _)| id)
            .collect()
    }

    // ------- DELETE -------

    fn exec_delete(&mut self, filter: Option<&Expr>) -> Result<QueryResult> {
        let ids: Vec<MemoryId> = match filter {
            Some(expr) => self.candidate_ids(expr),
            None => {
                return Err(nebula_core::Error::Sql(
                    "DELETE requires a WHERE clause in this build (safety)".into(),
                ))
            }
        };
        let count = self.delete_ids(&ids)?;
        Ok(QueryResult {
            columns: Vec::new(),
            rows: Vec::new(),
            message: format!("OK, deleted {count} memories"),
            affected: count,
        })
    }

    // ------- UPDATE -------

    fn exec_update(
        &mut self,
        assignments: &[(String, Literal)],
        filter: Option<&Expr>,
    ) -> Result<QueryResult> {
        let ids: Vec<MemoryId> = match filter {
            Some(expr) => self.candidate_ids(expr),
            None => self.index.all_ids(),
        };
        let mut updated = 0u64;
        for id in ids {
            if let Some(mut rec) = self.fetch_record(id)? {
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
                    self.replace_record(&rec)?;
                    updated += 1;
                }
            }
        }
        Ok(QueryResult {
            columns: Vec::new(),
            rows: Vec::new(),
            message: format!("OK, updated {updated} memories"),
            affected: updated,
        })
    }
}

/// 若表达式只涉及索引可评估的谓词(关键词/标签/id),返回 id 集(已排序)。
fn index_only_ids(index: &MemoryIndex, expr: &Expr) -> Option<Vec<MemoryId>> {
    match expr {
        Expr::KeywordEq(t) => Some(index.keyword_hits(t).into_iter().map(|(id, _)| id).collect()),
        Expr::TagEq(t) => Some(index.tag_hits(t)),
        Expr::IdEq(id) => index.location(*id).map(|_| vec![*id]),
        Expr::And(a, b) => {
            let x = index_only_ids(index, a)?;
            let y = index_only_ids(index, b)?;
            Some(intersect_sorted(x, y))
        }
        Expr::Or(a, b) => {
            let x = index_only_ids(index, a)?;
            let y = index_only_ids(index, b)?;
            let mut set: BTreeSet<MemoryId> = x.into_iter().collect();
            set.extend(y);
            Some(set.into_iter().collect())
        }
        Expr::Not(inner) => {
            let hit = index_only_ids(index, inner)?;
            let excluded: BTreeSet<MemoryId> = hit.into_iter().collect();
            Some(
                index
                    .all_ids()
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

/// 在完整记录上评估表达式(扫描路径)。
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

/// 按列清单投影一行(关键词/关键点列按提取配置上限截断展示)。
fn project_row(
    rec: &MemoryRecord,
    columns: &[String],
    ex_cfg: &nebula_tokenizer::ExtractorConfig,
) -> Vec<String> {
    columns
        .iter()
        .map(|col| match col.as_str() {
            "id" => rec.id.to_string(),
            "content" => rec.content.clone(),
            "key_points" => fmt_key_points(&rec.key_points, ex_cfg.max_key_points),
            "keywords" => fmt_keywords(&rec.keywords, ex_cfg.max_keywords),
            "keywords_inline" => keywords_inline(&rec.keywords, ex_cfg.max_keywords),
            "tags" => fmt_tags(&rec.tags),
            "source" => rec.source.clone(),
            "importance" => fmt_importance(rec.importance),
            "created_at" => fmt_time(rec.created_at),
            "updated_at" => fmt_time(rec.updated_at),
            other => format!("<unknown column '{other}'>"),
        })
        .collect()
}
