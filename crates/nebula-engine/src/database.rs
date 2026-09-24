//! 数据库门面:把存储文件、按库分片的内存索引、分词器与 SQL 执行串起来。
//!
//! 打开流程:
//! 1. 打开/创建加密文件(密码校验在存储层完成)
//! 2. 读取索引快照 → 按库分片的内存索引(旧版单库快照整体归入 "main")
//! 3. 确保默认库 "main" 始终存在
//! 4. 之后所有语句经 [`crate::executor`] 执行
//!
//! 多库模型(单文件内多逻辑库,MySQL database 风格):
//! - [`Database::create_database`] / [`Database::drop_database`] 建删逻辑库;
//! - 每个库 id 从 1 独立自增,倒排 / 共现图 / 热度按库隔离;
//! - [`Database::use_database`] 切换工作库,并按该库历史热度预加载热点文档。
//!
//! 持久化策略:
//! - INSERT 批量落索引,达到 `cfg.auto_checkpoint` 或显式 CHECKPOINT 时写快照
//! - DELETE / UPDATE 立即写快照(索引条目即时变化,防止重开后复活)
//! - 关闭时自动 CHECKPOINT,然后 fsync
//!
//! 缓存策略(重复检索免打分、重复读取免解密,见 [`crate::cache`]):
//! - 查询缓存 SEARCH / RELATED 的排序结果(键含库列表);写操作后整体失效;
//! - 文档缓存 LRU 保存读过的记录(键为 (库, id)),写路径同步 fill / evict。
//!
//! 所有行为参数(检查点阈值、内容/关键词/关键点上限、分词与停用词、缓存容量)
//! 通过 [`crate::EngineConfig`] 与停用词集合注入,本 crate 不读取配置文件。

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use nebula_core::{codec, MemoryId, MemoryRecord, Result, DEFAULT_DB};
#[cfg(test)]
use nebula_core::RecordLocation;
use nebula_tokenizer::{default_stopword_set, ExtractorConfig, TextExtractor};

use nebula_storage::MemoryFile;

use crate::backend::{MemBackend, RankedRow};
use crate::cache::{DocCache, QueryCache};
use crate::config::EngineConfig;
use crate::index::MemoryIndex;

