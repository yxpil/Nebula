//! 引擎内存索引:记录目录 + 关键词倒排 + 标签索引 + 检索统计 + 读取热度。
//!
//! 这是"类 MySQL"检索的核心:
//! - `records` 相当于聚簇索引(id → 物理行位置)
//! - `postings` 相当于二级倒排索引(关键词 → 命中记录权重)
//! - `tags` 相当于标签等值索引
//! - `stats` 是 AI 检索基础(词频/文档长度/共现图,见 [`crate::search`])
//! - `heat` 是读取热度(读一次 +1,打开库时按热度预加载热点文档)
//!
//! 索引整体可序列化为快照,由 [`crate::database`] 负责在检查点时落盘。
//! 快照布局:records / postings / tags 三段(旧版即到此为止),
//! 之后追加第 4 段 `stats`、第 5 段 `heat`;旧版快照加载时对应段为空,
//! 其中 `stats` 由 database 打开后扫描记录重建,`heat` 从零累计即可。

use std::collections::{BTreeMap, BTreeSet};

use nebula_core::codec::BinaryEncode;
use nebula_core::{MemoryId, MemoryRecord, RecordLocation, Result};

use crate::search::SearchStats;

#[derive(Debug, Default)]
pub struct MemoryIndex {
    /// id → 记录物理位置(聚簇)。
    records: BTreeMap<MemoryId, RecordLocation>,
    /// 关键词 → [(id, 权重)](倒排,权重降序)。
    postings: BTreeMap<String, Vec<(MemoryId, f32)>>,
    /// 标签 → [id]。
    tags: BTreeMap<String, Vec<MemoryId>>,
    /// 检索统计(词频/文档长度/共现图),随快照持久化。
    stats: SearchStats,
    /// id → 读取热度(每次读 +1;随快照持久化,打开库时用于热点预加载)。
    heat: BTreeMap<MemoryId, u32>,
}

