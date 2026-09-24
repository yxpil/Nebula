//! 存储后端抽象:单文件 [`Database`](crate::Database) 与目录集群(nebula-cluster)
//! 实现同一接口,executor 与 server 无需感知数据物理布局。

use nebula_core::{MemoryId, MemoryRecord, Result};
use nebula_sql::ast::{CacheTarget, RelatedSeed};

use crate::auth::UserDirectory;

/// 一条带库归属与相关度的检索行(SEARCH / RELATED 的跨库通用结果)。
#[derive(Debug, Clone, PartialEq)]
pub struct RankedRow {
    pub db: String,
    pub id: MemoryId,
    pub score: f32,
}

impl RankedRow {
    pub fn new(db: impl Into<String>, id: MemoryId, score: f32) -> Self {
        RankedRow {
            db: db.into(),
            id,
            score,
        }
    }
}

/// 记忆存储后端:库管理 + CRUD + AI 检索 + 缓存/状态。
///
/// 权限由 executor 在调用这些方法**之前**通过
/// [`crate::auth::UserDirectory`] 校验;实现只负责数据正确性。
pub trait MemBackend {
    // ------- 逻辑库管理 -------

    /// 全部逻辑库名(字典序)。
    fn list_dbs(&self) -> Vec<String>;

    fn db_exists(&self, db: &str) -> bool;

    /// 新建逻辑库;已存在返回 false。
    fn create_db(&mut self, db: &str) -> Result<bool>;

    /// 删除逻辑库(含全部记录);默认库受保护。
    fn drop_db(&mut self, db: &str) -> Result<()>;

    /// 挂接外部 .ndb 文件(集群模式;单文件模式不支持)。
    fn attach_file(&mut self, path: &str, name: &str) -> Result<()>;

    /// 取消挂接(集群模式;单文件模式不支持)。
    fn detach_db(&mut self, name: &str) -> Result<()>;

    /// USE 切换后的回调:按该库历史热度预加载热点文档。
    fn on_use(&mut self, db: &str) -> Result<()>;

    // ------- CRUD -------

    #[allow(clippy::too_many_arguments)]
    fn insert_mem(
        &mut self,
        db: &str,
        content: String,
        tags: Vec<String>,
        source: String,
        importance: f32,
    ) -> Result<MemoryId>;

    fn fetch_mem(&mut self, db: &str, id: MemoryId) -> Result<Option<MemoryRecord>>;

    fn replace_mem(&mut self, db: &str, record: &MemoryRecord) -> Result<()>;

    fn delete_mems(&mut self, db: &str, ids: &[MemoryId]) -> Result<u64>;

    // ------- 索引只读(SELECT 索引优先路径)-------

    /// 关键词命中 (id, 权重)。
    fn keyword_hits(&self, db: &str, term: &str) -> Vec<(MemoryId, f32)>;

    /// 标签命中 id。
    fn tag_hits(&self, db: &str, tag: &str) -> Vec<MemoryId>;

    /// 库内全部 id。
    fn all_ids(&self, db: &str) -> Vec<MemoryId>;

    // ------- AI 检索 -------

    /// 跨库 BM25 检索;返回按相关度降序、已截断到 limit 的行。
    fn search_ranked(
        &mut self,
        dbs: &[String],
        query: &str,
        limit: usize,
    ) -> Result<Vec<RankedRow>>;

    /// 跨库联想;种子为 Id(某库)/ QualifiedId / Text;
    /// 返回按相关度降序、已截断到 limit 的行(种子自身不出现)。
    fn related_ranked(
        &mut self,
        dbs: &[String],
        seed: RelatedSeed,
        limit: usize,
    ) -> Result<Vec<RankedRow>>;

    // ------- 缓存 / 状态 -------

    /// SHOW CACHE 的 (变量, 值) 行。
    fn cache_rows(&self) -> Vec<Vec<String>>;

    /// CLEAR CACHE:清空查询/文档缓存并重置计数。
    fn clear_caches(&mut self);

    /// SET CACHE:在线调整容量。
    fn set_cache_capacity(&mut self, target: CacheTarget, capacity: usize);

    /// SHOW HOT:(id, heat, cached) 行。
    fn hot_rows(&self, db: &str, limit: usize) -> Vec<Vec<String>>;

    /// SHOW STATUS:(变量, 值) 行。
    fn status_rows(&self) -> Vec<Vec<String>>;

    /// CHECKPOINT:同步索引快照。
    fn checkpoint(&mut self) -> Result<()>;

    /// 开启/关闭持久化延迟(事务期间由 executor 控制):
    /// - 开启时,写操作只改内存数据结构,**不提交文件 catalog、不写索引快照**;
    /// - 关闭后,调用方通常紧跟一次 [`checkpoint`](Self::checkpoint) 落盘。
    ///
    /// 这样进程在事务中崩溃后,重开的文件完全处于事务前状态。
    fn set_persistence_deferred(&mut self, deferred: bool);
}

/// 会话后端:同时提供数据操作([`MemBackend`])与用户目录([`UserDirectory`])。
///
/// executor / server 统一接收该对象:单文件模式直接用 Database
/// (内置全权用户语义),目录模式用 nebula-cluster 的 Cluster。
pub trait SessionBackend: MemBackend + UserDirectory {}

impl<T: MemBackend + UserDirectory> SessionBackend for T {}