/// 逻辑库名最大长度(标识符语法规则属代码语义,非可调参数)。
pub const MAX_DB_NAME_LEN: usize = 64;

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
    /// 持久化延迟开关:事务期间为 true,跳过 catalog 提交与快照写入。
    pub(crate) persistence_deferred: bool,
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
    /// 打开后确保默认库存在,并按默认库的历史读取热度把最热的
    /// `engine.cache.hot_preload` 条记忆预加载进文档缓存;其它库在
    /// USE 时按需预热。
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
            persistence_deferred: false,
        };
        // 仅空索引(空快照/空文件)补默认库;已有库的文件原样打开,
        // 避免把 main 强塞进集群的单库受管文件。
        if db.index.db_names().is_empty() {
            db.ensure_default_db();
        }
        if !stats_present {
            // 旧版(三段)快照没有检索统计:扫描全部库的全部记录重建,
            // 保证 SEARCH / RELATED 对旧库立即可用(之后检查点会持久化)。
            db.rebuild_term_stats()?;
        }
        // 热点预加载:默认库优先,单库文件则预热其唯一库。
        let preload_db = if db.index.has_db(DEFAULT_DB) {
            Some(DEFAULT_DB.to_string())
        } else {
            db.index.db_names().into_iter().next()
        };
        if let Some(name) = preload_db {
            db.preload_hot_docs(&name)?;
        }
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
        Self::create_with_initial_db(
            path,
            password,
            page_size,
            DEFAULT_DB,
            cfg,
            extractor_cfg,
            stopwords,
        )
    }

    /// 创建只含指定单个逻辑库的新文件(集群受管文件用;initial_db 必须
    /// 是合法库名;文件内不会再额外创建 main)。
    pub fn create_configured_single(
        path: impl AsRef<Path>,
        password: &str,
        page_size: u32,
        initial_db: &str,
        cfg: &EngineConfig,
        extractor_cfg: &ExtractorConfig,
        stopwords: &HashSet<String>,
    ) -> Result<Self> {
        Self::create_with_initial_db(
            path,
            password,
            page_size,
            initial_db,
            cfg,
            extractor_cfg,
            stopwords,
        )
    }

    fn create_with_initial_db(
        path: impl AsRef<Path>,
        password: &str,
        page_size: u32,
        initial_db: &str,
        cfg: &EngineConfig,
        extractor_cfg: &ExtractorConfig,
        stopwords: &HashSet<String>,
    ) -> Result<Self> {
        let initial = normalize_db_name(initial_db)?;
        let path = path.as_ref().to_path_buf();
        let file = MemoryFile::create(&path, password, page_size)?;
        let mut index = MemoryIndex::new();
        index.create_db(&initial);
        Ok(Database {
            path,
            file,
            index,
            extractor: TextExtractor::new(extractor_cfg.clone(), stopwords.clone()),
            cfg: cfg.clone(),
            query_cache: QueryCache::new(cfg.cache.query_cache_capacity),
            doc_cache: DocCache::new(cfg.cache.doc_cache_capacity),
            persistence_deferred: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 全部库的记忆总数。
    pub fn memory_count(&self) -> usize {
        self.index.total_len()
    }

    /// 某库的记忆数(库不存在为 0)。
    pub fn memory_count_in(&self, db: &str) -> usize {
        self.index.len(db)
    }

    /// 当前提取配置(展示截断等场景读取上限)。
    pub fn extractor_config(&self) -> &nebula_tokenizer::ExtractorConfig {
        self.extractor.config()
    }

    // ------- 逻辑库管理 -------

    /// 全部逻辑库名(按字典序升序)。
    pub fn database_names(&self) -> Vec<String> {
        self.index.db_names()
    }

    /// 是否存在某逻辑库。
    pub fn has_database(&self, db: &str) -> bool {
        self.index.has_db(db)
    }

    /// 创建逻辑库;库已存在时返回 false(不覆盖)。
    /// 库名规则:小写化后 1~64 字符,字母/下划线开头,仅含字母、数字、下划线。
    pub fn create_database(&mut self, db: &str) -> Result<bool> {
        let name = normalize_db_name(db)?;
        let created = self.index.create_db(&name);
        if created {
            // 立即落快照:库结构变化不能等到自动检查点。
            self.query_cache.invalidate();
            self.checkpoint()?;
        }
        Ok(created)
    }

    /// 删除逻辑库及其全部记忆(释放数据页 + 清理索引 + 立即写快照)。
    /// 默认库 "main" 受保护不允许删除(旧库升级与默认连接的锚点)。
    pub fn drop_database(&mut self, db: &str) -> Result<()> {
        let name = normalize_db_name(db)?;
        if name == DEFAULT_DB {
            return Err(nebula_core::Error::Engine(
                "cannot drop the protected default database 'main'".into(),
            ));
        }
        let Some(locs) = self.index.drop_db(&name) else {
            return Err(nebula_core::Error::Engine(format!(
                "unknown database '{name}'"
            )));
        };
        for loc in &locs {
            self.file.free_record(loc)?;
        }
        self.file.set_memory_count(self.index.total_len() as u64);
        self.file.commit_catalog()?;
        // 库整体消失:文档缓存中该库的条目全部清除,查询缓存整体失效。
        self.doc_cache.evict_db(&name);
        self.query_cache.invalidate();
        self.checkpoint()?;
        Ok(())
    }

    /// 切换工作库:库必须存在,并按该库的历史读取热度预加载热点文档。
    pub fn use_database(&mut self, db: &str) -> Result<()> {
        let name = normalize_db_name(db)?;
        if !self.index.has_db(&name) {
            return Err(nebula_core::Error::Engine(format!(
                "unknown database '{name}'"
            )));
        }
        // 切换库后首批查询即可命中文档缓存(容量为 0 / hot_preload=0 时为空操作)。
        self.preload_hot_docs(&name)?;
        Ok(())
    }

    /// 默认库缺失时补建(打开旧版空库 / 空快照场景)。
    fn ensure_default_db(&mut self) {
        if !self.index.has_db(DEFAULT_DB) {
            self.index.create_db(DEFAULT_DB);
        }
    }

    // ------- 供 executor 使用的内部 API -------

    /// 向指定库插入一条记忆(自动提取关键词与关键点),返回分配到的 id。
    pub(crate) fn insert_memory(
        &mut self,
        db: &str,
        content: String,
        tags: Vec<String>,
        source: String,
        importance: f32,
    ) -> Result<MemoryId> {
        if !self.index.has_db(db) {
            return Err(nebula_core::Error::Engine(format!(
                "unknown database '{db}'"
            )));
        }
        let mut record = MemoryRecord::new(content).with_db(db);
        record.importance = importance.clamp(0.0, 1.0);
        record.source = source;
        record.tags = tags;

        record.keywords = self.extractor.extract_keywords(&record.content);
        record.key_points = self
            .extractor
            .extract_key_points(&record.content, &record.keywords);

        record.id = self.index.allocate_id(db);
        record.updated_at = record.created_at;
        let bytes = codec::to_vec(&record);
        let loc = self.file.put_record(&bytes)?;
        let terms = term_counts(&self.extractor.tokenize(&record.content));
        self.index.upsert(db, &record, &terms, loc);
        self.file.set_memory_count(self.index.total_len() as u64);
        // 事务期间(持久化延迟)只改内存,catalog 不落盘。
        if !self.persistence_deferred {
            self.file.commit_catalog()?;
        }

        // 缓存维护:新记录 fill 文档缓存;写操作使 BM25 的 df / avgdl 变化,
        // 查询缓存必须整体失效,否则旧排序不再可信。
        self.doc_cache.put(db, record.id, record.clone());
        self.query_cache.invalidate();

        if !self.persistence_deferred && self.file.dirty_records() >= self.cfg.auto_checkpoint {
            self.checkpoint()?;
        }
        Ok(record.id)
    }

    /// 读取某库的一条记忆(经索引定位);命中文档缓存则免去解密读页,并累计读取热度。
    pub(crate) fn fetch_record(
        &mut self,
        db: &str,
        id: MemoryId,
    ) -> Result<Option<MemoryRecord>> {
        let Some(loc) = self.index.location(db, id).copied() else {
            return Ok(None);
        };
        if let Some(rec) = self.doc_cache.get(db, id) {
            self.index.bump_heat(db, id);
            return Ok(Some(rec));
        }
        let bytes = self.file.read_record(&loc)?;
        let record: MemoryRecord = codec::from_slice(&bytes)?;
        self.doc_cache.put(db, id, record.clone());
        self.index.bump_heat(db, id);
        Ok(Some(record))
    }

    /// 用新内容替换指定库的记录(写新页 → 更新索引 → 释放旧页)。
    pub(crate) fn replace_record(
        &mut self,
        db: &str,
        record: &MemoryRecord,
    ) -> Result<()> {
        if record.db != db {
            return Err(nebula_core::Error::Engine(format!(
                "record belongs to database '{}', cannot write it into '{db}'",
                record.db
            )));
        }
        let old_loc = self.index.location(db, record.id).copied();
        let bytes = codec::to_vec(record);
        let loc = self.file.put_record(&bytes)?;
        let terms = term_counts(&self.extractor.tokenize(&record.content));
        self.index.upsert(db, record, &terms, loc);
        if let Some(old) = old_loc {
            if old != loc {
                self.file.free_record(&old)?;
            }
        }
        self.file.set_memory_count(self.index.total_len() as u64);
        if !self.persistence_deferred {
            self.file.commit_catalog()?;
        }
        // 索引位置已变,必须重写快照,否则重开后索引会指向旧页。
        // 缓存维护:文档缓存 fill 新记录;查询缓存整体失效。
        self.doc_cache.put(db, record.id, record.clone());
        self.query_cache.invalidate();
        if !self.persistence_deferred {
            self.checkpoint()?;
        }
        Ok(())
    }

    /// 删除指定库的一组记忆(释放页 + 清理索引 + 立即写快照)。
    pub(crate) fn delete_ids(&mut self, db: &str, ids: &[MemoryId]) -> Result<u64> {
        let mut removed = 0u64;
        for id in ids {
            if let Some(loc) = self.index.remove(db, *id) {
                self.file.free_record(&loc)?;
                self.doc_cache.evict(db, *id);
                removed += 1;
            }
        }
        if removed > 0 {
            self.file.set_memory_count(self.index.total_len() as u64);
            if !self.persistence_deferred {
                self.file.commit_catalog()?;
            }
            self.query_cache.invalidate();
            if !self.persistence_deferred {
                self.checkpoint()?;
            }
        }
        Ok(removed)
    }

    /// 写索引快照(单页目录提交 = 原子切换)。
    pub(crate) fn checkpoint(&mut self) -> Result<()> {
        // 延迟持久化期间禁止落盘(executor 正常会在 COMMIT/ROLLBACK 后调用,
        // 此处防止任何路径绕过)。
        if self.persistence_deferred {
            return Err(nebula_core::Error::Engine(
                "cannot checkpoint while persistence is deferred (run COMMIT or ROLLBACK first)"
                    .into(),
            ));
        }
        let bytes = self.index.encode_snapshot();
        self.file.write_index_snapshot(&bytes)?;
        self.file.commit_catalog()
    }

    /// 写入任意快照字节(覆盖式;供 nebula-cluster 的 _admin.ndb 使用)。
    pub fn write_aux_snapshot(&mut self, bytes: &[u8]) -> Result<()> {
        self.file.write_index_snapshot(bytes)
    }

    /// 读取快照字节。
    pub fn read_aux_snapshot(&mut self) -> Result<Vec<u8>> {
        self.file.read_index_snapshot()
    }

    /// 关闭:确保快照与目录已持久化。
    pub fn close(&mut self) -> Result<()> {
        self.checkpoint()
    }

    /// 旧版快照升级:扫描全部库的全部记录,重建检索统计(词频/文档长度/共现图)。
    ///
    /// 一次性成本:每条记录读一次 + 分词;重建后由检查点持久化,不再需要。
    fn rebuild_term_stats(&mut self) -> Result<()> {
        let names = self.index.db_names();
        // 先收集再回写,避免借用冲突;fetch_record 会同时累计热度。
        let mut docs: Vec<(String, MemoryId, Vec<(String, u32)>, Vec<(String, f32)>)> =
            Vec::new();
        for name in &names {
            for id in self.index.all_ids(name) {
                if let Some(rec) = self.fetch_record(name, id)? {
                    let terms = term_counts(&self.extractor.tokenize(&rec.content));
                    let kws = rec.keywords.iter().map(|k| (k.term.clone(), k.weight)).collect();
                    docs.push((name.clone(), id, terms, kws));
                }
            }
        }
        for (name, id, terms, kws) in docs {
            self.index.set_doc_stats(&name, id, &terms, &kws);
        }
        Ok(())
    }

    /// 按某库的读取热度预加载热点文档进文档缓存。
    ///
    /// `engine.cache.hot_preload = 0` 或文档缓存关闭时不预加载。
    /// 按热度**升序**插入,保证容量不足时最热的文档最终留在缓存里。
    fn preload_hot_docs(&mut self, db: &str) -> Result<()> {
        let n = self.cfg.cache.hot_preload;
        if n == 0 || !self.index.has_db(db) {
            return Ok(());
        }
        let hot = self.index.hottest(db, n);
        for id in hot.into_iter().rev() {
            self.fetch_record(db, id)?;
        }
        Ok(())
    }

    // ------- 便捷查询 API(不走 SQL,供内部/测试用) -------

    /// 按关键词直接检索某库(id, 权重, 记录)。
    pub fn search_by_keyword(
        &mut self,
        db: &str,
        term: &str,
        limit: usize,
    ) -> Result<Vec<(MemoryId, f32, MemoryRecord)>> {
        let hits = self.index.keyword_hits(db, term);
        let mut out = Vec::new();
        for (id, weight) in hits.into_iter().take(limit) {
            if let Some(rec) = self.fetch_record(db, id)? {
                out.push((id, weight, rec));
            }
        }
        Ok(out)
    }

    /// 某库索引内全部关键词(用于诊断)。
    pub fn all_terms(&self, db: &str) -> Vec<(String, usize)> {
        self.index.all_terms(db)
    }

    /// 记录位置(仅供单元测试验证索引与存储一致性)。
    #[cfg(test)]
    pub(crate) fn location_of(&self, db: &str, id: MemoryId) -> Option<RecordLocation> {
        self.index.location(db, id).copied()
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        // 尽力落盘;失败时无法在此传播错误,但已尽可能持久化。
        let _ = self.checkpoint();
    }
}

// ------- MemBackend:单文件后端实现 -------

impl MemBackend for Database {
    fn list_dbs(&self) -> Vec<String> {
        self.database_names()
    }

    fn db_exists(&self, db: &str) -> bool {
        self.has_database(db)
    }

    fn create_db(&mut self, db: &str) -> Result<bool> {
        self.create_database(db)
    }

    fn drop_db(&mut self, db: &str) -> Result<()> {
        self.drop_database(db)
    }

    fn attach_file(&mut self, _path: &str, _name: &str) -> Result<()> {
        Err(nebula_core::Error::Engine(
            "ATTACH is supported only in directory (cluster) mode (open with --dir)".into(),
        ))
    }

    fn detach_db(&mut self, _name: &str) -> Result<()> {
        Err(nebula_core::Error::Engine(
            "DETACH is supported only in directory (cluster) mode (--dir)".into(),
        ))
    }

    fn on_use(&mut self, db: &str) -> Result<()> {
        self.preload_hot_docs(db)
    }

    fn insert_mem(
        &mut self,
        db: &str,
        content: String,
        tags: Vec<String>,
        source: String,
        importance: f32,
    ) -> Result<MemoryId> {
        self.insert_memory(db, content, tags, source, importance)
    }

    fn fetch_mem(&mut self, db: &str, id: MemoryId) -> Result<Option<MemoryRecord>> {
        self.fetch_record(db, id)
    }

    fn replace_mem(&mut self, db: &str, record: &MemoryRecord) -> Result<()> {
        self.replace_record(db, record)
    }

    fn delete_mems(&mut self, db: &str, ids: &[MemoryId]) -> Result<u64> {
        self.delete_ids(db, ids)
    }

    fn keyword_hits(&self, db: &str, term: &str) -> Vec<(MemoryId, f32)> {
        self.index.keyword_hits(db, term)
    }

    fn tag_hits(&self, db: &str, tag: &str) -> Vec<MemoryId> {
        self.index.tag_hits(db, tag)
    }

    fn all_ids(&self, db: &str) -> Vec<MemoryId> {
        self.index.all_ids(db)
    }

    fn search_ranked(
        &mut self,
        dbs: &[String],
        query: &str,
        limit: usize,
    ) -> Result<Vec<RankedRow>> {
        self.search_ranked_rows(dbs, query, limit)
    }

    fn related_ranked(
        &mut self,
        dbs: &[String],
        seed: nebula_sql::ast::RelatedSeed,
        limit: usize,
    ) -> Result<Vec<RankedRow>> {
        self.related_ranked_rows(dbs, seed, limit)
    }

    fn cache_rows(&self) -> Vec<Vec<String>> {
        let q = &self.query_cache;
        let d = &self.doc_cache;
        vec![
            vec!["query_cache_capacity".into(), q.capacity().to_string()],
            vec!["query_cache_entries".into(), q.len().to_string()],
            vec!["query_cache_hits".into(), q.hits().to_string()],
            vec!["query_cache_misses".into(), q.misses().to_string()],
            vec!["query_cache_evictions".into(), q.evictions().to_string()],
            vec!["query_cache_epoch".into(), q.epoch().to_string()],
            vec!["doc_cache_capacity".into(), d.capacity().to_string()],
            vec!["doc_cache_entries".into(), d.len().to_string()],
            vec!["doc_cache_hits".into(), d.hits().to_string()],
            vec!["doc_cache_misses".into(), d.misses().to_string()],
            vec!["doc_cache_evictions".into(), d.evictions().to_string()],
            vec![
                "hot_tracked".into(),
                self.index.heat_len_total().to_string(),
            ],
        ]
    }

    fn clear_caches(&mut self) {
        self.query_cache.clear();
    }

    fn set_cache_capacity(&mut self, target: nebula_sql::ast::CacheTarget, capacity: usize) {
        match target {
            nebula_sql::ast::CacheTarget::Query => {
                self.query_cache.set_capacity(capacity);
            }
            nebula_sql::ast::CacheTarget::Doc => {
                self.doc_cache.set_capacity(capacity);
            }
        }
    }

    fn hot_rows(&self, db: &str, limit: usize) -> Vec<Vec<String>> {
        self.index
            .hottest(db, limit)
            .into_iter()
            .map(|id| {
                vec![
                    id.to_string(),
                    self.index.heat_of(db, id).to_string(),
                    self.doc_cache.contains(db, id).to_string(),
                ]
            })
            .collect()
    }

    fn status_rows(&self) -> Vec<Vec<String>> {
        let info = self.file.info();
        vec![
            vec![
                "databases".into(),
                self.index.db_names().join(","),
            ],
            vec!["memories".into(), self.index.total_len().to_string()],
            vec!["page_size".into(), info.page_size.to_string()],
            vec!["page_count".into(), info.page_count.to_string()],
            vec!["free_pages".into(), info.free_pages.to_string()],
            vec!["checkpoint_seq".into(), info.checkpoint_seq.to_string()],
            vec![
                "dirty_records".into(),
                self.file.dirty_records().to_string(),
            ],
        ]
    }

    fn checkpoint(&mut self) -> Result<()> {
        Database::checkpoint(self)
    }

    fn set_persistence_deferred(&mut self, deferred: bool) {
        self.persistence_deferred = deferred;
    }
}

impl Database {
    /// SEARCH 实现:查询词向量 → 逐库三层打分 → 全局合并排序;
    /// 排序结果按 (库列表, 查询, limit) 缓存。
    fn search_ranked_rows(
        &mut self,
        dbs: &[String],
        query: &str,
        limit: usize,
    ) -> Result<Vec<RankedRow>> {
        let qv: Vec<(String, f32)> = self
            .extractor
            .extract_keywords(query)
            .into_iter()
            .map(|k| (k.term, k.weight))
            .collect();
        if qv.is_empty() {
            return Ok(Vec::new());
        }
        let key = crate::cache::search_key(dbs, query, limit);
        if let Some(list) = self.query_cache.get(&key) {
            return Ok(cache_list_to_rows(list));
        }
        let cfg = self.cfg.search.clone();
        let mut all = Vec::new();
        for db in dbs {
            all.extend(self.rank_in_db(db, &qv, &[], &cfg)?);
        }
        sort_ranked(&mut all);
        all.truncate(limit);
        self.query_cache
            .put(key.clone(), rows_to_cache_list(&all));
        Ok(all)
    }

    /// RELATED 实现:解析种子词向量(种子自身排除)→ 逐库打分 → 合并。
    fn related_ranked_rows(
        &mut self,
        dbs: &[String],
        seed: nebula_sql::ast::RelatedSeed,
        limit: usize,
    ) -> Result<Vec<RankedRow>> {
        use nebula_sql::ast::RelatedSeed::*;
        // 种子库:Id 用 dbs 首库(executor 已把裸 id 转成当前库限定名,
        // 这里仅作兜底);QualifiedId 用其声明库。
        let (seed_db, seed_id): (Option<String>, Option<MemoryId>) = match &seed {
            Id(id) => (dbs.first().cloned(), Some(*id)),
            QualifiedId(db, id) => (Some(db.clone()), Some(*id)),
            Text(_) => (None, None),
        };
        let seed_vec: Vec<(String, f32)> = match (seed_db.as_ref(), seed_id) {
            (Some(db), Some(id)) => {
                let Some(rec) = self.fetch_record(db, id)? else {
                    return Err(nebula_core::Error::Sql(format!(
                        "RELATED TO: no memory {db}.{id}"
                    )));
                };
                rec.keywords
                    .iter()
                    .map(|k| (k.term.clone(), k.weight))
                    .collect()
            }
            _ => match &seed {
                Text(text) => self
                    .extractor
                    .extract_keywords(text)
                    .into_iter()
                    .map(|k| (k.term, k.weight))
                    .collect(),
                _ => unreachable!(),
            },
        };
        if seed_vec.is_empty() {
            return Ok(Vec::new());
        }
        let key = match seed_id {
            Some(id) => crate::cache::related_id_key(dbs, id, limit),
            None => match &seed {
                Text(text) => crate::cache::related_text_key(dbs, text, limit),
                _ => unreachable!(),
            },
        };
        if let Some(list) = self.query_cache.get(&key) {
            return Ok(cache_list_to_rows(list));
        }
        let cfg = self.cfg.search.clone();
        let mut all = Vec::new();
        for db in dbs {
            // 仅种子所在库排除种子自身
            let excludes: Vec<MemoryId> = seed_id
                .filter(|_| seed_db.as_deref() == Some(db.as_str()))
                .into_iter()
                .collect();
            all.extend(self.rank_in_db(db, &seed_vec, &excludes, &cfg)?);
        }
        sort_ranked(&mut all);
        all.truncate(limit);
        self.query_cache.put(key, rows_to_cache_list(&all));
        Ok(all)
    }

    /// 单库三层打分(逻辑同旧版 rank):共现图扩展 → BM25 → 种子余弦 → 联合重排。
    fn rank_in_db(
        &self,
        db: &str,
        query: &[(String, f32)],
        exclude: &[MemoryId],
        cfg: &crate::config::SearchConfig,
    ) -> Result<Vec<RankedRow>> {
        if !self.index.has_db(db) {
            return Err(nebula_core::Error::Engine(format!(
                "unknown database '{db}'"
            )));
        }
        // 第一层:共现图扩展,追加查询词
        let mut q_all: Vec<(String, f32)> = query.to_vec();
        for (term, w) in self.index.expand(db, query, cfg) {
            if !q_all.iter().any(|(t, _)| t == &term) {
                q_all.push((term, w));
            }
        }
        let terms: Vec<&str> = q_all.iter().map(|(t, _)| t.as_str()).collect();
        let candidates = self.index.docs_with_terms(db, terms);
        let bm25 = self.index.bm25(db, &q_all, cfg);
        let mut out = Vec::with_capacity(candidates.len());
        for id in candidates {
            if exclude.contains(&id) {
                continue;
            }
            let cos = if query.is_empty() {
                0.0
            } else {
                self.index.cosine(db, query, id)
            };
            let score = bm25.get(&id).copied().unwrap_or(0.0)
                + cfg.similarity_weight * cos;
            if score >= cfg.min_score {
                out.push(RankedRow::new(db, id, score));
            }
        }
        Ok(out)
    }
}

/// 相关度全局降序,同分时 (库, id) 升序,保证跨库结果可复现。
fn sort_ranked(rows: &mut Vec<RankedRow>) {
    rows.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.db.cmp(&b.db))
            .then_with(|| a.id.cmp(&b.id))
    });
}

