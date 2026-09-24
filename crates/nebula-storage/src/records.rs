//! 记录链写入与读取:单页放得下的小记录单页存储,
//! 超大记录拆成多页链(每页内部头部描述 record_id/seq/total/next)。

use nebula_core::{RecordLocation, Result};
use nebula_core::codec::{Reader, Writer};

use crate::catalog::CatalogPayload;
use crate::page::ROLE_DATA;
use crate::pager::Pager;

/// 单页载荷中用于页内头部的固定开销(角色字节 + u64 + u32*2 + u64 + varint 长度)。
const DATA_PAGE_HEADER_MAX: usize = 1 + 8 + 4 + 4 + 8 + 5;

/// 把一条编码后的记录写入页链,返回物理位置。
pub fn write_record(pager: &mut Pager, catalog: &mut CatalogPayload, bytes: &[u8]) -> Result<RecordLocation> {
    let capacity = pager.payload_capacity().saturating_sub(DATA_PAGE_HEADER_MAX);
    if capacity == 0 {
        return Err(nebula_core::Error::Storage("page size too small".into()));
    }
    let total_pages = ((bytes.len() + capacity - 1) / capacity).max(1) as u32;
    let head_page = pager.allocate(catalog)?;

    let mut pages: Vec<u64> = Vec::with_capacity(total_pages as usize);
    pages.push(head_page);
    for _ in 1..total_pages {
        pages.push(pager.allocate(catalog)?);
    }

    let mut offset = 0usize;
    for (seq, &page_no) in pages.iter().enumerate() {
        let end = (offset + capacity).min(bytes.len());
        let chunk = &bytes[offset..end];
        let next_page = pages.get(seq + 1).copied().unwrap_or(0);
        let mut w = Writer::new();
        w.varint(chunk.len() as u64);
        w.u64(next_page);
        w.u32(seq as u32);
        w.u32(total_pages);
        w.bytes(chunk);
        pager.write_payload(page_no, ROLE_DATA, &w.into_vec())?;
        offset = end;
    }

    Ok(RecordLocation::new(head_page, total_pages, bytes.len() as u64))
}

/// 按物理位置读取完整记录(链遍历)。
pub fn read_record(pager: &mut Pager, loc: &RecordLocation) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(loc.encoded_len as usize);
    let mut page_no = loc.head_page;
    let mut seq_expect = 0u32;
    let mut guard = 0u32;
    loop {
        let mut buf = Vec::new();
        pager.read_payload(page_no, ROLE_DATA, &mut buf)?;
        let mut r = Reader::new(&buf);
        let chunk_len = r.varint()? as usize;
        let next_page = r.u64()?;
        let seq = r.u32()?;
        let total = r.u32()?;
        let chunk = r.bytes()?;
        if chunk.len() != chunk_len {
            return Err(nebula_core::Error::Storage(
                "record page chunk length mismatch".into(),
            ));
        }
        if seq != seq_expect || total != loc.page_count {
            return Err(nebula_core::Error::Storage(
                "record chain header mismatch".into(),
            ));
        }
        out.extend_from_slice(&chunk);
        seq_expect += 1;
        guard += 1;
        if next_page == 0 {
            break;
        }
        if guard > 1_000_000 {
            return Err(nebula_core::Error::Storage(
                "record chain too long (corrupted?)".into(),
            ));
        }
        page_no = next_page;
    }
    if out.len() as u64 != loc.encoded_len {
        return Err(nebula_core::Error::Storage(
            "record length mismatch".into(),
        ));
    }
    Ok(out)
}

/// 释放记录占用的页链。
pub fn free_record(pager: &mut Pager, catalog: &mut CatalogPayload, loc: &RecordLocation) -> Result<()> {
    let mut page_no = loc.head_page;
    let mut guard = 0u32;
    loop {
        let mut buf = Vec::new();
        pager.read_payload(page_no, ROLE_DATA, &mut buf)?;
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
                "record chain too long (corrupted?)".into(),
            ));
        }
        page_no = next_page;
    }
    Ok(())
}
