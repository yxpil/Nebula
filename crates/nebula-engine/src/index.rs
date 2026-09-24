//! 引擎内存索引:按逻辑库分片的记录目录 + 关键词倒排 + 标签索引 + 检索统计 + 读取热度。
//!
//! 这是"类 MySQL"检索的核心:
//! - `records` 相当于聚簇索引(id → 物理行位置)
//! - `postings` 相当于二级倒排索引(关键词 → 命中记录权重)
//! - `tags` 相当于标签等值索引
//! - `stats` 是 AI 检索基础(词频/文档长度/共现图,见 [`crate::search`])
//! - `heat` 是读取热度(读一次 +1,切换库时按热度预加载热点文档)
//!
//! 多库模型:索引按库名分片为 [`DbIndex`](DbIndex),每个库:
//! - id 从 1 独立自增(`next_id`),不同库之间 id 可重复;
//! - 拥有独立的倒排 / 共现图 / 热度,BM25 的 df / avgdl 按库归一化。
//!
//! 索引整体可序列化为快照,由 [`crate::database`] 负责在检查点时落盘。
//! 快照布局(NBMS2):
//! ```text
//! str("NBMS2") || varint(db_count) || 每库:
//!   str(name) || varint(next_id) ||
//!   records / postings / tags / stats / heat 五段
//! ```
//! 旧版快照(无魔数,3/4/5 段单库格式)加载时整体归入默认库 "main",
//! 其中无 `stats` 段时由 database 打开后扫描记录重建,`heat` 缺失时从零累计。

use std::collections::{BTreeMap, BTreeSet};

use nebula_core::codec::BinaryEncode;
use nebula_core::{DEFAULT_DB, MemoryId, MemoryRecord, RecordLocation, Result};

use crate::search::SearchStats;

/// 新版索引快照魔数(旧版快照以 varint 记录数开头,不可能与该前缀混淆)。
const SNAPSHOT_MAGIC: &[u8] = b"\x05NBMS2";

/// 单个逻辑库的索引分片(原 MemoryIndex 的五要素 + 库内 id 自增计数器)。
#[derive(Debug, Default)]
pub struct DbIndex {
    /// id → 记录物理位置(聚簇)。
    records: BTreeMap<MemoryId, RecordLocation>,
    /// 关键词 → [(id, 权重)](倒排,权重降序)。
    postings: BTreeMap<String, Vec<(MemoryId, f32)>>,
    /// 标签 → [id]。
    tags: BTreeMap<String, Vec<MemoryId>>,
    /// 检索统计(词频/文档长度/共现图),随快照持久化。
    stats: SearchStats,
    /// id → 读取热度(每次读 +1;随快照持久化,切换库时用于热点预加载)。
    heat: BTreeMap<MemoryId, u32>,
    /// 下一个分配的 id(每库从 1 自增)。
    next_id: MemoryId,
}