fn rows_to_cache_list(rows: &[RankedRow]) -> Vec<(String, MemoryId, f32)> {
    rows.iter()
        .map(|r| (r.db.clone(), r.id, r.score))
        .collect()
}

fn cache_list_to_rows(list: Vec<(String, MemoryId, f32)>) -> Vec<RankedRow> {
    list.into_iter()
        .map(|(db, id, score)| RankedRow::new(db, id, score))
        .collect()
}

/// 库名归一化:小写化并校验标识符语法,返回规范库名。
///
/// 库名大小写不敏感(与 SQL 标识符一致):1~64 字符,字母/下划线开头,
/// 仅含字母、数字、下划线。
pub fn normalize_db_name(db: &str) -> Result<String> {
    let name = db.trim().to_lowercase();
    if name.is_empty() || name.len() > MAX_DB_NAME_LEN {
        return Err(nebula_core::Error::Engine(format!(
            "database name must be 1..{MAX_DB_NAME_LEN} characters"
        )));
    }
    let mut chars = name.chars();
    // 前置 is_empty 已保证有首字符;此处防御性处理,不依赖 unwrap。
    let Some(first) = chars.next() else {
        return Err(nebula_core::Error::Engine(
            "database name must not be empty".into(),
        ));
    };
    if !first.is_ascii_alphabetic() && first != '_' {
        return Err(nebula_core::Error::Engine(
            "database name must start with a letter or underscore".into(),
        ));
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(nebula_core::Error::Engine(
            "database name may contain only letters, digits and underscores".into(),
        ));
    }
    Ok(name)
}

