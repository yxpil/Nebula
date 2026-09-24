//! 数据库门面:把存储文件、内存索引、分词器与 SQL 执行串起来。
//!
//! 打开流程:
//! 1. 打开/创建加密文件(密码校验在存储层完成)
//! 2. 读取索引快照 → 内存索引
//! 3. 之后所有语句经 [`crate::executor`] 执行
//!
//! 持久化策略:
//! - INSERT 批量落索引,达到 `cfg.auto_checkpoint` 或显式 CHECKPOINT 时写快照
//! - DELETE / UPDATE 立即写快照(索引条目即时变化,防止重开后复活)
//! - 关闭时自动 CHECKPOINT,然后 fsync
//!
//! 缓存策略(重复检索免打分、重复读取免解密,见 [`crate::cache`]):
//! - 查询缓存 SEARCH / RELATED 的排序结果;写操作(INSERT/UPDATE/DELETE)
//!   后整体失效(BM25 的 df / avgdl 随写入改变,旧结果不再可信);
//! - 文档缓存 LRU 保存读过的记录,写路径同步 fill / evict;
//! - 打开库时按历史读取热度(随索引快照持久化)预加载热点文档。
//!
//! 所有行为参数(检查点阈值、内容/关键词/关键点上限、分词与停用词、缓存容量)
//! 通过 [`crate::EngineConfig`] 与停用词集合注入,本 crate 不读取配置文件。

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use nebula_core::{codec, MemoryId, MemoryRecord, Result};
#[cfg(test)]
use nebula_core::RecordLocation;
use nebula_tokenizer::{default_stopword_set, ExtractorConfig, TextExtractor};

use nebula_storage::MemoryFile;

use crate::cache::{DocCache, QueryCache};
use crate::config::EngineConfig;
use crate::index::MemoryIndex;

pub struct Database {
    path: PathBuf,
    pub(crate) file: MemoryFile,
    pub(crate) index: MemoryIndex,
    pub(crate) extractor: TextExtractor,
    pub(crate) cfg: EngineConfig,
    /// SEARCH / RELATED 排序结果缓存(写操作后整体失效)。
    pub(crate) query_cache: QueryCache,
    /// 记忆记录 LRU 缓存(读路径自动填充,写路径同步维护)。
    pub(crate) doc_cache: DocCache,
}

impl Database {
    /// 打开已存在的记忆库(使用引擎默认配置与内置停用词表)。
    /// 密码错误返回 [`Error::wrong_password`](nebula_core::Error::wrong_password)。
    pub fn open(path: impl AsRef<Path>, password: &str) -> Result<Self> {
        Self::open_configured(
            path,
            password,
            &EngineConfig::default(),
            &ExtractorConfig::default(),
            &default_stopword_set(),
        )
    }