impl DbIndex {
    fn new() -> Self {
        // 每库 id 从 1 自增(0 作为"库不存在"的哨兵返回值)。
        Self {
            next_id: 1,
            ..Default::default()
        }
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn location(&self, id: MemoryId) -> Option<&RecordLocation> {
        self.records.get(&id)
    }

    /// 分配下一个 id(先返回后自增)。
    pub fn allocate_id(&mut self) -> MemoryId {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    pub fn next_id(&self) -> MemoryId {
        self.next_id
    }

    /// 插入或替换一条记忆的索引条目。
    ///
    /// - `record`:记忆记录(postings/tags/共现图的来源);
    /// - `terms`:内容分词后的 (词项, 词频)(BM25 与文档长度的来源);
    /// - `loc`:记录在文件中的物理位置。
    pub fn upsert(&mut self, record: &MemoryRecord, terms: &[(String, u32)], loc: RecordLocation) {
        if let Some(old) = self.records.get(&record.id) {
            if *old != loc {
                self.remove_postings(record.id);
                self.insert_postings(record, loc);
                self.records.insert(record.id, loc);
                self.stats.set_doc(record.id, terms, &keyword_pairs(record));
            }
            return;
        }
        self.insert_postings(record, loc);
        self.records.insert(record.id, loc);
        self.stats.set_doc(record.id, terms, &keyword_pairs(record));
    }

    /// 移除一条记忆的全部索引条目,返回其物理位置。
    pub fn remove(&mut self, id: MemoryId) -> Option<RecordLocation> {
        let loc = self.records.remove(&id)?;
        // 从倒排移除
        self.postings.retain(|_, ids| {
            ids.retain(|(rid, _)| *rid != id);
            !ids.is_empty()
        });
        self.tags.retain(|_, ids| {
            ids.retain(|rid| *rid != id);
            !ids.is_empty()
        });
        self.stats.remove_doc(id);
        self.heat.remove(&id);
        Some(loc)
    }

    // ------- 读取热度(切换库时按热度预加载热点文档)-------

    /// 记录一次读取:该记忆的热度 +1(饱和加法,不会溢出)。
    pub fn bump_heat(&mut self, id: MemoryId) {
        let e = self.heat.entry(id).or_insert(0);
        *e = e.saturating_add(1);
    }

    /// 热度最高的前 n 个 id(热度降序,同热度按 id 升序,结果可复现)。
    pub fn hottest(&self, n: usize) -> Vec<MemoryId> {
        if n == 0 {
            return Vec::new();
        }
        let mut all: Vec<(MemoryId, u32)> = self.heat.iter().map(|(id, h)| (*id, *h)).collect();
        all.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        all.into_iter().take(n).map(|(id, _)| id).collect()
    }

    /// 某记忆的当前热度。
    pub fn heat_of(&self, id: MemoryId) -> u32 {
        self.heat.get(&id).copied().unwrap_or(0)
    }

    /// 有热度记录的文档数。
    pub fn heat_len(&self) -> usize {
        self.heat.len()
    }

    // ------- 检索统计委托(见 crate::search)-------

    /// 登记/重建一篇文档的检索统计(旧快照升级或 UPDATE 后调用)。
    pub fn set_doc_stats(&mut self, id: MemoryId, terms: &[(String, u32)], keywords: &[(String, f32)]) {
        self.stats.set_doc(id, terms, keywords);
    }

    /// 至少包含一个词项的候选文档集合(升序)。
    pub fn docs_with_terms<'a>(
        &self,
        terms: impl IntoIterator<Item = &'a str>,
    ) -> BTreeSet<MemoryId> {
        self.stats.docs_with_terms(terms)
    }

    /// Okapi BM25 打分。
    pub fn bm25(
        &self,
        query: &[(String, f32)],
        cfg: &crate::config::SearchConfig,
    ) -> BTreeMap<MemoryId, f32> {
        self.stats.bm25(query, cfg)
    }

    /// 共现图查询扩展。
    pub fn expand(
        &self,
        query: &[(String, f32)],
        cfg: &crate::config::SearchConfig,
    ) -> Vec<(String, f32)> {
        self.stats.expand(query, cfg)
    }

    /// 种子关键词向量与某文档的余弦相似度。
    pub fn cosine(&self, seed: &[(String, f32)], id: MemoryId) -> f32 {
        self.stats.cosine(seed, id)
    }

    /// 已建立检索统计的文档数。
    pub fn stats_doc_count(&self) -> usize {
        self.stats.doc_count()
    }

    fn insert_postings(&mut self, record: &MemoryRecord, loc: RecordLocation) {
        for kw in &record.keywords {
            let entry = self.postings.entry(kw.term.clone()).or_default();
            entry.push((record.id, kw.weight));
            // 权重降序,同权重按 id 升序,保证可复现的排序
            entry.sort_by(|a, b| {
                b.1.partial_cmp(&a.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.0.cmp(&b.0))
            });
        }
        let _ = loc;
        for tag in &record.tags {
            let entry = self.tags.entry(tag.clone()).or_default();
            if !entry.contains(&record.id) {
                entry.push(record.id);
                entry.sort_unstable();
            }
        }
    }

    fn remove_postings(&mut self, id: MemoryId) {
        self.postings.retain(|_, ids| {
            ids.retain(|(rid, _)| *rid != id);
            !ids.is_empty()
        });
        self.tags.retain(|_, ids| {
            ids.retain(|rid| *rid != id);
            !ids.is_empty()
        });
    }

    /// 关键词命中(按权重降序)。
    pub fn keyword_hits(&self, term: &str) -> Vec<(MemoryId, f32)> {
        self.postings.get(term).cloned().unwrap_or_default()
    }

    /// 标签命中(id 升序)。
    pub fn tag_hits(&self, tag: &str) -> Vec<MemoryId> {
        self.tags.get(tag).cloned().unwrap_or_default()
    }

    /// 全部 id 升序。
    pub fn all_ids(&self) -> Vec<MemoryId> {
        self.records.keys().copied().collect()
    }

    pub fn ids_set(&self) -> BTreeSet<MemoryId> {
        self.records.keys().copied().collect()
    }

    /// 库内全部关键词(按字典序)。
    pub fn all_terms(&self) -> Vec<(String, usize)> {
        self.postings
            .iter()
            .map(|(t, ids)| (t.clone(), ids.len()))
            .collect()
    }

    // ------- 快照序列化(单库五段)-------

    /// 编码为快照字节(records 与 postings 均按键/ id 有序,保证可复现)。
    ///
    /// 布局:`records` / `postings` / `tags` / `stats`(检索统计)/ `heat`(读取热度)。
    fn encode_sections(&self, w: &mut nebula_core::codec::Writer) {
        // records: (id, head_page, page_count, encoded_len)
        w.varint(self.records.len() as u64);
        for (id, loc) in &self.records {
            id.encode(w);
            loc.head_page.encode(w);
            loc.page_count.encode(w);
            loc.encoded_len.encode(w);
        }
        // postings
        w.varint(self.postings.len() as u64);
        for (term, hits) in &self.postings {
            w.str(term);
            w.varint(hits.len() as u64);
            for (id, weight) in hits {
                id.encode(w);
                weight.encode(w);
            }
        }
        // tags
        w.varint(self.tags.len() as u64);
        for (tag, ids) in &self.tags {
            w.str(tag);
            w.varint(ids.len() as u64);
            for id in ids {
                id.encode(w);
            }
        }
        // stats:检索统计(词频/文档长度/共现图)
        self.stats.encode(w);
        // heat:读取热度(id, 热度),按 id 升序
        w.varint(self.heat.len() as u64);
        for (id, heat) in &self.heat {
            id.encode(w);
            w.varint(u64::from(*heat));
        }
    }

    /// 从快照字节恢复单库索引(records / postings / tags 之后按剩余数据
    /// 探测 stats 与 heat 两段是否存在)。
    ///
    /// 返回 `(本库索引, next_id, 是否含检索统计段, 是否含热度段)`:
    /// - 旧版快照(三段)不含统计,调用方需在打开后扫描记录重建,否则 BM25 不可用;
    /// - 旧版快照(四段,有统计无热度)的热度从零开始累计,不影响检索正确性。
    fn load_sections(r: &mut nebula_core::codec::Reader<'_>) -> Result<(Self, bool, bool)> {
        let n = r.varint()? as usize;
        let mut records = BTreeMap::new();
        for _ in 0..n {
            let id = r.u64()?;
            let head_page = r.u64()?;
            let page_count = r.u32()?;
            let encoded_len = r.u64()?;
            records.insert(
                id,
                RecordLocation::new(head_page, page_count, encoded_len),
            );
        }
        let np = r.varint()? as usize;
        let mut postings: BTreeMap<String, Vec<(MemoryId, f32)>> = BTreeMap::new();
        for _ in 0..np {
            let term = r.str()?;
            let m = r.varint()? as usize;
            let mut hits = Vec::with_capacity(m.min(4096));
            for _ in 0..m {
                let id = r.u64()?;
                let weight = r.f32()?;
                hits.push((id, weight));
            }
            postings.insert(term, hits);
        }
        let nt = r.varint()? as usize;
        let mut tags: BTreeMap<String, Vec<MemoryId>> = BTreeMap::new();
        for _ in 0..nt {
            let tag = r.str()?;
            let m = r.varint()? as usize;
            let mut ids = Vec::with_capacity(m.min(4096));
            for _ in 0..m {
                ids.push(r.u64()?);
            }
            tags.insert(tag, ids);
        }
        // stats 段:存在则解析;不存在说明是旧版快照(调用方负责重建)
        let (stats, stats_present) = if r.remaining() > 0 {
            (SearchStats::decode(r)?, true)
        } else {
            (SearchStats::new(), false)
        };
        // heat 段:存在则解析;不存在(旧版快照)从零开始累计
        let (heat, heat_present) = if r.remaining() > 0 {
            let nh = r.varint()? as usize;
            let mut map = BTreeMap::new();
            for _ in 0..nh {
                let id = r.u64()?;
                let heat = r.varint()? as u32;
                map.insert(id, heat);
            }
            (map, true)
        } else {
            (BTreeMap::new(), false)
        };
        // 旧版(无魔数)快照:next_id 从库内最大 id 之后继续,避免 id 冲突
        let next_id = records.keys().next_back().map_or(1, |max| max + 1);
        Ok((
            DbIndex {
                records,
                postings,
                tags,
                stats,
                heat,
                next_id,
            },
            stats_present,
            heat_present,
        ))
    }
}

/// 按逻辑库名分片的索引总表。
#[derive(Debug, Default)]
pub struct MemoryIndex {
    /// 库名(小写)→ 库索引(BTreeMap 保证库名有序,快照可复现)。
    dbs: BTreeMap<String, DbIndex>,
}

impl MemoryIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// 全部库名(按字典序升序)。
    pub fn db_names(&self) -> Vec<String> {
        self.dbs.keys().cloned().collect()
    }

