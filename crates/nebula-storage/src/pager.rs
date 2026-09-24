//! Pager:数据库文件的页级访问。
//!
//! 职责:
//! - 建库/开库:头页认证(密码错误 → [`Error::wrong_password`])
//! - 原始页读写、整页加密载荷读写(AAD = 页码||角色)
//! - 页分配:优先复用空闲页链,否则追加新页并同步头页计数
//! - 空闲页链维护与 fsync

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use nebula_core::codec::{from_slice, to_vec};
use nebula_core::{Error, Result};

use crate::catalog::{
    build_header_page, CatalogPayload, HeaderPayload,
};
use crate::page::{
    open_page, seal_page, FORMAT_VERSION, HEADER_PREFIX_LEN, MAGIC, PAGE_CATALOG, ROLE_CATALOG,
    ROLE_FREE,
};

/// 默认页大小。
pub const DEFAULT_PAGE_SIZE: u32 = 4096;

/// 最小/最大页大小(必须是 2 的幂)。
pub const MIN_PAGE_SIZE: u32 = 4096;
pub const MAX_PAGE_SIZE: u32 = 65536;

pub struct Pager {
    file: File,
    page_size: usize,
    page_count: u64,
    header_key: [u8; 32],
    page_key: [u8; 32],
    /// 头页的 page_count 与文件实际大小不一致时为 true(需要 checkpoint 落盘)。
    header_dirty: bool,
}

impl Pager {
    /// 创建新库。调用方负责确认密码与二次确认。
    pub fn create(
        path: &Path,
        password: &str,
        page_size: u32,
        catalog: &CatalogPayload,
    ) -> Result<Self> {
        if page_size < MIN_PAGE_SIZE || page_size > MAX_PAGE_SIZE || !page_size.is_power_of_two() {
            return Err(Error::Storage(format!(
                "page size {page_size} invalid (must be power of two in {MIN_PAGE_SIZE}..={MAX_PAGE_SIZE})"
            )));
        }
        if path.exists() {
            return Err(Error::Storage(format!(
                "file already exists: {}",
                path.display()
            )));
        }
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() && !dir.exists() {
                return Err(Error::Storage(format!(
                    "directory does not exist: {}",
                    dir.display()
                )));
            }
        }

        let salt = nebula_crypto::random_salt();
        let master = nebula_crypto::derive_master_key(password, &salt);
        let header_key = nebula_crypto::hkdf_sha256(&master, b"nebula/header/v1");
        let page_key = nebula_crypto::hkdf_sha256(&master, b"nebula/page/v1");

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;

        // page 0: 文件头页
        let header_page = build_header_page(&header_key, page_size, &salt, &HeaderPayload::new(2))?;
        file.write_all(&header_page)?;

