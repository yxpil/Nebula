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
//! 所有行为参数(检查点阈值、内容/关键词/关键点上限、分词与停用词)
//! 通过 [`crate::EngineConfig`] 与停用词集合注入,本 crate 不读取配置文件。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use nebula_core::{codec, MemoryId, MemoryRecord, Result};
#[cfg(test)]
use nebula_core::RecordLocation;
use nebula_tokenizer::{default_stopword_set, ExtractorConfig, TextExtractor};

use nebula_storage::MemoryFile;

use crate::config::EngineConfig;
use crate::index::MemoryIndex;

pub struct Database {
    path: PathBuf,
    pub(crate) file: MemoryFile,
    pub(crate) index: MemoryIndex,
    pub(crate) extractor: TextExtractor,
    pub(crate) cfg: EngineConfig,
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
        let index = MemoryIndex::load_snapshot(&snap)?;
        Ok(Database {
            path,
            file,
            index,
            extractor: TextExtractor::new(extractor_cfg.clone(), stopwords.clone()),
            cfg: cfg.clone(),
        })
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
        self.index.upsert(&record, loc);
        self.file.set_memory_count(self.index.len() as u64);
        self.file.commit_catalog()?;

        if self.file.dirty_records() >= self.cfg.auto_checkpoint {
            self.checkpoint()?;
        }
        Ok(record.id)
    }

    /// 读取一条记忆(经索引定位)。
    pub(crate) fn fetch_record(&mut self, id: MemoryId) -> Result<Option<MemoryRecord>> {
        let Some(loc) = self.index.location(id).copied() else {
            return Ok(None);
        };
        let bytes = self.file.read_record(&loc)?;
        let record: MemoryRecord = codec::from_slice(&bytes)?;
        Ok(Some(record))
    }

    /// 用新内容替换记录(写新页 → 更新索引 → 释放旧页)。
    pub(crate) fn replace_record(&mut self, record: &MemoryRecord) -> Result<()> {
        let old_loc = self.index.location(record.id).copied();
        let bytes = codec::to_vec(record);
        let loc = self.file.put_record(&bytes)?;
        self.index.upsert(record, loc);
        if let Some(old) = old_loc {
            if old != loc {
                self.file.free_record(&old)?;
            }
        }
        self.file.set_memory_count(self.index.len() as u64);
        self.file.commit_catalog()?;
        // 索引位置已变,必须重写快照,否则重开后索引会指向旧页。
        self.checkpoint()?;
        Ok(())
    }

    /// 删除一组记忆(释放页 + 清理索引 + 立即写快照)。
    pub(crate) fn delete_ids(&mut self, ids: &[MemoryId]) -> Result<u64> {
        let mut removed = 0u64;
        for id in ids {
            if let Some(loc) = self.index.remove(*id) {
                self.file.free_record(&loc)?;
                removed += 1;
            }
        }
        if removed > 0 {
            self.file.set_memory_count(self.index.len() as u64);
            self.file.commit_catalog()?;
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
}