    /// 是否存在某库。
    pub fn has_db(&self, db: &str) -> bool {
        self.dbs.contains_key(db)
    }

    /// 新建空库;库已存在时返回 false(不覆盖)。
    pub fn create_db(&mut self, db: &str) -> bool {
        if self.dbs.contains_key(db) {
            return false;
        }
        self.dbs.insert(db.to_string(), DbIndex::new());
        true
    }

    /// 删除某库及其全部索引条目,返回被删记录的物理位置(调用方负责释放页)。
    pub fn drop_db(&mut self, db: &str) -> Option<Vec<RecordLocation>> {
        let idx = self.dbs.remove(db)?;
        Some(idx.records.into_values().collect())
    }

    /// 某库的记录数。
    pub fn len(&self, db: &str) -> usize {
        self.dbs.get(db).map_or(0, DbIndex::len)
    }

    /// 全部库的记录总数。
    pub fn total_len(&self) -> usize {
        self.dbs.values().map(DbIndex::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.total_len() == 0
    }

    /// 某库分片(不存在返回 None)。
    pub fn db(&self, db: &str) -> Option<&DbIndex> {
        self.dbs.get(db)
    }

    fn db_mut(&mut self, db: &str) -> Option<&mut DbIndex> {
        self.dbs.get_mut(db)
    }

    /// 读取某库的下一个待分配 id。
    pub fn next_id(&self, db: &str) -> MemoryId {
        self.dbs.get(db).map_or(0, DbIndex::next_id)
    }

    /// 在某库分配下一个 id(库必须存在)。
    pub fn allocate_id(&mut self, db: &str) -> MemoryId {
        self.db_mut(db).map(DbIndex::allocate_id).unwrap_or(0)
    }

    pub fn location(&self, db: &str, id: MemoryId) -> Option<&RecordLocation> {
        self.dbs.get(db).and_then(|idx| idx.location(id))
    }

    /// 插入或替换某库内一条记忆的索引条目。
    pub fn upsert(&mut self, db: &str, record: &MemoryRecord, terms: &[(String, u32)], loc: RecordLocation) {
        if let Some(idx) = self.db_mut(db) {
            idx.upsert(record, terms, loc);
        }
    }

    /// 移除某库内一条记忆的全部索引条目,返回其物理位置。
    pub fn remove(&mut self, db: &str, id: MemoryId) -> Option<RecordLocation> {
        self.db_mut(db).and_then(|idx| idx.remove(id))
    }

    // ------- 读取热度 -------

    pub fn bump_heat(&mut self, db: &str, id: MemoryId) {
        if let Some(idx) = self.db_mut(db) {
            idx.bump_heat(id);
        }
    }

    pub fn hottest(&self, db: &str, n: usize) -> Vec<MemoryId> {
        self.dbs.get(db).map_or_else(Vec::new, |idx| idx.hottest(n))
    }

    pub fn heat_of(&self, db: &str, id: MemoryId) -> u32 {
        self.dbs.get(db).map_or(0, |idx| idx.heat_of(id))
    }

    pub fn heat_len(&self, db: &str) -> usize {
        self.dbs.get(db).map_or(0, DbIndex::heat_len)
    }

    /// 全部库有热度记录的文档总数。
    pub fn heat_len_total(&self) -> usize {
        self.dbs.values().map(DbIndex::heat_len).sum()
    }

    // ------- 检索统计委托 -------

    pub fn set_doc_stats(&mut self, db: &str, id: MemoryId, terms: &[(String, u32)], keywords: &[(String, f32)]) {
        if let Some(idx) = self.db_mut(db) {
            idx.set_doc_stats(id, terms, keywords);
        }
    }

    pub fn docs_with_terms<'a>(
        &self,
        db: &str,
        terms: impl IntoIterator<Item = &'a str>,
    ) -> BTreeSet<MemoryId> {
        self.dbs
            .get(db)
            .map_or_else(BTreeSet::new, |idx| idx.docs_with_terms(terms))
    }