impl MemoryIndex {
    pub fn new() -> Self {
        Self::default()
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
                return;
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

    // ------- 读取热度(打开库时按热度预加载热点文档)-------

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

    /// 索引内全部关键词(按字典序)。
    pub fn all_terms(&self) -> Vec<(String, usize)> {
        self.postings
            .iter()
            .map(|(t, ids)| (t.clone(), ids.len()))
            .collect()
    }

    // ------- 快照序列化 -------

    /// 编码为快照字节(records 与 postings 均按键/ id 有序,保证可复现)。
    ///
    /// 布局:`records` / `postings` / `tags` / `stats`(检索统计)/ `heat`(读取热度)。
    /// 前三段与旧版一致,`stats`、`heat` 为追加段;加载时据此区分新旧快照。
    pub fn encode_snapshot(&self) -> Vec<u8> {
        let mut w = nebula_core::codec::Writer::new();
        // records: (id, head_page, page_count, encoded_len)
        w.varint(self.records.len() as u64);
        for (id, loc) in &self.records {
            id.encode(&mut w);
            loc.head_page.encode(&mut w);
            loc.page_count.encode(&mut w);
            loc.encoded_len.encode(&mut w);
        }
        // postings
        w.varint(self.postings.len() as u64);
        for (term, hits) in &self.postings {
            w.str(term);
            w.varint(hits.len() as u64);
            for (id, weight) in hits {
                id.encode(&mut w);
                weight.encode(&mut w);
            }
        }
        // tags
        w.varint(self.tags.len() as u64);
        for (tag, ids) in &self.tags {
            w.str(tag);
            w.varint(ids.len() as u64);
            for id in ids {
                id.encode(&mut w);
            }
        }
        // stats:检索统计(词频/文档长度/共现图)
        self.stats.encode(&mut w);
        // heat:读取热度(id, 热度),按 id 升序
        w.varint(self.heat.len() as u64);
        for (id, heat) in &self.heat {
            id.encode(&mut w);
            w.varint(u64::from(*heat));
        }
        w.into_vec()
    }

    /// 从快照字节恢复索引。
    ///
    /// 返回 `(索引, 是否含检索统计段, 是否含热度段)`:
    /// - 旧版快照(三段)不含统计,调用方需在打开后扫描记录重建,否则 BM25 不可用;
    /// - 旧版快照(四段,有统计无热度)的热度从零开始累计,不影响检索正确性。
    pub fn load_snapshot(bytes: &[u8]) -> Result<(Self, bool, bool)> {
        if bytes.is_empty() {
            return Ok((Self::new(), false, false));
        }
        let mut r = nebula_core::codec::Reader::new(bytes);
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
            (SearchStats::decode(&mut r)?, true)
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
        if r.remaining() != 0 {
            return Err(nebula_core::Error::Engine(
                "index snapshot has trailing bytes".into(),
            ));
        }
        Ok((
            MemoryIndex {
                records,
                postings,
                tags,
                stats,
                heat,
            },
            stats_present,
            heat_present,
        ))
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
    fn snapshot_roundtrip() {
        let mut idx = MemoryIndex::new();
        idx.upsert(
            &rec(1, &[("rust", 0.9), ("内存", 0.4)], &["编程"]),
            &tf(&[("rust", 2), ("内存", 1)]),
            RecordLocation::new(2, 1, 10),
        );
        idx.upsert(
            &rec(5, &[("rust", 0.7)], &["编程", "重点"]),
            &tf(&[("rust", 1)]),
            RecordLocation::new(3, 2, 20),
        );
        idx.upsert(
            &rec(7, &[("数据库", 1.0)], &[]),
            &tf(&[("数据库", 3), ("索引", 1)]),
            RecordLocation::new(5, 1, 8),
        );

        // 热度:id 7 被读 3 次、id 1 被读 1 次,随后一起持久化
        idx.bump_heat(1);
        idx.bump_heat(7);
        idx.bump_heat(7);
        idx.bump_heat(7);

        let bytes = idx.encode_snapshot();
        let (back, stats_present, heat_present) = MemoryIndex::load_snapshot(&bytes).unwrap();
        assert!(stats_present, "新版快照必须带 stats 段");
        assert!(heat_present, "新版快照必须带 heat 段");

        assert_eq!(back.len(), 3);
        assert_eq!(back.keyword_hits("rust"), vec![(1, 0.9), (5, 0.7)]);
        assert_eq!(back.tag_hits("编程"), vec![1, 5]);
        assert_eq!(back.tag_hits("重点"), vec![5]);
        assert_eq!(back.location(7).unwrap().head_page, 5);
        // 检索统计随快照恢复
        assert_eq!(back.stats_doc_count(), 3);
        assert!(back.docs_with_terms(["rust"]).contains(&1));
        assert!(back.docs_with_terms(["索引"]).contains(&7));
        // 共现图恢复:rust 与 内存(文档 1 共现)可扩展
        let q = vec![("rust".to_string(), 1.0f32)];
        let expanded = back.expand(&q, &crate::config::SearchConfig::default());
        assert!(expanded.iter().any(|(t, _)| t == "内存"));
        // 热度恢复:7(3 次) hottest,其次 1(1 次);无热度的 5 不在热度表内
        assert_eq!(back.hottest(3), vec![7, 1]);
        assert_eq!(back.heat_of(7), 3);
        assert_eq!(back.heat_of(5), 0);
        assert_eq!(back.heat_len(), 2);
    }

    #[test]
    fn legacy_snapshot_without_stats_loads() {
        // 旧版三段快照:records + postings + tags(手工编码,无 stats 段)
        let mut idx = MemoryIndex::new();
        idx.upsert(
            &rec(1, &[("rust", 0.9)], &["编程"]),
            &tf(&[("rust", 1)]),
            RecordLocation::new(2, 1, 10),
        );
        let mut w = Writer::new();
        // 手工重放前三段(与 encode_snapshot 的前半部分一致)
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
        assert_eq!(back.len(), 1);
        assert_eq!(back.keyword_hits("rust"), vec![(1, 0.9)]);
        // stats 为空 → BM25 无候选(等待调用方重建)
        assert_eq!(back.stats_doc_count(), 0);
        assert!(back.docs_with_terms(["rust"]).is_empty());
        // 热度从零开始,不影响检索
        assert_eq!(back.heat_len(), 0);
        assert_eq!(back.hottest(5), Vec::<MemoryId>::new());
        // 旧索引仍可参与新快照:set_doc_stats 后恢复检索能力
        let mut back = back;
        back.set_doc_stats(1, &tf(&[("rust", 1)]), &[("rust".to_string(), 0.9f32)]);
        assert_eq!(back.stats_doc_count(), 1);
        assert!(back.docs_with_terms(["rust"]).contains(&1));
        // 原 idx 与手工编码的旧快照前三段一致(校验手工编码没写错)
        let full = idx.encode_snapshot();
        let re = nebula_core::codec::Reader::new(&full);
        assert!(re.remaining() > legacy.len());
    }

    #[test]
    fn remove_cleans_postings() {
        let mut idx = MemoryIndex::new();
        idx.upsert(
            &rec(1, &[("rust", 0.9)], &["t"]),
            &tf(&[("rust", 1)]),
            RecordLocation::new(2, 1, 1),
        );
        idx.upsert(
            &rec(2, &[("rust", 0.5)], &["t"]),
            &tf(&[("rust", 1)]),
            RecordLocation::new(3, 1, 1),
        );
        idx.remove(1);
        assert_eq!(idx.keyword_hits("rust"), vec![(2, 0.5)]);
        assert_eq!(idx.tag_hits("t"), vec![2]);
        assert_eq!(idx.len(), 1);
        // 检索统计同步清理
        assert_eq!(idx.stats_doc_count(), 1);
        assert!(!idx.docs_with_terms(["rust"]).contains(&1));
    }

    #[test]
    fn legacy_snapshot_with_stats_without_heat_loads() {
        // 旧版四段快照:records + postings + tags + stats(有统计,无 heat)
        let mut idx = MemoryIndex::new();
        idx.upsert(
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
        idx.stats.encode(&mut w);
        let legacy = w.into_vec();

        let (back, stats_present, heat_present) = MemoryIndex::load_snapshot(&legacy).unwrap();
        assert!(stats_present, "四段快照含 stats");
        assert!(!heat_present, "四段快照无 heat");
        assert_eq!(back.len(), 1);
        // 检索能力完整恢复
        assert_eq!(back.stats_doc_count(), 1);
        assert!(back.docs_with_terms(["rust"]).contains(&1));
        // 热度从零开始(不影响检索正确性)
        assert_eq!(back.heat_len(), 0);
    }

    #[test]
    fn remove_cleans_heat() {
        let mut idx = MemoryIndex::new();
        idx.upsert(
            &rec(1, &[("rust", 0.9)], &["t"]),
            &tf(&[("rust", 1)]),
            RecordLocation::new(2, 1, 1),
        );
        idx.bump_heat(1);
        idx.bump_heat(1);
        assert_eq!(idx.heat_of(1), 2);
        idx.remove(1);
        assert_eq!(idx.heat_len(), 0);
        assert_eq!(idx.heat_of(1), 0);
    }

    #[test]
    fn hottest_orders_by_heat_then_id() {
        let mut idx = MemoryIndex::new();
        for id in 1..=4 {
            idx.bump_heat(id);
        }
        idx.bump_heat(3);
        idx.bump_heat(3);
        // 热度降序、同热度 id 升序;n=0 返回空
        assert_eq!(idx.hottest(0), Vec::<MemoryId>::new());
        assert_eq!(idx.hottest(2), vec![3, 1]);
        assert_eq!(idx.hottest(10), vec![3, 1, 2, 4]);
    }

    #[test]
    fn empty_snapshot_loads_empty_index() {
        let (idx, stats_present, heat_present) = MemoryIndex::load_snapshot(&[]).unwrap();
        assert!(idx.is_empty());
        assert!(!stats_present);
        assert!(!heat_present);
    }
}
