//! 检索基础设施:词频倒排、文档长度、关键词共现图,以及 BM25 / 查询扩展 / 余弦相似度。
//!
//! 三层检索模型:
//! 1. **BM25**:`term → [(id, tf)]` 倒排 + 每文档词条总数,经典 Okapi BM25 打分;
//! 2. **共现图联想**:同一记忆的关键词两两共现计数,支持 1~2 跳查询扩展
//!    (每多一跳权重乘以 `expansion_decay`,扩展强度按条件概率 P(other | term) 归一化);
//! 3. **联合重排**:种子关键词权重向量与候选文档的余弦相似度,按
//!    `similarity_weight` 叠加到 BM25 分数上(组合逻辑在 [`crate::executor`])。
//!
//! 这些统计与倒排索引一起持久化进快照(见 [`crate::index`]);
//! 旧版快照没有这些段,由 [`crate::database`] 打开时扫描全部记录一次性重建。

use std::collections::{BTreeMap, BTreeSet, HashMap};

use nebula_core::codec::{BinaryEncode, Reader, Writer};
use nebula_core::{MemoryId, Result};

use crate::config::SearchConfig;

/// 每篇文档的检索统计(随索引快照持久化)。
#[derive(Debug, Default)]
pub struct SearchStats {
    /// term → [(id, tf)](按 id 升序;tf = 词项在文档中的出现次数)。
    freqs: BTreeMap<String, Vec<(MemoryId, u32)>>,
    /// id → 文档长度(该文档全部索引词条的 tf 之和)。
    doc_len: BTreeMap<MemoryId, u32>,
    /// id → 该记忆的关键词 (词项, 权重)(共现图与种子相似度用)。
    doc_terms: BTreeMap<MemoryId, Vec<(String, f32)>>,
    /// 关键词共现图(双向存储):term → 共现词 → 同现文档数。
    cooc: BTreeMap<String, BTreeMap<String, u32>>,
}

impl SearchStats {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记/更新一篇文档的全部检索统计(先清除旧数据,保证与记录内容一致)。
    ///
    /// - `terms`:内容分词后的 (词项, 词频) 列表(用于 BM25 与文档长度);
    /// - `keywords`:该记忆的关键词 (词项, 权重)(用于共现图与余弦相似度)。
    pub fn set_doc(&mut self, id: MemoryId, terms: &[(String, u32)], keywords: &[(String, f32)]) {
        self.remove_doc(id);
        for (term, tf) in terms {
            if *tf == 0 {
                continue;
            }
            let entry = self.freqs.entry(term.clone()).or_default();
            entry.push((id, *tf));
            entry.sort_unstable_by_key(|(i, _)| *i);
        }
        let total: u32 = terms.iter().map(|(_, tf)| tf).sum();
        self.doc_len.insert(id, total);
        self.doc_terms.insert(id, keywords.to_vec());
        // 共现图:该文档关键词的两两组合(先去重,避免同一文档内重复计数)。
        let unique: BTreeSet<&str> = keywords.iter().map(|(t, _)| t.as_str()).collect();
        let kws: Vec<&str> = unique.into_iter().collect();
        for i in 0..kws.len() {
            for j in (i + 1)..kws.len() {
                bump(&mut self.cooc, kws[i], kws[j], 1);
                bump(&mut self.cooc, kws[j], kws[i], 1);
            }
        }
    }

    /// 清除一篇文档的全部检索统计。
    pub fn remove_doc(&mut self, id: MemoryId) {
        self.freqs.retain(|_, hits| {
            hits.retain(|(i, _)| *i != id);
            !hits.is_empty()
        });
        self.doc_len.remove(&id);
        if let Some(kws) = self.doc_terms.remove(&id) {
            let unique: BTreeSet<&str> = kws.iter().map(|(t, _)| t.as_str()).collect();
            let kws: Vec<&str> = unique.into_iter().collect();
            for i in 0..kws.len() {
                for j in (i + 1)..kws.len() {
                    decrement(&mut self.cooc, kws[i], kws[j]);
                    decrement(&mut self.cooc, kws[j], kws[i]);
                }
            }
        }
    }

    /// 已建立统计的文档数(BM25 的 N)。
    pub fn doc_count(&self) -> usize {
        self.doc_len.len()
    }