    pub fn bm25(
        &self,
        db: &str,
        query: &[(String, f32)],
        cfg: &crate::config::SearchConfig,
    ) -> BTreeMap<MemoryId, f32> {
        self.dbs
            .get(db)
            .map_or_else(BTreeMap::new, |idx| idx.bm25(query, cfg))
    }

    pub fn expand(
        &self,
        db: &str,
        query: &[(String, f32)],
        cfg: &crate::config::SearchConfig,
    ) -> Vec<(String, f32)> {
        self.dbs
            .get(db)
            .map_or_else(Vec::new, |idx| idx.expand(query, cfg))
    }

    pub fn cosine(&self, db: &str, seed: &[(String, f32)], id: MemoryId) -> f32 {
        self.dbs
            .get(db)
            .map_or(0.0, |idx| idx.cosine(seed, id))
    }

    pub fn stats_doc_count(&self, db: &str) -> usize {
        self.dbs.get(db).map_or(0, DbIndex::stats_doc_count)
    }

    // ------- 倒排 / 标签 / 扫描 -------

    pub fn keyword_hits(&self, db: &str, term: &str) -> Vec<(MemoryId, f32)> {
        self.dbs
            .get(db)
            .map_or_else(Vec::new, |idx| idx.keyword_hits(term))
    }