        // page 1: 目录页
        let mut pager = Pager {
            file,
            page_size: page_size as usize,
            page_count: 2,
            header_key,
            page_key,
            header_dirty: false,
        };
        pager.write_catalog(catalog)?;
        pager.file.sync_all()?;
        Ok(pager)
    }

    /// 打开已有库。密码错误或文件损坏时返回 [`Error::wrong_password`]。
    pub fn open(path: &Path, password: &str) -> Result<(Self, CatalogPayload)> {
        if !path.exists() {
            return Err(Error::Storage(format!(
                "database file not found: {}",
                path.display()
            )));
        }
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        let file_len = file.seek(SeekFrom::End(0))?;
        if file_len < HEADER_PREFIX_LEN as u64 {
            return Err(Error::wrong_password());
        }

        let mut head = vec![0u8; HEADER_PREFIX_LEN];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut head)?;
        if &head[0..8] != MAGIC {
            return Err(Error::wrong_password());
        }
        let version = u16::from_le_bytes([head[8], head[9]]);
        if version != FORMAT_VERSION {
            return Err(Error::Storage(format!(
                "unsupported file format version {version}"
            )));
        }
        let page_size = u32::from_le_bytes([head[10], head[11], head[12], head[13]]);
        if page_size < MIN_PAGE_SIZE
            || page_size > MAX_PAGE_SIZE
            || !page_size.is_power_of_two()
            || file_len % page_size as u64 != 0
        {
            return Err(Error::Storage(
                "invalid page size or truncated database file".into(),
            ));
        }
        let mut salt = [0u8; 16];
        salt.copy_from_slice(&head[crate::page::SALT_OFFSET..crate::page::SALT_OFFSET + 16]);
        let payload_len = u16::from_le_bytes([head[30], head[31]]) as usize;

        let master = nebula_crypto::derive_master_key(password, &salt);
        let header_key = nebula_crypto::hkdf_sha256(&master, b"nebula/header/v1");
        let page_key = nebula_crypto::hkdf_sha256(&master, b"nebula/page/v1");

        // 认证:解密头页载荷,tag 失败 = 密码错误/文件损坏。
        let mut aad = Vec::with_capacity(30);
        aad.extend_from_slice(MAGIC);
        aad.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        aad.extend_from_slice(&page_size.to_le_bytes());
        aad.extend_from_slice(&salt);

        let mut blob = vec![0u8; payload_len];
        file.read_exact(&mut blob)?;
        let plain = nebula_crypto::open(&header_key, &blob, &aad)
            .map_err(|_| Error::wrong_password())?;
        let _header: HeaderPayload = from_slice(&plain)?;

        // 目录页
        let mut pager = Pager {
            file,
            page_size: page_size as usize,
            page_count: file_len / page_size as u64,
            header_key,
            page_key,
            header_dirty: false,
        };
        let mut buf = Vec::new();
        pager.read_payload(PAGE_CATALOG, ROLE_CATALOG, &mut buf)?;
        let catalog: CatalogPayload = from_slice(&buf)?;
        Ok((pager, catalog))
    }

    pub fn page_size(&self) -> usize {
        self.page_size
    }

    pub fn page_count(&self) -> u64 {
        self.page_count
    }

    /// 单页载荷容量(扣除长度前缀/nonce/tag)。
    pub fn payload_capacity(&self) -> usize {
        self.page_size - crate::page::PAGE_OVERHEAD
    }

    /// 读取并解密一页载荷。
    pub fn read_payload(&mut self, page_no: u64, role: u8, out: &mut Vec<u8>) -> Result<()> {
        out.clear();
        let mut raw = vec![0u8; self.page_size];
        {
            let offset = page_no
                .checked_mul(self.page_size as u64)
                .ok_or_else(|| Error::Storage("page offset overflow".into()))?;
            self.file.seek(SeekFrom::Start(offset))?;
            self.file.read_exact(&mut raw)?;
        }
        let payload = open_page(&self.page_key, page_no, role, &raw)?;
        out.extend_from_slice(&payload);
        Ok(())
    }

    /// 加密并整页写入载荷(载荷不得超出一页容量)。
    pub fn write_payload(&mut self, page_no: u64, role: u8, payload: &[u8]) -> Result<()> {
        if payload.len() > self.payload_capacity() {
            return Err(Error::Storage("payload exceeds page capacity".into()));
        }
        let blob = seal_page(&self.page_key, page_no, role, payload);
        if blob.len() > self.page_size {
            return Err(Error::Storage("sealed payload exceeds page size".into()));
        }
        let mut raw = vec![0u8; self.page_size];
        raw[..blob.len()].copy_from_slice(&blob);
        let offset = page_no * self.page_size as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(&raw)?;
        Ok(())
    }

    /// 追加新页写入载荷,返回新页号;同步更新头页 page_count。
    pub fn append_payload(&mut self, role: u8, payload: &[u8]) -> Result<u64> {
        let page_no = self.page_count;
        self.write_payload(page_no, role, payload)?;
        self.page_count += 1;
        self.header_dirty = true;
        Ok(page_no)
    }

    /// 分配一页:优先从空闲链摘取,否则追加新页。
    pub fn allocate(&mut self, catalog: &mut CatalogPayload) -> Result<u64> {
        if catalog.free_head != 0 {
            let page_no = catalog.free_head;
            let mut buf = Vec::new();
            self.read_payload(page_no, ROLE_FREE, &mut buf)?;
            // 空闲页载荷:next_free
            let next_free = nebula_core::codec::Reader::new(&buf).u64()?;
            catalog.free_head = next_free;
            self.clear_page(page_no)?;
            return Ok(page_no);
        }
        let page_no = self.append_payload(ROLE_FREE, &[])?;
        self.clear_page(page_no)?;
        Ok(page_no)
    }

    /// 归还一页到空闲链(写入 next_free 指针)。
    pub fn free_page(&mut self, catalog: &mut CatalogPayload, page_no: u64) -> Result<()> {
        if page_no < PAGE_CATALOG + 1 {
            return Err(Error::Storage(format!(
                "cannot free reserved page {page_no}"
            )));
        }
        let payload = to_vec(&catalog.free_head);
        self.write_payload(page_no, ROLE_FREE, &payload)?;
        catalog.free_head = page_no;
        Ok(())
    }

    /// 空闲页计数(仅供统计展示)。
    pub fn free_page_count(&self, catalog: &CatalogPayload) -> usize {
        let mut count = 0usize;
        let mut cur = catalog.free_head;
        let mut guard = 0usize;
        while cur != 0 && guard < 1_000_000 {
            count += 1;
            guard += 1;
            let mut buf = Vec::new();
            if self.clone_read(cur, ROLE_FREE, &mut buf).is_err() {
                break;
            }
            cur = nebula_core::codec::Reader::new(&buf).u64().unwrap_or(0);
        }
        count
    }

    // ------- 内部 -------

    /// 只读副本用的页读取(free_page_count 需要读但函数签名为 &self)。
    fn clone_read(&self, page_no: u64, role: u8, out: &mut Vec<u8>) -> Result<()> {
        out.clear();
        let mut file = self.file.try_clone()?;
        let mut raw = vec![0u8; self.page_size];
        let offset = page_no * self.page_size as u64;
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut raw)?;
        let payload = open_page(&self.page_key, page_no, role, &raw)?;
        out.extend_from_slice(&payload);
        Ok(())
    }

    fn clear_page(&mut self, page_no: u64) -> Result<()> {
        let raw = vec![0u8; self.page_size];
        let offset = page_no * self.page_size as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(&raw)?;
        Ok(())
    }

    /// 写目录页。
    pub fn write_catalog(&mut self, catalog: &CatalogPayload) -> Result<()> {
        let payload = to_vec(catalog);
        self.write_payload(PAGE_CATALOG, ROLE_CATALOG, &payload)
    }

    /// 把头页的 page_count 同步为实际值并 fsync。
    pub fn sync(&mut self) -> Result<()> {
        if self.header_dirty {
            // 头页载荷只需更新 page_count;重新 seal 整页保证认证通过。
            let payload = HeaderPayload::new(self.page_count);
            let mut salt = [0u8; 16];
            let mut head = vec![0u8; 30];
            self.file.seek(SeekFrom::Start(0))?;
            self.file.read_exact(&mut head)?;
            salt.copy_from_slice(&head[14..30]);
            let page = build_header_page(&self.header_key, self.page_size as u32, &salt, &payload)?;
            self.file.seek(SeekFrom::Start(0))?;
            self.file.write_all(&page)?;
            self.header_dirty = false;
        }
        self.file.sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env::temp_dir;

    fn tmp(name: &str) -> std::path::PathBuf {
        let mut p = temp_dir();
        p.push(format!("nebula_pager_{}_{}.ndb", name, std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn create_open_roundtrip() {
        let path = tmp("basic");
        let catalog = CatalogPayload::empty();
        {
            let mut p = Pager::create(&path, "pw12345678", DEFAULT_PAGE_SIZE, &catalog).unwrap();
            p.sync().unwrap();
        }
        let (mut p, cat) = Pager::open(&path, "pw12345678").unwrap();
        assert_eq!(cat, catalog);
        assert_eq!(p.page_count(), 2);

        // 错误密码必须被拒绝
        assert!(Pager::open(&path, "wrong-pw").is_err());

        // 写/读载荷
        let no = p.append_payload(ROLE_FREE, &[1, 2, 3]).unwrap();
        p.sync().unwrap();
        let mut buf = Vec::new();
        p.read_payload(no, ROLE_FREE, &mut buf).unwrap();
        assert_eq!(buf, vec![1, 2, 3]);
        // 角色不匹配必须拒绝(防页搬移)
        let mut buf2 = Vec::new();
        assert!(p.read_payload(no, ROLE_CATALOG, &mut buf2).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn alloc_free_reuse() {
        let path = tmp("reuse");
        let mut catalog = CatalogPayload::empty();
        {
            let mut p = Pager::create(&path, "pw12345678", DEFAULT_PAGE_SIZE, &catalog).unwrap();
            let a = p.allocate(&mut catalog).unwrap();
            let b = p.allocate(&mut catalog).unwrap();
            assert_ne!(a, b);
            p.free_page(&mut catalog, a).unwrap();
            p.free_page(&mut catalog, b).unwrap();
            let c = p.allocate(&mut catalog).unwrap();
            assert!(c == a || c == b);
            assert_eq!(p.free_page_count(&catalog), 1);
            p.write_catalog(&catalog).unwrap();
            p.sync().unwrap();
        }
        // 重开后目录一致
        let (mut p, cat) = Pager::open(&path, "pw12345678").unwrap();
        assert_eq!(cat, catalog);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_non_nebula_file() {
        let path = tmp("notdb");
        std::fs::write(&path, b"this is not a nebula database").unwrap();
        assert!(Pager::open(&path, "pw").is_err());
        let _ = std::fs::remove_file(&path);
    }
}
