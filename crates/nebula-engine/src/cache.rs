//! 查询缓存与文档缓存(热点加载),零外部依赖的 LRU 实现。
//!
//! 两层缓存:
//! 1. **查询缓存**([`QueryCache`]):缓存 SEARCH / RELATED 的相关度排序结果
//!    (仅 `(id, score)` 列表,不缓存记录文本),重复检索免去 BM25 打分、
//!    共现图扩展与重排;
//! 2. **文档缓存**([`DocCache`]):LRU 缓存 [`MemoryRecord`],避免同一文档
//!    被反复从加密存储页读出并反序列化。
//!
//! 多库维度:缓存键都带库名(查询缓存带库列表,文档缓存带 (库, id)),
//! 不同库的 id 各自独立,互不串味。
//!
//! 正确性约定:
//! - 任何写操作(INSERT / UPDATE / DELETE)都会改变 BM25 的 df / avgdl,
//!   写操作必须调用 [`QueryCache::invalidate`] 全量失效查询缓存;
//! - 文档缓存由写路径同步维护(fill / evict),所有读路径经
//!   [`crate::Database::fetch_record`] 自动填充与 LRU 更新;
//! - 容量为 0 表示关闭对应缓存(查询缓存恒 miss,文档缓存不保存记录)。

use std::collections::{BTreeMap, HashMap};

use nebula_core::{MemoryId, MemoryRecord};

/// 文档缓存键:(逻辑库, 记忆 id)。不同库的 id 各自独立。
pub type DocKey = (String, MemoryId);

/// 库列表归一化为缓存键片段(排序 + 逗号连接,屏蔽调用方顺序差异)。
fn dbs_key(dbs: &[String]) -> String {
    let mut sorted: Vec<&str> = dbs.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.join(",")
}

/// SEARCH 语句的缓存键(库列表 + 查询文本 + 生效 limit)。
pub fn search_key(dbs: &[String], query: &str, limit: usize) -> String {
    format!("s:{limit}\u{1}{}\u{1}{query}", dbs_key(dbs))
}

/// RELATED TO <id> 的缓存键。
pub fn related_id_key(dbs: &[String], id: MemoryId, limit: usize) -> String {
    format!("ri:{limit}\u{1}{}\u{1}{id}", dbs_key(dbs))
}

/// RELATED '文本' 的缓存键。
pub fn related_text_key(dbs: &[String], text: &str, limit: usize) -> String {
    format!("rt:{limit}\u{1}{}\u{1}{text}", dbs_key(dbs))
}

/// 查询缓存:SEARCH / RELATED 排序结果的 LRU 缓存。
///
/// 键为语句派生串,值为相关度降序的 `(库, id, score)` 列表
/// (不同库 id 可重复,必须带库名)。写操作后整体失效(见
/// [`QueryCache::invalidate`]);命中/未命中/淘汰与失效世代(`epoch`)
/// 均有计数,供 SHOW CACHE 展示。
pub struct QueryCache {
    capacity: usize,
    entries: HashMap<String, Vec<(String, MemoryId, f32)>>,
    /// key → LRU 序号(越大越新)。
    seq_of: HashMap<String, u64>,
    /// LRU 序号 → key(按序号升序即最久未用在前)。
    lru: BTreeMap<u64, String>,
    seq: u64,
    /// 失效世代:每次写操作失效 +1(仅统计展示,条目直接被清空)。
    epoch: u64,
    hits: u64,
    misses: u64,
    evictions: u64,
}

impl QueryCache {
    pub fn new(capacity: usize) -> Self {
        QueryCache {
            capacity,
            entries: HashMap::new(),
            seq_of: HashMap::new(),
            lru: BTreeMap::new(),
            seq: 0,
            epoch: 0,
            hits: 0,
            misses: 0,
            evictions: 0,
        }
    }

    /// 查询缓存;命中时刷新 LRU 并返回克隆的排序列表。
    pub fn get(&mut self, key: &str) -> Option<Vec<(String, MemoryId, f32)>> {
        if let Some(list) = self.entries.get(key) {
            self.hits += 1;
            let out = list.clone();
            self.touch(key);
            return Some(out);
        }
        self.misses += 1;
        None
    }

    /// 写入/刷新一条排序结果;超出容量时淘汰最久未用的条目。
    pub fn put(&mut self, key: String, ranked: Vec<(String, MemoryId, f32)>) {
        if self.capacity == 0 {
            return;
        }
        self.seq += 1;
        let seq = self.seq;
        self.entries.insert(key.clone(), ranked);
        if let Some(old) = self.seq_of.insert(key.clone(), seq) {
            self.lru.remove(&old);
        }
        self.lru.insert(seq, key);
        while self.entries.len() > self.capacity {
            self.evict_lru();
        }
    }

