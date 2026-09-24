//! 引擎内存索引:记录目录 + 关键词倒排 + 标签索引。
//!
//! 这是"类 MySQL"检索的核心:
//! - `records` 相当于聚簇索引(id → 物理行位置)
//! - `postings` 相当于二级倒排索引(关键词 → 命中记录权重)
//! - `tags` 相当于标签等值索引
//!
//! 索引整体可序列化为快照,由 [`crate::database`] 负责在检查点时落盘。

use std::collections::{BTreeMap, BTreeSet};

use nebula_core::codec::BinaryEncode;
use nebula_core::{MemoryId, MemoryRecord, RecordLocation, Result};

#[derive(Debug, Default)]
pub struct MemoryIndex {
    /// id → 记录物理位置(聚簇)。
    records: BTreeMap<MemoryId, RecordLocation>,
    /// 关键词 → [(id, 权重)](倒排,权重降序)。
    postings: BTreeMap<String, Vec<(MemoryId, f32)>>,
    /// 标签 → [id]。
    tags: BTreeMap<String, Vec<MemoryId>>,
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
    pub fn upsert(&mut self, record: &MemoryRecord, loc: RecordLocation) {
        if let Some(old) = self.records.get(&record.id) {
            if *old != loc {
                self.remove_postings(record.id);
                self.insert_postings(record, loc);
                self.records.insert(record.id, loc);
                return;
            }
            return;
        }
        self.insert_postings(record, loc);
        self.records.insert(record.id, loc);
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
        Some(loc)
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
        w.into_vec()
    }

    /// 从快照字节恢复索引。
    pub fn load_snapshot(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() {
            return Ok(Self::new());
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
        if r.remaining() != 0 {
            return Err(nebula_core::Error::Engine(
                "index snapshot has trailing bytes".into(),
            ));
        }
        Ok(MemoryIndex {
            records,
            postings,
            tags,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nebula_core::Keyword;

    fn rec(id: u64, terms: &[(&str, f32)], tags: &[&str]) -> MemoryRecord {
        let mut r = MemoryRecord::new("content");
        r.id = id;
        r.keywords = terms.iter().map(|(t, w)| Keyword::new(*t, *w)).collect();
        r.tags = tags.iter().map(|t| t.to_string()).collect();
        r
    }

    #[test]
    fn snapshot_roundtrip() {
        let mut idx = MemoryIndex::new();
        idx.upsert(&rec(1, &[("rust", 0.9), ("内存", 0.4)], &["编程"]), RecordLocation::new(2, 1, 10));
        idx.upsert(&rec(5, &[("rust", 0.7)], &["编程", "重点"]), RecordLocation::new(3, 2, 20));
        idx.upsert(&rec(7, &[("数据库", 1.0)], &[]), RecordLocation::new(5, 1, 8));

        let bytes = idx.encode_snapshot();
        let back = MemoryIndex::load_snapshot(&bytes).unwrap();

        assert_eq!(back.len(), 3);
        assert_eq!(
            back.keyword_hits("rust"),
            vec![(1, 0.9), (5, 0.7)]
        );
        assert_eq!(back.tag_hits("编程"), vec![1, 5]);
        assert_eq!(back.tag_hits("重点"), vec![5]);
        assert_eq!(back.location(7).unwrap().head_page, 5);
    }

    #[test]
    fn remove_cleans_postings() {
        let mut idx = MemoryIndex::new();
        idx.upsert(&rec(1, &[("rust", 0.9)], &["t"]), RecordLocation::new(2, 1, 1));
        idx.upsert(&rec(2, &[("rust", 0.5)], &["t"]), RecordLocation::new(3, 1, 1));
        idx.remove(1);
        assert_eq!(idx.keyword_hits("rust"), vec![(2, 0.5)]);
        assert_eq!(idx.tag_hits("t"), vec![2]);
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn empty_snapshot_loads_empty_index() {
        let idx = MemoryIndex::load_snapshot(&[]).unwrap();
        assert!(idx.is_empty());
    }
}
