//! 存储层门面:把 Pager + 目录页 + 记录链 + 快照链组合成
//! 对引擎层友好的高层 API。引擎只跟 `MemoryFile` 打交道,
//! 不直接感知页与加密细节。

use std::path::{Path, PathBuf};

use nebula_core::{RecordLocation, Result};

use crate::catalog::CatalogPayload;
use crate::pager::{Pager, DEFAULT_PAGE_SIZE};
use crate::{records, snapshot};

/// 打开的记忆数据库文件。持有:
/// - [`Pager`] 页访问与加解密
/// - [`CatalogPayload`] 内存态目录(权威)
pub struct MemoryFile {
    path: PathBuf,
    pager: Pager,
    catalog: CatalogPayload,
    /// 自上次检查点以来的记录变更数(供引擎决定何时 auto-checkpoint)。
    dirty_records: u64,
}

pub struct OpenInfo {
    pub page_size: u32,
    pub page_count: u64,
    pub free_pages: usize,
    pub memory_count: u64,
    pub checkpoint_seq: u64,
}

impl MemoryFile {
    /// 创建新库(文件必须不存在)。
    pub fn create(path: impl AsRef<Path>, password: &str, page_size: u32) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let catalog = CatalogPayload::empty();
        let pager = Pager::create(&path, password, page_size, &catalog)?;
        Ok(MemoryFile {
            path,
            pager,
            catalog,
            dirty_records: 0,
        })
    }

    /// 打开已有库。密码错误返回 [`Error::wrong_password`](nebula_core::Error::wrong_password)。
    pub fn open(path: impl AsRef<Path>, password: &str) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let (pager, catalog) = Pager::open(&path, password)?;
        Ok(MemoryFile {
            path,
            pager,
            catalog,
            dirty_records: 0,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    // ------- 记录 -------

    /// 写入一条编码后的记录,返回物理位置。调用方负责在目录中登记。
    pub fn put_record(&mut self, bytes: &[u8]) -> Result<RecordLocation> {
        let loc = records::write_record(&mut self.pager, &mut self.catalog, bytes)?;
        self.dirty_records += 1;
        Ok(loc)
    }

    pub fn read_record(&mut self, loc: &RecordLocation) -> Result<Vec<u8>> {
        records::read_record(&mut self.pager, loc)
    }

    pub fn free_record(&mut self, loc: &RecordLocation) -> Result<()> {
        records::free_record(&mut self.pager, &mut self.catalog, loc)
    }

    // ------- 索引快照 -------

    pub fn write_index_snapshot(&mut self, bytes: &[u8]) -> Result<()> {
        snapshot::write_snapshot(&mut self.pager, &mut self.catalog, bytes)?;
        self.dirty_records = 0;
        Ok(())
    }

    pub fn read_index_snapshot(&mut self) -> Result<Vec<u8>> {
        snapshot::read_snapshot(&mut self.pager, &self.catalog)
    }

    // ------- 目录字段 -------

    pub fn next_record_id(&self) -> u64 {
        self.catalog.next_record_id
    }

    pub fn set_next_record_id(&mut self, id: u64) {
        self.catalog.next_record_id = id;
    }

    pub fn bump_next_record_id(&mut self) -> u64 {
        let id = self.catalog.next_record_id;
        self.catalog.next_record_id += 1;
        id
    }

    pub fn memory_count(&self) -> u64 {
        self.catalog.memory_count
    }

    pub fn set_memory_count(&mut self, n: u64) {
        self.catalog.memory_count = n;
    }

    pub fn checkpoint_seq(&self) -> u64 {
        self.catalog.checkpoint_seq
    }

    pub fn dirty_records(&self) -> u64 {
        self.dirty_records
    }

    // ------- 持久化 -------

    /// 把内存目录写入目录页并 fsync(不重写索引快照)。
    pub fn commit_catalog(&mut self) -> Result<()> {
        self.pager.write_catalog(&self.catalog)?;
        self.pager.sync()
    }

    pub fn info(&self) -> OpenInfo {
        OpenInfo {
            page_size: self.pager.page_size() as u32,
            page_count: self.pager.page_count(),
            free_pages: self.pager.free_page_count(&self.catalog),
            memory_count: self.catalog.memory_count,
            checkpoint_seq: self.catalog.checkpoint_seq,
        }
    }

    /// 默认页大小(供 CLI 展示)。
    pub fn default_page_size() -> u32 {
        DEFAULT_PAGE_SIZE
    }
}