    /// 写操作后调用:世代 +1 并清空全部条目(旧排序不再可信)。
    pub fn invalidate(&mut self) {
        self.epoch += 1;
        self.entries.clear();
        self.seq_of.clear();
        self.lru.clear();
    }

    /// 显式清空缓存并把命中/未命中/淘汰计数清零(CLEAR CACHE)。
    pub fn clear(&mut self) {
        self.invalidate();
        self.hits = 0;
        self.misses = 0;
        self.evictions = 0;
    }

    /// 在线调整容量(缩小即按 LRU 淘汰;设为 0 关闭查询缓存)。
    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity;
        while self.entries.len() > self.capacity {
            self.evict_lru();
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn hits(&self) -> u64 {
        self.hits
    }

    pub fn misses(&self) -> u64 {
        self.misses
    }

    pub fn evictions(&self) -> u64 {
        self.evictions
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// 命中后把 key 移到 LRU 最新位置。
    fn touch(&mut self, key: &str) {
        if let Some(old) = self.seq_of.get(key).copied() {
            self.lru.remove(&old);
            self.seq += 1;
            let seq = self.seq;
            self.seq_of.insert(key.to_string(), seq);
            self.lru.insert(seq, key.to_string());
        }
    }

    fn evict_lru(&mut self) {
        if let Some((seq, key)) = self.lru.iter().next().map(|(s, k)| (*s, k.clone())) {
            self.lru.remove(&seq);
            self.seq_of.remove(&key);
            self.entries.remove(&key);
            self.evictions += 1;
        }
    }
}

/// 文档缓存:记忆记录的 LRU 缓存(避免重复解密存储页)。
///
/// 键为 (逻辑库, id);读路径命中即刷新 LRU,写路径 fill(INSERT / UPDATE)
/// 或 evict(DELETE);切换库时可按历史热度预填充(见 [`crate::Database`])。
pub struct DocCache {
    capacity: usize,
    entries: HashMap<DocKey, MemoryRecord>,
    seq_of: HashMap<DocKey, u64>,
    lru: BTreeMap<u64, DocKey>,
    seq: u64,
    hits: u64,
    misses: u64,
    evictions: u64,
}

impl DocCache {
    pub fn new(capacity: usize) -> Self {
        DocCache {
            capacity,
            entries: HashMap::new(),
            seq_of: HashMap::new(),
            lru: BTreeMap::new(),
            seq: 0,
            hits: 0,
            misses: 0,
            evictions: 0,
        }
    }

    /// 读取文档;命中时刷新 LRU 并返回克隆。容量为 0 时恒为 None(缓存关闭)。
    pub fn get(&mut self, db: &str, id: MemoryId) -> Option<MemoryRecord> {
        let key: DocKey = (db.to_string(), id);
        if let Some(rec) = self.entries.get(&key) {
            self.hits += 1;
            let out = rec.clone();
            self.touch(key);
            return Some(out);
        }
        self.misses += 1;
        None
    }

    pub fn contains(&self, db: &str, id: MemoryId) -> bool {
        self.entries.contains_key(&(db.to_string(), id))
    }

    /// 写入/刷新一条记录;超出容量时淘汰最久未用的记录。
    pub fn put(&mut self, db: &str, id: MemoryId, record: MemoryRecord) {
        if self.capacity == 0 {
            return;
        }
        self.seq += 1;
        let seq = self.seq;
        let key: DocKey = (db.to_string(), id);
        self.entries.insert(key.clone(), record);
        if let Some(old) = self.seq_of.insert(key.clone(), seq) {
            self.lru.remove(&old);
        }
        self.lru.insert(seq, key);
        while self.entries.len() > self.capacity {
            self.evict_lru();
        }
    }

    /// 删除记录时调用:移除缓存条目(不计入淘汰统计)。
    pub fn evict(&mut self, db: &str, id: MemoryId) {
        let key: DocKey = (db.to_string(), id);
        if let Some(old) = self.seq_of.remove(&key) {
            self.lru.remove(&old);
            self.entries.remove(&key);
        }
    }

    /// 删除整个逻辑库时调用:清空该库的全部缓存条目(不计入淘汰统计)。
    pub fn evict_db(&mut self, db: &str) {
        let keys: Vec<DocKey> = self
            .entries
            .keys()
            .filter(|(name, _)| name == db)
            .cloned()
            .collect();
        for key in keys {
            if let Some(old) = self.seq_of.remove(&key) {
                self.lru.remove(&old);
            }
            self.entries.remove(&key);
        }
    }

    /// 在线调整容量(缩小即按 LRU 淘汰;设为 0 关闭文档缓存)。
    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity;
        while self.entries.len() > self.capacity {
            self.evict_lru();
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn hits(&self) -> u64 {
        self.hits
    }

    pub fn misses(&self) -> u64 {
        self.misses
    }

    pub fn evictions(&self) -> u64 {
        self.evictions
    }

    fn touch(&mut self, key: DocKey) {
        if let Some(old) = self.seq_of.get(&key).copied() {
            self.lru.remove(&old);
            self.seq += 1;
            let seq = self.seq;
            self.seq_of.insert(key.clone(), seq);
            self.lru.insert(seq, key);
        }
    }

    fn evict_lru(&mut self) {
        if let Some((seq, key)) = self.lru.iter().next().map(|(s, k)| (*s, k.clone())) {
            self.lru.remove(&seq);
            self.seq_of.remove(&key);
            self.entries.remove(&key);
            self.evictions += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DB: &str = "main";

    fn dbs() -> Vec<String> {
        vec![DB.to_string()]
    }

    fn ranked(db: &str, ids: &[u64]) -> Vec<(String, MemoryId, f32)> {
        ids.iter()
            .enumerate()
            .map(|(i, id)| (db.to_string(), *id, 1.0 - i as f32 * 0.1))
            .collect()
    }

    #[test]
    fn query_cache_put_get_and_lru() {
        let mut c = QueryCache::new(2);
        c.put(search_key(&dbs(), "rust", 10), ranked(DB, &[1, 2]));
        c.put(search_key(&dbs(), "db", 10), ranked(DB, &[3]));
        // 命中第一个键会刷新它的 LRU 位置
        assert_eq!(c.get(&search_key(&dbs(), "rust", 10)), Some(ranked(DB, &[1, 2])));
        // 插入第三个键:最久未用的 "db" 被淘汰,而非刚访问过的 "rust"
        c.put(search_key(&dbs(), "go", 10), ranked(DB, &[4]));
        assert_eq!(c.len(), 2);
        assert!(c.get(&search_key(&dbs(), "db", 10)).is_none());
        assert!(c.get(&search_key(&dbs(), "rust", 10)).is_some());
        assert!(c.get(&search_key(&dbs(), "go", 10)).is_some());
        assert_eq!(c.evictions(), 1);
    }

    #[test]
    fn query_cache_distinguishes_limit_and_kind() {
        let mut c = QueryCache::new(4);
        c.put(search_key(&dbs(), "rust", 5), ranked(DB, &[1]));
        c.put(search_key(&dbs(), "rust", 10), ranked(DB, &[1, 2]));
        c.put(related_id_key(&dbs(), 7, 5), ranked(DB, &[3]));
        c.put(related_text_key(&dbs(), "rust", 5), ranked(DB, &[4]));
        assert_eq!(c.get(&search_key(&dbs(), "rust", 5)), Some(ranked(DB, &[1])));
        assert_eq!(c.get(&search_key(&dbs(), "rust", 10)), Some(ranked(DB, &[1, 2])));
        assert_eq!(c.get(&related_id_key(&dbs(), 7, 5)), Some(ranked(DB, &[3])));
        assert_eq!(c.get(&related_text_key(&dbs(), "rust", 5)), Some(ranked(DB, &[4])));
        assert_eq!(c.len(), 4);
    }

    #[test]
    fn query_cache_distinguishes_db_lists() {
        let one = vec!["main".to_string()];
        let two = vec!["main".to_string(), "work".to_string()];
        let mut c = QueryCache::new(4);
        c.put(search_key(&one, "rust", 10), ranked("main", &[1]));
        c.put(search_key(&two, "rust", 10), ranked("work", &[1]));
        assert_eq!(c.get(&search_key(&one, "rust", 10)), Some(ranked("main", &[1])));
        assert_eq!(c.get(&search_key(&two, "rust", 10)), Some(ranked("work", &[1])));
        // 库列表顺序不同但内容相同 → 同一键
        let two_reversed = vec!["work".to_string(), "main".to_string()];
        assert_eq!(
            c.get(&search_key(&two_reversed, "rust", 10)),
            Some(ranked("work", &[1]))
        );
    }

    #[test]
    fn query_cache_disabled_when_zero_capacity() {
        let mut c = QueryCache::new(0);
        c.put(search_key(&dbs(), "rust", 5), ranked(DB, &[1]));
        assert_eq!(c.len(), 0);
        assert!(c.get(&search_key(&dbs(), "rust", 5)).is_none());
        assert_eq!(c.misses(), 1);
        assert_eq!(c.hits(), 0);
    }

    #[test]
    fn query_cache_set_capacity_evicts() {
        let mut c = QueryCache::new(3);
        for i in 0..3 {
            c.put(search_key(&dbs(), &format!("q{i}"), 10), ranked(DB, &[i]));
        }
        // 缩小到 1:按 LRU 顺序淘汰,保留最近写入的
        c.set_capacity(1);
        assert_eq!(c.len(), 1);
        assert!(c.get(&search_key(&dbs(), "q2", 10)).is_some());
        // 设为 0:全部清空(关闭)
        c.put(search_key(&dbs(), "q3", 10), ranked(DB, &[9]));
        c.set_capacity(0);
        assert_eq!(c.len(), 0);
        assert!(c.get(&search_key(&dbs(), "q3", 10)).is_none());
    }

    #[test]
    fn query_cache_invalidate_and_clear() {
        let mut c = QueryCache::new(4);
        c.put(search_key(&dbs(), "rust", 5), ranked(DB, &[1]));
        c.get(&search_key(&dbs(), "rust", 5)); // hit
        c.get(&search_key(&dbs(), "go", 5)); // miss
        c.invalidate();
        assert_eq!(c.len(), 0, "写操作后必须整体失效");
        assert_eq!(c.epoch(), 1);
        assert!(c.get(&search_key(&dbs(), "rust", 5)).is_none());
        // clear 清空并清零计数
        c.put(search_key(&dbs(), "rust", 5), ranked(DB, &[1]));
        c.clear();
        assert_eq!(c.len(), 0);
        assert_eq!((c.hits(), c.misses(), c.evictions()), (0, 0, 0));
        assert_eq!(c.epoch(), 2);
    }

    #[test]
    fn doc_cache_put_get_and_lru() {
        let mut c = DocCache::new(2);
        let rec = |n: u64| MemoryRecord::new(format!("content {n}"));
        c.put(DB, 1, rec(1));
        c.put(DB, 2, rec(2));
        assert!(c.contains(DB, 1));
        assert_eq!(c.get(DB, 1).unwrap().content, "content 1");
        // 插入第三个:最久未用的 2 被淘汰
        c.put(DB, 3, rec(3));
        assert!(c.get(DB, 2).is_none(), "已淘汰的文档不应命中");
        assert!(c.contains(DB, 1));
        assert!(c.contains(DB, 3));
        assert_eq!(c.evictions(), 1);
        assert_eq!(c.hits(), 1);
        assert_eq!(c.misses(), 1);
    }

    #[test]
    fn doc_cache_same_id_in_different_dbs_is_independent() {
        let mut c = DocCache::new(4);
        c.put("main", 1, MemoryRecord::new("main content"));
        c.put("work", 1, MemoryRecord::new("work content"));
        assert_eq!(c.get("main", 1).unwrap().content, "main content");
        assert_eq!(c.get("work", 1).unwrap().content, "work content");
        c.evict("main", 1);
        assert!(!c.contains("main", 1));
        assert!(c.contains("work", 1), "另一库的同 id 条目不受影响");
    }

    #[test]
    fn doc_cache_evict_and_set_capacity() {
        let mut c = DocCache::new(3);
        for id in 1..=3 {
            c.put(DB, id, MemoryRecord::new(format!("c{id}")));
        }
        c.evict(DB, 2);
        assert!(!c.contains(DB, 2));
        assert_eq!(c.len(), 2);
        c.set_capacity(1);
        assert_eq!(c.len(), 1);
        assert!(c.contains(DB, 3));
        c.set_capacity(0);
        assert!(!c.contains(DB, 3));
        assert_eq!(c.len(), 0);
        assert!(c.get(DB, 3).is_none(), "容量 0 时文档缓存关闭");
    }

    #[test]
    fn doc_cache_disabled_when_zero_capacity() {
        let mut c = DocCache::new(0);
        c.put(DB, 1, MemoryRecord::new("x"));
        assert!(!c.contains(DB, 1));
        assert_eq!(c.len(), 0);
        assert!(c.get(DB, 1).is_none());
        assert_eq!(c.misses(), 1);
    }
}