    pub fn tag_hits(&self, db: &str, tag: &str) -> Vec<MemoryId> {
        self.dbs
            .get(db)
            .map_or_else(Vec::new, |idx| idx.tag_hits(tag))
    }

    pub fn all_ids(&self, db: &str) -> Vec<MemoryId> {
        self.dbs
            .get(db)
            .map_or_else(Vec::new, |idx| idx.all_ids())
    }

    pub fn ids_set(&self, db: &str) -> BTreeSet<MemoryId> {
        self.dbs
            .get(db)
            .map_or_else(BTreeSet::new, |idx| idx.ids_set())
    }

    pub fn all_terms(&self, db: &str) -> Vec<(String, usize)> {
        self.dbs
            .get(db)
            .map_or_else(Vec::new, |idx| idx.all_terms())
    }

    // ------- 快照序列化 -------

    /// 编码为多库快照:`str("NBMS2") || varint(库数) || 每库(name, next_id, 五段)`。
    pub fn encode_snapshot(&self) -> Vec<u8> {
        let mut w = nebula_core::codec::Writer::new();
        w.raw(SNAPSHOT_MAGIC);
        w.varint(self.dbs.len() as u64);
        for (name, idx) in &self.dbs {
            w.str(name);
            w.varint(idx.next_id);
            idx.encode_sections(&mut w);
        }
        w.into_vec()
    }

    /// 从快照字节恢复索引。
    ///
    /// 返回 `(索引, 是否含检索统计段, 是否含热度段)`(对整个文件而言,
    /// 旧版单库快照统一归入默认库 "main"):
    /// - NBMS2 快照恒含 stats / heat 两段;
    /// - 旧版快照(三段/四段)按段探测,调用方负责在打开后重建缺失的统计。
    pub fn load_snapshot(bytes: &[u8]) -> Result<(Self, bool, bool)> {
        if bytes.is_empty() {
            return Ok((Self::new(), false, false));
        }
        if bytes.starts_with(SNAPSHOT_MAGIC) {
            // NBMS2:多库布局
            let mut r = nebula_core::codec::Reader::new(&bytes[SNAPSHOT_MAGIC.len()..]);
            let n = r.varint()? as usize;
            let mut dbs = BTreeMap::new();
            for _ in 0..n {
                let name = r.str()?;
                let next_id = r.varint()?;
                let (mut idx, _s, _h) = DbIndex::load_sections(&mut r)?;
                // v2 快照显式记录 next_id(空库或全部删除后仍保持不回退)
                idx.next_id = next_id;
                dbs.insert(name, idx);
            }
            if r.remaining() != 0 {
                return Err(nebula_core::Error::Engine(
                    "index snapshot has trailing bytes".into(),
                ));
            }
            Ok((MemoryIndex { dbs }, true, true))
        } else {
            // 旧版单库快照(3/4/5 段):整体归入默认库,next_id 从最大 id 推导
            let mut r = nebula_core::codec::Reader::new(bytes);
            let (idx, stats_present, heat_present) = DbIndex::load_sections(&mut r)?;
            if r.remaining() != 0 {
                return Err(nebula_core::Error::Engine(
                    "index snapshot has trailing bytes".into(),
                ));
            }
            let mut dbs = BTreeMap::new();
            if !idx.is_empty() {
                dbs.insert(DEFAULT_DB.to_string(), idx);
            }
            Ok((MemoryIndex { dbs }, stats_present, heat_present))
        }
    }
}