    /// 某文档的关键词权重向量(测试/诊断用)。
    pub fn doc_keywords(&self, id: MemoryId) -> Option<&[(String, f32)]> {
        self.doc_terms.get(&id).map(Vec::as_slice)
    }

    /// 至少包含一个查询词(含扩展词)的候选文档集合(升序)。
    pub fn docs_with_terms<'a>(&self, terms: impl IntoIterator<Item = &'a str>) -> BTreeSet<MemoryId> {
        let mut out = BTreeSet::new();
        for t in terms {
            if let Some(hits) = self.freqs.get(t) {
                out.extend(hits.iter().map(|(id, _)| *id));
            }
        }
        out
    }

    /// Okapi BM25 打分:查询向量 (词项, 权重) → {文档: 分数}。
    ///
    /// `score(q, d) = Σ_t qw_t · idf(t) · tf·(k1+1) / (tf + k1·(1 - b + b·|d|/avgdl))`
    pub fn bm25(&self, query: &[(String, f32)], cfg: &SearchConfig) -> BTreeMap<MemoryId, f32> {
        let n = self.doc_len.len() as f32;
        if n == 0.0 {
            return BTreeMap::new();
        }
        let total: u64 = self.doc_len.values().map(|l| u64::from(*l)).sum();
        let avgdl = (total as f32 / n).max(f32::EPSILON);
        let mut scores: BTreeMap<MemoryId, f32> = BTreeMap::new();
        for (term, qw) in query {
            let Some(hits) = self.freqs.get(term) else {
                continue;
            };
            let df = hits.len() as f32;
            // +1 保证 df 超过半数文档时 idf 仍非负
            let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln().max(0.0);
            if idf <= 0.0 {
                continue;
            }
            for (id, tf) in hits {
                let dl = self.doc_len.get(id).copied().unwrap_or(0).max(1) as f32;
                let tf = *tf as f32;
                let norm = tf * (cfg.k1 + 1.0) / (tf + cfg.k1 * (1.0 - cfg.b + cfg.b * dl / avgdl));
                *scores.entry(*id).or_insert(0.0) += qw * idf * norm;
            }
        }
        scores
    }

    /// 共现图查询扩展:从查询词出发走 `hops` 跳,返回 (扩展词, 权重) 按权重降序。
    ///
    /// 权重 = 查询词权重 × P(扩展词 | 查询词) × decay^跳数,
    /// 其中 P(other | term) = 共现文档数 / term 的文档频率(条件概率,天然归一化)。
    /// 原始查询词本身不会被当成扩展词返回。
    pub fn expand(&self, query: &[(String, f32)], cfg: &SearchConfig) -> Vec<(String, f32)> {
        if cfg.hops == 0 || cfg.expansion_limit == 0 {
            return Vec::new();
        }
        let seeds: BTreeSet<&str> = query.iter().map(|(t, _)| t.as_str()).collect();
        let mut acc: BTreeMap<String, f32> = BTreeMap::new();
        let mut frontier: Vec<(String, f32)> = query.to_vec();
        let mut decay = 1.0f32;
        for _ in 0..cfg.hops {
            decay *= cfg.expansion_decay;
            let mut next: Vec<(String, f32)> = Vec::new();
            for (term, qw) in &frontier {
                let Some(neighbors) = self.cooc.get(term) else {
                    continue;
                };
                let df = self.freqs.get(term).map_or(1, |h| h.len()).max(1);
                for (other, count) in neighbors {
                    if seeds.contains(other.as_str()) {
                        continue;
                    }
                    let p = *count as f32 / df as f32;
                    let w = qw * p * decay;
                    if w <= 0.0 {
                        continue;
                    }
                    *acc.entry(other.clone()).or_insert(0.0) += w;
                    next.push((other.clone(), w));
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        let mut out: Vec<(String, f32)> = acc.into_iter().collect();
        out.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        out.truncate(cfg.expansion_limit);
        out
    }

    /// 种子关键词向量与某文档关键词向量的余弦相似度(0..1)。
    pub fn cosine(&self, seed: &[(String, f32)], id: MemoryId) -> f32 {
        let Some(doc) = self.doc_terms.get(&id) else {
            return 0.0;
        };
        let seed_map = weight_map(seed);
        let n1 = norm(&seed_map);
        if n1 <= 0.0 {
            return 0.0;
        }
        let doc_map = weight_map(doc);
        let n2 = norm(&doc_map);
        if n2 <= 0.0 {
            return 0.0;
        }
        let mut dot = 0.0f32;
        for (term, w) in &seed_map {
            if let Some(d) = doc_map.get(term) {
                dot += w * d;
            }
        }
        (dot / (n1 * n2)).clamp(0.0, 1.0)
    }

    // ------- 快照序列化(索引快照的第 4 段)-------

    pub(crate) fn encode(&self, w: &mut Writer) {
        // freqs: term → [(id, tf)]
        w.varint(self.freqs.len() as u64);
        for (term, hits) in &self.freqs {
            w.str(term);
            w.varint(hits.len() as u64);
            for (id, tf) in hits {
                id.encode(w);
                w.varint(u64::from(*tf));
            }
        }
        // doc_len: id → 词条总数
        w.varint(self.doc_len.len() as u64);
        for (id, len) in &self.doc_len {
            id.encode(w);
            w.varint(u64::from(*len));
        }
        // doc_terms: id → [(term, weight)]
        w.varint(self.doc_terms.len() as u64);
        for (id, kws) in &self.doc_terms {
            id.encode(w);
            w.varint(kws.len() as u64);
            for (term, weight) in kws {
                w.str(term);
                weight.encode(w);
            }
        }
        // cooc: term → [(term, count)](双向各存一份)
        w.varint(self.cooc.len() as u64);
        for (term, others) in &self.cooc {
            w.str(term);
            w.varint(others.len() as u64);
            for (other, count) in others {
                w.str(other);
                w.varint(u64::from(*count));
            }
        }
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let nf = r.varint()? as usize;
        let mut freqs: BTreeMap<String, Vec<(MemoryId, u32)>> = BTreeMap::new();
        for _ in 0..nf {
            let term = r.str()?;
            let m = r.varint()? as usize;
            let mut hits = Vec::with_capacity(m.min(4096));
            for _ in 0..m {
                let id = r.u64()?;
                let tf = r.varint()? as u32;
                hits.push((id, tf));
            }
            freqs.insert(term, hits);
        }
        let nl = r.varint()? as usize;
        let mut doc_len: BTreeMap<MemoryId, u32> = BTreeMap::new();
        for _ in 0..nl {
            let id = r.u64()?;
            let len = r.varint()? as u32;
            doc_len.insert(id, len);
        }
        let nt = r.varint()? as usize;
        let mut doc_terms: BTreeMap<MemoryId, Vec<(String, f32)>> = BTreeMap::new();
        for _ in 0..nt {
            let id = r.u64()?;
            let m = r.varint()? as usize;
            let mut kws = Vec::with_capacity(m.min(4096));
            for _ in 0..m {
                let term = r.str()?;
                let weight = r.f32()?;
                kws.push((term, weight));
            }
            doc_terms.insert(id, kws);
        }
        let nc = r.varint()? as usize;
        let mut cooc: BTreeMap<String, BTreeMap<String, u32>> = BTreeMap::new();
        for _ in 0..nc {
            let term = r.str()?;
            let m = r.varint()? as usize;
            let mut others = BTreeMap::new();
            for _ in 0..m {
                let other = r.str()?;
                let count = r.varint()? as u32;
                others.insert(other, count);
            }
            cooc.insert(term, others);
        }
        Ok(SearchStats {
            freqs,
            doc_len,
            doc_terms,
            cooc,
        })
    }
}

/// 共现计数 +1。
fn bump(cooc: &mut BTreeMap<String, BTreeMap<String, u32>>, a: &str, b: &str, delta: u32) {
    *cooc
        .entry(a.to_string())
        .or_default()
        .entry(b.to_string())
        .or_insert(0) += delta;
}

/// 共现计数 -1(减到 0 时清理条目,防止图无限增长)。
fn decrement(cooc: &mut BTreeMap<String, BTreeMap<String, u32>>, a: &str, b: &str) {
    if let Some(inner) = cooc.get_mut(a) {
        if let Some(c) = inner.get_mut(b) {
            *c = c.saturating_sub(1);
            if *c == 0 {
                inner.remove(b);
            }
        }
        if inner.is_empty() {
            cooc.remove(a);
        }
    }
}

/// (词项, 权重) 列表 → 去重求和权重表。
fn weight_map(items: &[(String, f32)]) -> HashMap<&str, f32> {
    let mut map: HashMap<&str, f32> = HashMap::new();
    for (t, w) in items {
        *map.entry(t.as_str()).or_insert(0.0) += w;
    }
    map
}

/// 权重向量的 L2 范数。
fn norm(map: &HashMap<&str, f32>) -> f32 {
    map.values().map(|w| w * w).sum::<f32>().sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SearchConfig {
        SearchConfig::default()
    }

    fn tf(terms: &[(&str, u32)]) -> Vec<(String, u32)> {
        terms.iter().map(|(t, f)| ((*t).into(), *f)).collect()
    }

    fn kws(terms: &[(&str, f32)]) -> Vec<(String, f32)> {
        terms.iter().map(|(t, w)| ((*t).into(), *w)).collect()
    }

    fn sample() -> SearchStats {
        let mut s = SearchStats::new();
        // 文档 1:rust 出现 2 次,主题 rust/所有权
        s.set_doc(1, &tf(&[("rust", 2), ("所有权", 1), ("内存", 1)]), &kws(&[("rust", 1.0), ("所有权", 0.5)]));
        // 文档 2:rust 1 次,主题 rust/借用
        s.set_doc(2, &tf(&[("rust", 1), ("借用", 2)]), &kws(&[("rust", 0.8), ("借用", 0.4)]));
        // 文档 3:无关文档(数据库)
        s.set_doc(3, &tf(&[("数据库", 2), ("索引", 1)]), &kws(&[("数据库", 1.0), ("索引", 0.6)]));
        s
    }

    #[test]
    fn set_and_remove_doc() {
        let mut s = sample();
        assert_eq!(s.doc_count(), 3);
        assert_eq!(s.doc_keywords(1).unwrap().len(), 2);
        s.remove_doc(1);
        assert_eq!(s.doc_count(), 2);
        // 倒排与共现都应清理
        assert!(s.docs_with_terms(["内存"]).is_empty());
        assert!(s.docs_with_terms(["所有权"]).is_empty());
        assert!(s.docs_with_terms(["rust"]).contains(&2));
        // 只出现在 1 中的词(所有权)失去全部共现 → 无扩展;
        // 借用 与 rust 在文档 2 内共现,删除 1 后仍可扩展出 rust
        assert!(s.expand(&kws(&[("所有权", 1.0)]), &cfg()).is_empty());
        let exp = s.expand(&kws(&[("借用", 1.0)]), &cfg());
        assert_eq!(exp.len(), 1);
        assert_eq!(exp[0].0, "rust");
    }

    #[test]
    fn remove_doc_keeps_other_pairs() {
        let mut s = sample();
        // 4 与 5 共现 rust/并发
        s.set_doc(4, &tf(&[("rust", 1), ("并发", 1)]), &kws(&[("rust", 1.0), ("并发", 0.5)]));
        s.set_doc(5, &tf(&[("rust", 1), ("并发", 1)]), &kws(&[("rust", 1.0), ("并发", 0.5)]));
        // 删除 4:rust↔并发 计数 2→1(文档 5 的贡献保留),其余共现不受影响
        s.remove_doc(4);
        let q = kws(&[("rust", 1.0)]);
        let exp = s.expand(&q, &cfg());
        let terms: Vec<&str> = exp.iter().map(|(t, _)| t.as_str()).collect();
        assert!(terms.contains(&"所有权"), "{terms:?}");
        assert!(terms.contains(&"借用"), "{terms:?}");
        assert!(terms.contains(&"并发"), "{terms:?}");
        // 再删除 5:rust↔并发 归零 → 并发 从扩展中消失,其余仍在
        s.remove_doc(5);
        let exp = s.expand(&q, &cfg());
        let terms: Vec<&str> = exp.iter().map(|(t, _)| t.as_str()).collect();
        assert!(!terms.contains(&"并发"), "{terms:?}");
        assert!(terms.contains(&"所有权"), "{terms:?}");
    }

    #[test]
    fn bm25_ranks_more_relevant_doc_first() {
        let s = sample();
        let query = kws(&[("rust", 1.0)]);
        let scores = s.bm25(&query, &cfg());
        // rust 在文档 1 出现 2 次(tf 更高)→ 分数应高于文档 2
        assert!(scores[&1] > scores[&2]);
        // 没出现 rust 的文档 3 不在候选内
        assert!(!scores.contains_key(&3));
    }

    #[test]
    fn bm25_respects_length_normalization() {
        let mut s = SearchStats::new();
        // 两个文档 rust 都出现 1 次,但文档 1 更长 → b>0 时文档 2 分数更高
        s.set_doc(1, &tf(&[("rust", 1), ("填充", 10)]), &kws(&[("rust", 1.0)]));
        s.set_doc(2, &tf(&[("rust", 1)]), &kws(&[("rust", 1.0)]));
        let q = kws(&[("rust", 1.0)]);
        let with_norm = s.bm25(&q, &cfg());
        assert!(with_norm[&2] > with_norm[&1], "长文档应被归一化降权");
        let mut no_norm = cfg();
        no_norm.b = 0.0;
        let flat = s.bm25(&q, &no_norm);
        // b=0 时与长度无关,两者接近(仅 avgdl 相同 → 相等)
        assert!((flat[&1] - flat[&2]).abs() < 1e-5);
    }

    #[test]
    fn expand_uses_cooccurrence() {
        let s = sample();
        let query = kws(&[("rust", 1.0)]);
        let expanded = s.expand(&query, &cfg());
        // rust 与 所有权/借用 共现 → 都应被扩展出来
        let terms: Vec<&str> = expanded.iter().map(|(t, _)| t.as_str()).collect();
        assert!(terms.contains(&"所有权"));
        assert!(terms.contains(&"借用"));
        // 与 rust 从未共现的 数据库/索引 不能出现
        assert!(!terms.contains(&"数据库"));
        assert!(!terms.contains(&"索引"));
        // 权重降序
        assert!(expanded.windows(2).all(|p| p[0].1 >= p[1].1 - 1e-6));
    }

    #[test]
    fn expand_disabled_and_capped() {
        let s = sample();
        let query = kws(&[("rust", 1.0)]);
        let mut no_hops = cfg();
        no_hops.hops = 0;
        assert!(s.expand(&query, &no_hops).is_empty());
        let mut capped = cfg();
        capped.expansion_limit = 1;
        assert_eq!(s.expand(&query, &capped).len(), 1);
    }

    #[test]
    fn cosine_similarity() {
        let s = sample();
        let seed = kws(&[("rust", 1.0), ("所有权", 0.5)]);
        let self_cos = s.cosine(&seed, 1);
        assert!((self_cos - 1.0).abs() < 1e-5, "自身应为 1");
        let other_cos = s.cosine(&seed, 2);
        // 文档 2 只共享 rust → 相似度应在 (0,1) 之间
        assert!(other_cos > 0.0 && other_cos < 1.0);
        // 文档 3 无共享词 → 0
        assert_eq!(s.cosine(&seed, 3), 0.0);
    }

    #[test]
    fn snapshot_roundtrip() {
        let s = sample();
        let mut w = Writer::new();
        s.encode(&mut w);
        let bytes = w.into_vec();
        let mut r = Reader::new(&bytes);
        let back = SearchStats::decode(&mut r).unwrap();
        assert_eq!(r.remaining(), 0);
        assert_eq!(back.doc_count(), 3);
        assert_eq!(back.doc_keywords(1).unwrap(), &kws(&[("rust", 1.0), ("所有权", 0.5)]));
        // 打分能力保持一致
        let q = kws(&[("rust", 1.0)]);
        assert_eq!(back.bm25(&q, &cfg()), s.bm25(&q, &cfg()));
        assert_eq!(back.expand(&q, &cfg()), s.expand(&q, &cfg()));
        assert_eq!(back.cosine(&q, 1), s.cosine(&q, 1));
    }

    #[test]
    fn empty_stats_encode_decode() {
        let s = SearchStats::new();
        let mut w = Writer::new();
        s.encode(&mut w);
        let bytes = w.into_vec();
        let mut r = Reader::new(&bytes);
        let back = SearchStats::decode(&mut r).unwrap();
        assert_eq!(r.remaining(), 0);
        assert_eq!(back.doc_count(), 0);
        assert!(back.bm25(&kws(&[("rust", 1.0)]), &cfg()).is_empty());
    }
}