    /// 打开已存在的记忆库,注入引擎配置、提取配置与停用词表。
    ///
    /// 打开后按历史读取热度把最热的 `engine.cache.hot_preload` 条记忆
    /// 预加载进文档缓存(无热度数据的旧库自然从零开始累计)。
    pub fn open_configured(
        path: impl AsRef<Path>,
        password: &str,
        cfg: &EngineConfig,
        extractor_cfg: &ExtractorConfig,
        stopwords: &HashSet<String>,
    ) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut file = MemoryFile::open(&path, password)?;
        let snap = file.read_index_snapshot()?;
        let (index, stats_present, _heat_present) = MemoryIndex::load_snapshot(&snap)?;
        let mut db = Database {
            path,
            file,
            index,
            extractor: TextExtractor::new(extractor_cfg.clone(), stopwords.clone()),
            cfg: cfg.clone(),
            query_cache: QueryCache::new(cfg.cache.query_cache_capacity),
            doc_cache: DocCache::new(cfg.cache.doc_cache_capacity),
        };
        if !stats_present {
            // 旧版(三段)快照没有检索统计:扫描全部记录重建,
            // 保证 SEARCH / RELATED 对旧库立即可用(之后检查点会持久化)。
            db.rebuild_term_stats()?;
        }
        // 热点预加载:按历史读取热度填充文档缓存,首次查询即可命中。
        db.preload_hot_docs()?;
        Ok(db)
    }

    /// 创建新记忆库(文件必须不存在;使用引擎默认配置与内置停用词表)。
    pub fn create(path: impl AsRef<Path>, password: &str, page_size: u32) -> Result<Self> {
        Self::create_configured(
            path,
            password,
            page_size,
            &EngineConfig::default(),
            &ExtractorConfig::default(),
            &default_stopword_set(),
        )
    }

    /// 创建新记忆库,注入引擎配置、提取配置与停用词表。
    pub fn create_configured(
        path: impl AsRef<Path>,
        password: &str,
        page_size: u32,
        cfg: &EngineConfig,
        extractor_cfg: &ExtractorConfig,
        stopwords: &HashSet<String>,
    ) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = MemoryFile::create(&path, password, page_size)?;
        Ok(Database {
            path,
            file,
            index: MemoryIndex::new(),
            extractor: TextExtractor::new(extractor_cfg.clone(), stopwords.clone()),
            cfg: cfg.clone(),
            query_cache: QueryCache::new(cfg.cache.query_cache_capacity),
            doc_cache: DocCache::new(cfg.cache.doc_cache_capacity),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn memory_count(&self) -> usize {
        self.index.len()
    }

    /// 当前提取配置(展示截断等场景读取上限)。
    pub fn extractor_config(&self) -> &nebula_tokenizer::ExtractorConfig {
        self.extractor.config()
    }

    // ------- 供 executor 使用的内部 API -------

    /// 插入一条记忆(自动提取关键词与关键点),返回分配到的 id。
    pub(crate) fn insert_memory(
        &mut self,
        content: String,
        tags: Vec<String>,
        source: String,
        importance: f32,
    ) -> Result<MemoryId> {
        let mut record = MemoryRecord::new(content);
        record.importance = importance.clamp(0.0, 1.0);
        record.source = source;
        record.tags = tags;

        record.keywords = self.extractor.extract_keywords(&record.content);
        record.key_points = self
            .extractor
            .extract_key_points(&record.content, &record.keywords);

        record.id = self.file.bump_next_record_id();
        record.updated_at = record.created_at;
        let bytes = codec::to_vec(&record);
        let loc = self.file.put_record(&bytes)?;
        let terms = term_counts(&self.extractor.tokenize(&record.content));
        self.index.upsert(&record, &terms, loc);
        self.file.set_memory_count(self.index.len() as u64);
        self.file.commit_catalog()?;

        // 缓存维护:新记录 fill 文档缓存;写操作使 BM25 的 df / avgdl 变化,
        // 查询缓存必须整体失效,否则旧排序不再可信。
        self.doc_cache.put(record.id, record.clone());
        self.query_cache.invalidate();

        if self.file.dirty_records() >= self.cfg.auto_checkpoint {
            self.checkpoint()?;
        }
        Ok(record.id)
    }

    /// 读取一条记忆(经索引定位);命中文档缓存则免去解密读页,并累计读取热度。
    pub(crate) fn fetch_record(&mut self, id: MemoryId) -> Result<Option<MemoryRecord>> {
        let Some(loc) = self.index.location(id).copied() else {
            return Ok(None);
        };
        if let Some(rec) = self.doc_cache.get(id) {
            self.index.bump_heat(id);
            return Ok(Some(rec));
        }
        let bytes = self.file.read_record(&loc)?;
        let record: MemoryRecord = codec::from_slice(&bytes)?;
        self.doc_cache.put(id, record.clone());
        self.index.bump_heat(id);
        Ok(Some(record))
    }

    /// 用新内容替换记录(写新页 → 更新索引 → 释放旧页)。
    pub(crate) fn replace_record(&mut self, record: &MemoryRecord) -> Result<()> {
        let old_loc = self.index.location(record.id).copied();
        let bytes = codec::to_vec(record);
        let loc = self.file.put_record(&bytes)?;
        let terms = term_counts(&self.extractor.tokenize(&record.content));
        self.index.upsert(record, &terms, loc);
        if let Some(old) = old_loc {
            if old != loc {
                self.file.free_record(&old)?;
            }
        }
        self.file.set_memory_count(self.index.len() as u64);
        self.file.commit_catalog()?;
        // 索引位置已变,必须重写快照,否则重开后索引会指向旧页。
        // 缓存维护:文档缓存 fill 新记录;查询缓存整体失效。
        self.doc_cache.put(record.id, record.clone());
        self.query_cache.invalidate();
        self.checkpoint()?;
        Ok(())
    }

    /// 删除一组记忆(释放页 + 清理索引 + 立即写快照)。
    pub(crate) fn delete_ids(&mut self, ids: &[MemoryId]) -> Result<u64> {
        let mut removed = 0u64;
        for id in ids {
            if let Some(loc) = self.index.remove(*id) {
                self.file.free_record(&loc)?;
                self.doc_cache.evict(*id);
                removed += 1;
            }
        }
        if removed > 0 {
            self.file.set_memory_count(self.index.len() as u64);
            self.file.commit_catalog()?;
            self.query_cache.invalidate();
            self.checkpoint()?;
        }
        Ok(removed)
    }

    /// 写索引快照(单页目录提交 = 原子切换)。
    pub(crate) fn checkpoint(&mut self) -> Result<()> {
        let bytes = self.index.encode_snapshot();
        self.file.write_index_snapshot(&bytes)?;
        self.file.commit_catalog()
    }

    /// 关闭:确保快照与目录已持久化。
    pub fn close(&mut self) -> Result<()> {
        self.checkpoint()
    }

    /// 旧版快照升级:扫描全部记录,重建检索统计(词频/文档长度/共现图)。
    ///
    /// 一次性成本:每条记录读一次 + 分词;重建后由检查点持久化,不再需要。
    fn rebuild_term_stats(&mut self) -> Result<()> {
        let ids = self.index.all_ids();
        let mut docs: Vec<(MemoryId, Vec<(String, u32)>, Vec<(String, f32)>)> =
            Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(rec) = self.fetch_record(id)? {
                let terms = term_counts(&self.extractor.tokenize(&rec.content));
                let kws = rec.keywords.iter().map(|k| (k.term.clone(), k.weight)).collect();
                docs.push((id, terms, kws));
            }
        }
        for (id, terms, kws) in docs {
            self.index.set_doc_stats(id, &terms, &kws);
        }
        Ok(())
    }

    /// 打开库后按读取热度预加载热点文档进文档缓存。
    ///
    /// `engine.cache.hot_preload = 0` 或文档缓存关闭时不预加载。
    /// 按热度**升序**插入,保证容量不足时最热的文档最终留在缓存里。
    fn preload_hot_docs(&mut self) -> Result<()> {
        let n = self.cfg.cache.hot_preload;
        if n == 0 {
            return Ok(());
        }
        let hot = self.index.hottest(n);
        for id in hot.into_iter().rev() {
            self.fetch_record(id)?;
        }
        Ok(())
    }

    // ------- 便捷查询 API(不走 SQL,供内部/测试用) -------

    /// 按关键词直接检索(id, 权重, 记录)。
    pub fn search_by_keyword(
        &mut self,
        term: &str,
        limit: usize,
    ) -> Result<Vec<(MemoryId, f32, MemoryRecord)>> {
        let hits = self.index.keyword_hits(term);
        let mut out = Vec::new();
        for (id, weight) in hits.into_iter().take(limit) {
            if let Some(rec) = self.fetch_record(id)? {
                out.push((id, weight, rec));
            }
        }
        Ok(out)
    }

    /// 索引内全部关键词(用于诊断)。
    pub fn all_terms(&self) -> Vec<(String, usize)> {
        self.index.all_terms()
    }

    /// 记录位置(仅供单元测试验证索引与存储一致性)。
    #[cfg(test)]
    pub(crate) fn location_of(&self, id: MemoryId) -> Option<RecordLocation> {
        self.index.location(id).copied()
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        // 尽力落盘;失败时无法在此传播错误,但已尽可能持久化。
        let _ = self.checkpoint();
    }
}