/// 单文件模式的用户目录语义:只认内置 admin,任何库上拥有全部权限。
///
/// 与 [`crate::auth::FullAccess`] 行为一致,但直接实现在 Database 上,
/// 使单文件模式自身即满足 SessionBackend(executor 只需一个对象)。
impl crate::auth::UserDirectory for Database {
    fn user_exists(&self, user: &str) -> bool {
        user == crate::auth::DEFAULT_USER
    }

    fn has_priv(&self, user: &str, _db: &str, _priv_: crate::auth::Privilege) -> bool {
        user == crate::auth::DEFAULT_USER
    }

    fn is_admin(&self, user: &str) -> bool {
        user == crate::auth::DEFAULT_USER
    }

    fn create_user(
        &mut self,
        _user: &str,
        _password: &str,
        _if_not_exists: bool,
    ) -> Result<bool> {
        Err(nebula_core::Error::Engine(
            "user management requires directory mode (open a cluster directory with --dir)"
                .into(),
        ))
    }

    fn drop_user(&mut self, _user: &str, _if_exists: bool) -> Result<bool> {
        Err(nebula_core::Error::Engine(
            "user management requires directory mode (--dir)".into(),
        ))
    }

    fn alter_user(&mut self, _user: &str, _password: &str) -> Result<()> {
        Err(nebula_core::Error::Engine(
            "user management requires directory mode (--dir)".into(),
        ))
    }