/// 记录的关键词 → (词项, 权重) 列表(共现图与种子相似度用)。
fn keyword_pairs(record: &MemoryRecord) -> Vec<(String, f32)> {
    record
        .keywords
        .iter()
        .map(|k| (k.term.clone(), k.weight))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nebula_core::Keyword;
    use nebula_core::codec::Writer;

    fn rec(id: u64, terms: &[(&str, f32)], tags: &[&str]) -> MemoryRecord {
        let mut r = MemoryRecord::new("content");
        r.id = id;
        r.keywords = terms.iter().map(|(t, w)| Keyword::new(*t, *w)).collect();
        r.tags = tags.iter().map(|t| t.to_string()).collect();
        r
    }

    fn tf(terms: &[(&str, u32)]) -> Vec<(String, u32)> {
        terms.iter().map(|(t, f)| ((*t).into(), *f)).collect()
    }

    #[test]
    fn snapshot_roundtrip_multi_db() {
        let mut idx = MemoryIndex::new();
        assert!(idx.create_db("main"));
        assert!(idx.create_db("work"));
        assert!(!idx.create_db("main"), "重复建库应返回 false");
        assert_eq!(idx.db_names(), vec!["main", "work"]);

        idx.upsert(
            "main",
            &rec(1, &[("rust", 0.9), ("内存", 0.4)], &["编程"]),
            &tf(&[("rust", 2), ("内存", 1)]),
            RecordLocation::new(2, 1, 10),
        );
        idx.upsert(
            "work",
            &rec(1, &[("rust", 0.7)], &["项目"]),
            &tf(&[("rust", 1)]),
            RecordLocation::new(3, 2, 20),
        );
        // 热度:main 库的 1 被读 3 次、work 库的 1 被读 1 次
        idx.bump_heat("main", 1);
        idx.bump_heat("main", 1);
        idx.bump_heat("main", 1);
        idx.bump_heat("work", 1);

        let bytes = idx.encode_snapshot();
        let (back, stats_present, heat_present) = MemoryIndex::load_snapshot(&bytes).unwrap();
        assert!(stats_present, "新版快照必须带 stats 段");
        assert!(heat_present, "新版快照必须带 heat 段");

        assert_eq!(back.db_names(), vec!["main", "work"]);
        assert_eq!(back.len("main"), 1);
        assert_eq!(back.len("work"), 1);
        // 两库 id 各自独立,内容互不串味
        assert_eq!(back.keyword_hits("main", "rust"), vec![(1, 0.9)]);
        assert_eq!(back.keyword_hits("work", "rust"), vec![(1, 0.7)]);
        assert_eq!(back.tag_hits("main", "编程"), vec![1]);
        assert_eq!(back.tag_hits("work", "项目"), vec![1]);
        assert_eq!(back.location("work", 1).unwrap().head_page, 3);
        // 检索统计随快照恢复
        assert_eq!(back.stats_doc_count("main"), 1);
        assert_eq!(back.stats_doc_count("work"), 1);
        assert!(back.docs_with_terms("work", ["rust"]).contains(&1));
        // 共现图恢复:rust 与 内存(文档 1 共现)可扩展
        let q = vec![("rust".to_string(), 1.0f32)];
        let expanded = back.expand("main", &q, &crate::config::SearchConfig::default());
        assert!(expanded.iter().any(|(t, _)| t == "内存"));
        // 热度恢复:main 库 hottest 为 1;work 库的热度独立统计
        assert_eq!(back.heat_of("main", 1), 3);
        assert_eq!(back.heat_of("work", 1), 1);
        assert_eq!(back.hottest("main", 3), vec![1]);
        assert_eq!(back.hottest("work", 3), vec![1]);
    }

    #[test]
    fn next_id_allocates_per_db() {
        let mut idx = MemoryIndex::new();
        idx.create_db("main");
        idx.create_db("work");
        // 每库从 1 开始独立自增
        assert_eq!(idx.next_id("main"), 1);
        assert_eq!(idx.allocate_id("main"), 1);
        assert_eq!(idx.allocate_id("main"), 2);
        assert_eq!(idx.allocate_id("work"), 1);
        assert_eq!(idx.next_id("work"), 2);
        // 不存在的库:next_id = 0,allocate 返回 0(调用方保证库已创建)
        assert_eq!(idx.next_id("ghost"), 0);
        assert_eq!(idx.allocate_id("ghost"), 0);
        // next_id 随快照持久化,重开后不回头
        let bytes = idx.encode_snapshot();
        let (back, _, _) = MemoryIndex::load_snapshot(&bytes).unwrap();
        assert_eq!(back.next_id("main"), 3);
        assert_eq!(back.next_id("work"), 2);
    }

    #[test]
    fn create_and_drop_db() {
        let mut idx = MemoryIndex::new();
        idx.create_db("main");
        idx.upsert(
            "main",
            &rec(1, &[("rust", 0.9)], &[]),
            &tf(&[("rust", 1)]),
            RecordLocation::new(2, 1, 10),
        );
        idx.create_db("work");
        idx.upsert(
            "work",
            &rec(1, &[("go", 0.9)], &[]),
            &tf(&[("go", 1)]),
            RecordLocation::new(7, 1, 10),
        );
        // drop 返回被删记录的物理位置(调用方负责释放页)
        let locs = idx.drop_db("work").expect("work db existed");
        assert_eq!(locs, vec![RecordLocation::new(7, 1, 10)]);
        assert!(!idx.has_db("work"));
        assert_eq!(idx.total_len(), 1);
        assert_eq!(idx.all_terms("work"), Vec::<(String, usize)>::new());
        // 删不存在的库返回 None
        assert!(idx.drop_db("ghost").is_none());
    }

    #[test]
    fn legacy_snapshot_without_stats_loads() {
        // 旧版三段快照:records + postings + tags(手工编码,无 stats 段)
        let mut w = Writer::new();
        w.varint(1);
        1u64.encode(&mut w);
        2u64.encode(&mut w);
        1u32.encode(&mut w);
        10u64.encode(&mut w);
        w.varint(1);
        w.str("rust");
        w.varint(1);
        1u64.encode(&mut w);
        0.9f32.encode(&mut w);
        w.varint(1);
        w.str("编程");
        w.varint(1);
        1u64.encode(&mut w);
        let legacy = w.into_vec();

        let (back, stats_present, heat_present) = MemoryIndex::load_snapshot(&legacy).unwrap();
        assert!(!stats_present, "旧版快照应标记为无 stats");
        assert!(!heat_present, "旧版快照应标记为无 heat");
        // 旧库记录整体归入默认库
        assert_eq!(back.db_names(), vec![DEFAULT_DB]);
        assert_eq!(back.len(DEFAULT_DB), 1);
        assert_eq!(back.keyword_hits(DEFAULT_DB, "rust"), vec![(1, 0.9)]);
        // stats 为空 → BM25 无候选(等待调用方重建)
        assert_eq!(back.stats_doc_count(DEFAULT_DB), 0);
        assert!(back.docs_with_terms(DEFAULT_DB, ["rust"]).is_empty());
        // 热度从零开始,不影响检索
        assert_eq!(back.heat_len(DEFAULT_DB), 0);
        assert_eq!(back.hottest(DEFAULT_DB, 5), Vec::<MemoryId>::new());
        // next_id 从最大 id 之后继续,不与既有记录冲突
        assert_eq!(back.next_id(DEFAULT_DB), 2);
        // 旧索引仍可参与新快照:set_doc_stats 后恢复检索能力
        let mut back = back;
        back.set_doc_stats(DEFAULT_DB, 1, &tf(&[("rust", 1)]), &[("rust".to_string(), 0.9f32)]);
        assert_eq!(back.stats_doc_count(DEFAULT_DB), 1);
        assert!(back.docs_with_terms(DEFAULT_DB, ["rust"]).contains(&1));
    }

    #[test]
    fn legacy_snapshot_with_stats_without_heat_loads() {
        // 旧版四段快照:records + postings + tags + stats(有统计,无 heat)
        let mut idx = MemoryIndex::new();
        idx.create_db(DEFAULT_DB);
        idx.upsert(
            DEFAULT_DB,
            &rec(1, &[("rust", 0.9)], &["编程"]),
            &tf(&[("rust", 1)]),
            RecordLocation::new(2, 1, 10),
        );
        let mut w = Writer::new();
        w.varint(1);
        1u64.encode(&mut w);
        2u64.encode(&mut w);
        1u32.encode(&mut w);
        10u64.encode(&mut w);
        w.varint(1);
        w.str("rust");
        w.varint(1);
        1u64.encode(&mut w);
        0.9f32.encode(&mut w);
        w.varint(1);
        w.str("编程");
        w.varint(1);
        1u64.encode(&mut w);
        // stats 段直接取当前索引内部的统计(有数据),但不写 heat 段
        idx.db(DEFAULT_DB).unwrap().stats.encode(&mut w);
        let legacy = w.into_vec();

        let (back, stats_present, heat_present) = MemoryIndex::load_snapshot(&legacy).unwrap();
        assert!(stats_present, "四段快照含 stats");
        assert!(!heat_present, "四段快照无 heat");
        assert_eq!(back.len(DEFAULT_DB), 1);
        // 检索能力完整恢复
        assert_eq!(back.stats_doc_count(DEFAULT_DB), 1);
        assert!(back.docs_with_terms(DEFAULT_DB, ["rust"]).contains(&1));
        // 热度从零开始(不影响检索正确性)
        assert_eq!(back.heat_len(DEFAULT_DB), 0);
    }

    #[test]
    fn empty_legacy_snapshot_has_no_dbs() {
        let (back, stats_present, heat_present) = MemoryIndex::load_snapshot(&[]).unwrap();
        assert!(back.is_empty());
        assert!(back.db_names().is_empty());
        assert!(!stats_present);
        assert!(!heat_present);
    }

    #[test]
    fn remove_cleans_postings_and_heat() {
        let mut idx = MemoryIndex::new();
        idx.create_db("main");
        idx.upsert(
            "main",
            &rec(1, &[("rust", 0.9)], &["t"]),
            &tf(&[("rust", 1)]),
            RecordLocation::new(2, 1, 1),
        );
        idx.upsert(
            "main",
            &rec(2, &[("rust", 0.5)], &["t"]),
            &tf(&[("rust", 1)]),
            RecordLocation::new(3, 1, 1),
        );
        idx.bump_heat("main", 1);
        idx.bump_heat("main", 1);
        idx.remove("main", 1);
        assert_eq!(idx.keyword_hits("main", "rust"), vec![(2, 0.5)]);
        assert_eq!(idx.tag_hits("main", "t"), vec![2]);
        assert_eq!(idx.len("main"), 1);
        // 检索统计与热度同步清理
        assert_eq!(idx.stats_doc_count("main"), 1);
        assert!(!idx.docs_with_terms("main", ["rust"]).contains(&1));
        assert_eq!(idx.heat_len("main"), 0);
        assert_eq!(idx.heat_of("main", 1), 0);
        // 删除不存在的库/记录是安全的空操作
        assert!(idx.remove("ghost", 1).is_none());
        assert!(idx.remove("main", 999).is_none());
    }

    #[test]
    fn hottest_orders_by_heat_then_id() {
        let mut idx = MemoryIndex::new();
        idx.create_db("main");
        for id in 1..=4 {
            idx.bump_heat("main", id);
        }
        idx.bump_heat("main", 3);
        idx.bump_heat("main", 3);
        // 热度降序、同热度 id 升序;n=0 返回空
        assert_eq!(idx.hottest("main", 0), Vec::<MemoryId>::new());
        assert_eq!(idx.hottest("main", 2), vec![3, 1]);
        assert_eq!(idx.hottest("main", 10), vec![3, 1, 2, 4]);
    }
}
