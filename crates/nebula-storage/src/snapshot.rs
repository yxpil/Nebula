//! 索引快照链:引擎内存态索引(记录目录 + 倒排 + 标签)的落盘载体。
//!
//! 写入策略(崩溃安全的关键):
//! 1. 先在**新页**写出全部快照分片并 fsync;
//! 2. 更新目录页(index_root 指向新链)并 fsync —— 单页写入视为原子提交;
//! 3. 最后才把旧快照页释放进空闲链。
//! 任何时刻崩溃,旧链始终可用,不会出现半个新快照可见的情况。

use nebula_core::codec::{Reader, Writer};
use nebula_core::Result;

use crate::catalog::CatalogPayload;
use crate::page::ROLE_SNAPSHOT;
use crate::pager::Pager;

/// 快照单页载荷的页内头部开销。
const SNAPSHOT_PAGE_HEADER_MAX: usize = 1 + 4 + 4 + 8 + 5;

/// 写入快照字节流,返回新链首页页码。
pub fn write_snapshot(pager: &mut Pager, catalog: &mut CatalogPayload, bytes: &[u8]) -> Result<u64> {
    let capacity = pager.payload_capacity().saturating_sub(SNAPSHOT_PAGE_HEADER_MAX);
    if capacity == 0 {
        return Err(nebula_core::Error::Storage("page size too small".into()));
    }
    let total_parts = ((bytes.len() + capacity - 1) / capacity).max(1) as u32;

    // 1. 全部分配到新页(可能来自空闲链),写盘
    let mut pages = Vec::with_capacity(total_parts as usize);
    for _ in 0..total_parts {
        pages.push(pager.allocate(catalog)?);
    }
    let mut offset = 0usize;
    for (part, &page_no) in pages.iter().enumerate() {
        let end = (offset + capacity).min(bytes.len());
        let chunk = &bytes[offset..end];
        let next_page = pages.get(part + 1).copied().unwrap_or(0);
        let mut w = Writer::new();
        w.varint(chunk.len() as u64);
        w.u64(next_page);
        w.u32(part as u32);
        w.u32(total_parts);
        w.bytes(chunk);
        pager.write_payload(page_no, ROLE_SNAPSHOT, &w.into_vec())?;
        offset = end;
    }
    pager.sync()?; // 数据页先落盘

    // 2. 提交:目录页指向新链
    let old_root = catalog.index_root;
    catalog.index_root = pages[0];
    catalog.checkpoint_seq += 1;
    pager.write_catalog(catalog)?;
    pager.sync()?;

    // 3. 释放旧链(此时新链已提交)
    if old_root != 0 {
        free_chain(pager, catalog, old_root)?;
        pager.sync()?;
    }
    Ok(pages[0])
}

/// 读取快照字节流。无快照时返回空 Vec。
pub fn read_snapshot(pager: &mut Pager, catalog: &CatalogPayload) -> Result<Vec<u8>> {
    if catalog.index_root == 0 {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut page_no = catalog.index_root;
    let mut part_expect = 0u32;
    let mut total_expect: Option<u32> = None;
    let mut guard = 0u32;
    loop {
        let mut buf = Vec::new();
        pager.read_payload(page_no, ROLE_SNAPSHOT, &mut buf)?;
        let mut r = Reader::new(&buf);
        let chunk_len = r.varint()? as usize;
        let next_page = r.u64()?;
        let part = r.u32()?;
        let total = r.u32()?;
        let chunk = r.bytes()?;
        if chunk.len() != chunk_len || part != part_expect {
            return Err(nebula_core::Error::Storage(
                "snapshot chain header mismatch".into(),
            ));
        }
        if let Some(t) = total_expect {
            if t != total {
                return Err(nebula_core::Error::Storage(
                    "snapshot part count mismatch".into(),
                ));
            }
        } else {
            total_expect = Some(total);
        }
        out.extend_from_slice(&chunk);
        part_expect += 1;
        guard += 1;
        if next_page == 0 {
            break;
        }
        if guard > 1_000_000 {
            return Err(nebula_core::Error::Storage(
                "snapshot chain too long (corrupted?)".into(),
            ));
        }
        page_no = next_page;
    }
    Ok(out)
}

/// 遍历并释放页链(供新快照提交后回收旧链)。
fn free_chain(pager: &mut Pager, catalog: &mut CatalogPayload, head: u64) -> Result<()> {
    let mut page_no = head;
    let mut guard = 0u32;
    loop {
        let mut buf = Vec::new();
        pager.read_payload(page_no, ROLE_SNAPSHOT, &mut buf)?;
        let mut r = Reader::new(&buf);
        let _chunk_len = r.varint()?;
        let next_page = r.u64()?;
        pager.free_page(catalog, page_no)?;
        if next_page == 0 {
            break;
        }
        guard += 1;
        if guard > 1_000_000 {
            return Err(nebula_core::Error::Storage(
                "snapshot chain too long (corrupted?)".into(),
            ));
        }
        page_no = next_page;
    }
    Ok(())
}

/// 删除整个快照(清空索引时用)。
pub fn drop_snapshot(pager: &mut Pager, catalog: &mut CatalogPayload) -> Result<()> {
    if catalog.index_root != 0 {
        let root = catalog.index_root;
        catalog.index_root = 0;
        pager.write_catalog(catalog)?;
        free_chain(pager, catalog, root)?;
    }
    Ok(())
}