    fn grant(
        &mut self,
        _user: &str,
        _object: &nebula_sql::ast::GrantObject,
        _privs: &[crate::auth::Privilege],
    ) -> Result<()> {
        Err(nebula_core::Error::Engine(
            "GRANT requires directory mode (--dir)".into(),
        ))
    }

    fn revoke(
        &mut self,
        _user: &str,
        _object: &nebula_sql::ast::GrantObject,
        _privs: &[crate::auth::Privilege],
    ) -> Result<()> {
        Err(nebula_core::Error::Engine(
            "REVOKE requires directory mode (--dir)".into(),
        ))
    }

    fn list_users(&self) -> Vec<String> {
        vec![crate::auth::DEFAULT_USER.into()]
    }

    fn list_grants(
        &self,
        user: &str,
    ) -> Vec<(nebula_sql::ast::GrantObject, Vec<crate::auth::Privilege>)> {
        use crate::auth::Privilege;
        if user != crate::auth::DEFAULT_USER {
            return Vec::new();
        }
        vec![(
            nebula_sql::ast::GrantObject::AllDatabases,
            vec![Privilege::Read, Privilege::Write, Privilege::Admin],
        )]
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
    fn db_name_normalization_rules() {
        assert_eq!(normalize_db_name("Work").unwrap(), "work");
        assert_eq!(normalize_db_name("  My_DB2 ").unwrap(), "my_db2");
        assert_eq!(normalize_db_name("_tmp").unwrap(), "_tmp");
        assert!(normalize_db_name("").is_err());
        assert!(normalize_db_name("2work").is_err(), "不能数字开头");
        assert!(normalize_db_name("my-db").is_err(), "不允许连字符");
        assert!(normalize_db_name("my.db").is_err(), "不允许点号");
        assert!(normalize_db_name(&"x".repeat(65)).is_err(), "超长");
    }

    #[test]
    fn insert_fetch_and_location_per_db() {
        let path = tmp("insert");
        let mut db = Database::create(&path, PW, 4096).unwrap();
        // 新库自带默认库 main
        assert_eq!(db.database_names(), vec![DEFAULT_DB]);
        let id = db
            .insert_memory(
                DEFAULT_DB,
                " borrowing rules".to_string(),
                vec!["lang".into()],
                "test".into(),
                0.8,
            )
            .unwrap();
        let loc = db
            .location_of(DEFAULT_DB, id)
            .expect("inserted record has a location");
        assert!(loc.page_count >= 1);
        let rec = db
            .fetch_record(DEFAULT_DB, id)
            .unwrap()
            .expect("record readable");
        assert_eq!(rec.db, DEFAULT_DB);
        assert_eq!(rec.content, " borrowing rules");
        assert_eq!(rec.tags, vec!["lang".to_string()]);
        assert!((rec.importance - 0.8).abs() < f32::EPSILON);
        // fetch_record 走索引定位 + 页链读取,位置必须与 location_of 一致
        assert_eq!(db.location_of(DEFAULT_DB, id), Some(loc));
        assert!(db.fetch_record(DEFAULT_DB, 999).unwrap().is_none());
        // 错误的库写入直接拒绝
        assert!(db
            .insert_memory("ghost", "x".into(), vec![], String::new(), 0.0)
            .is_err());
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn multi_db_ids_are_independent() {
        let path = tmp("multi");
        let mut db = Database::create(&path, PW, 4096).unwrap();
        assert!(db.create_database("work").unwrap());
        assert!(!db.create_database("work").unwrap(), "重复建库 false");
        assert_eq!(db.database_names(), vec!["main", "work"]);

        // 两个库各自插入:id 都从 1 开始,互不冲突
        let id_a = db
            .insert_memory(
                "main",
                "Rust 内存安全的所有权规则。".into(),
                vec![],
                "test".into(),
                0.9,
            )
            .unwrap();
        let id_b = db
            .insert_memory(
                "work",
                "Rust 项目的工作日志。".into(),
                vec![],
                "test".into(),
                0.3,
            )
            .unwrap();
        assert_eq!((id_a, id_b), (1, 1), "每库 id 独立自增");
        assert_eq!(db.memory_count(), 2);
        assert_eq!(db.memory_count_in("main"), 1);
        assert_eq!(db.memory_count_in("work"), 1);

        // 同 id 在不同库取到不同记录
        let ra = db.fetch_record("main", 1).unwrap().unwrap();
        let rb = db.fetch_record("work", 1).unwrap().unwrap();
        assert_ne!(ra.content, rb.content);
        assert_eq!(ra.db, "main");
        assert_eq!(rb.db, "work");

        // 库内删除不影响另一库的同 id
        assert_eq!(db.delete_ids("main", &[1]).unwrap(), 1);
        assert!(db.fetch_record("main", 1).unwrap().is_none());
        assert!(db.fetch_record("work", 1).unwrap().is_some());
        assert_eq!(db.memory_count(), 1);

        // 重开:两库结构、库内 id 计数都随快照恢复
        drop(db);
        let mut db2 = Database::open(&path, PW).unwrap();
        assert_eq!(db2.database_names(), vec!["main", "work"]);
        let new_id = db2
            .insert_memory("work", "另一条工作记忆。".into(), vec![], "t".into(), 0.1)
            .unwrap();
        assert_eq!(new_id, 2, "work 库的 id 从 2 继续");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn create_drop_and_use_database() {
        let path = tmp("ddl");
        let mut db = Database::create(&path, PW, 4096).unwrap();
        assert!(db.create_database("Temp").unwrap(), "大写名归一化创建");
        assert!(db.has_database("temp"));
        db.insert_memory(
            "temp",
            "临时库内容。".into(),
            vec![],
            "t".into(),
            0.2,
        )
        .unwrap();

        // USE 存在的库正常;不存在的库报错
        db.use_database("TEMP").unwrap();
        assert!(db.use_database("ghost").is_err());

        // 删除:记录页释放、文档缓存清除
        assert!(db.doc_cache.contains("temp", 1), "USE 预热后应已缓存");
        db.drop_database("temp").unwrap();
        assert!(!db.has_database("temp"));
        assert!(!db.doc_cache.contains("temp", 1));
        assert!(db.drop_database("temp").is_err(), "再删报错");
        // 默认库受保护
        assert!(db.drop_database("main").is_err());
        assert!(db.has_database(DEFAULT_DB));

        // 重开后删除仍然生效
        drop(db);
        let db2 = Database::open(&path, PW).unwrap();
        assert_eq!(db2.database_names(), vec!["main"]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn search_by_keyword_is_scoped_per_db() {
        let path = tmp("scope");
        let mut db = Database::create(&path, PW, 4096).unwrap();
        db.create_database("work").unwrap();
        db.insert_memory("main", "Rust 所有权与内存安全。".into(), vec![], "t".into(), 0.9)
            .unwrap();
        db.insert_memory("work", "Rust 周会安排。".into(), vec![], "t".into(), 0.5)
            .unwrap();
        let main_hits = db.search_by_keyword("main", "rust", 10).unwrap();
        let work_hits = db.search_by_keyword("work", "rust", 10).unwrap();
        assert_eq!(main_hits.len(), 1);
        assert_eq!(work_hits.len(), 1);
        assert_eq!(main_hits[0].2.db, "main");
        assert_eq!(work_hits[0].2.db, "work");
        // 不存在的库检索为空,不报错
        assert!(db.search_by_keyword("ghost", "rust", 10).unwrap().is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