/// 词频统计:tokens → [(词项, 次数)](按词项排序,结果可复现)。
fn term_counts(tokens: &[String]) -> Vec<(String, u32)> {
    let mut counts: BTreeMap<&str, u32> = BTreeMap::new();
    for t in tokens {
        *counts.entry(t.as_str()).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .map(|(t, c)| (t.to_string(), c))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("nebula_eng_{}_{}.ndb", name, std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    const PW: &str = "engine-test-password";

    #[test]
    fn insert_fetch_and_location() {
        let path = tmp("insert");
        let mut db = Database::create(&path, PW, 4096).unwrap();
        let id = db
            .insert_memory(
                " borrowing rules".to_string(),
                vec!["lang".into()],
                "test".into(),
                0.8,
            )
            .unwrap();
        let loc = db.location_of(id).expect("inserted record has a location");
        assert!(loc.page_count >= 1);
        let rec = db.fetch_record(id).unwrap().expect("record readable");
        assert_eq!(rec.content, " borrowing rules");
        assert_eq!(rec.tags, vec!["lang".to_string()]);
        assert!((rec.importance - 0.8).abs() < f32::EPSILON);
        // fetch_record 走索引定位 + 页链读取,位置必须与 location_of 一致
        assert_eq!(db.location_of(id), Some(loc));
        assert!(db.fetch_record(999).unwrap().is_none());
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn search_and_related_via_sql() {
        let path = tmp("search");
        let mut db = Database::create(&path, PW, 4096).unwrap();
        let id1 = db
            .insert_memory(
                "Rust 的所有权机制让内存安全无需 GC,所有权在编译期检查借用。".to_string(),
                vec!["lang".into()],
                "test".into(),
                0.9,
            )
            .unwrap();
        let id2 = db
            .insert_memory(
                "Rust 的借用检查器在编译期保证内存安全,这是所有权的核心规则。".to_string(),
                vec![],
                "test".into(),
                0.5,
            )
            .unwrap();
        let id3 = db
            .insert_memory(
                "今天下午去超市买菜,晚上做饭,顺便取了快递。".to_string(),
                vec![],
                "test".into(),
                0.1,
            )
            .unwrap();

        // SEARCH:两条 rust 记忆应排在生活记忆之前,结果带 score 列
        let r = db.execute("SEARCH 'rust 内存安全' LIMIT 2").unwrap();
        assert_eq!(
            r.columns,
            vec!["id", "score", "content", "keywords", "tags", "importance"]
        );
        assert_eq!(r.rows.len(), 2);
        let top: std::collections::BTreeSet<u64> = r
            .rows
            .iter()
            .map(|row| row[0].parse::<u64>().unwrap())
            .collect();
        let expected: std::collections::BTreeSet<u64> =
            [id1, id2].into_iter().collect();
        assert_eq!(top, expected);
        assert!(!top.contains(&id3));
        // 分数降序且为正
        let s0: f32 = r.rows[0][1].parse().unwrap();
        let s1: f32 = r.rows[1][1].parse().unwrap();
        assert!(s0 >= s1 && s1 > 0.0);

        // RELATED TO:以 id1 为种子,联想到 id2(rust/内存共现),不含自身
        let r = db.execute(&format!("RELATED TO {id1} LIMIT 1")).unwrap();
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0][0], id2.to_string());

        // RELATED '文本':自由文本作种子
        let r = db.execute("RELATED '所有权 编译期'").unwrap();
        assert!(!r.rows.is_empty());
        assert_eq!(r.rows[0][0], id1.to_string());

        // 错误种子 / 无可索引词
        assert!(db.execute("RELATED TO 999").is_err());
        let r = db.execute("SEARCH '的 了 和'").unwrap();
        assert!(r.rows.is_empty(), "全停用词查询应无结果");
        assert_eq!(r.message, "query has no indexable terms (all stopwords or too short?)");

        drop(db);
        // 重开:检索统计随快照恢复,SEARCH 仍然可用
        let mut db2 = Database::open(&path, PW).unwrap();
        let r = db2.execute("SEARCH 'rust' LIMIT 5").unwrap();
        assert_eq!(r.rows.len(), 2);
        let _ = std::fs::remove_file(&path);
    }
}
