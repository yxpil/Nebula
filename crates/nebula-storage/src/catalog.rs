//! 文件头页与目录页的结构定义与编解码。
//!
//! 文件头页(page 0)布局:
//! ```text
//! [0..8]    magic "NEBULADB"
//! [8..10]   version u16
//! [10..14]  page_size u32
//! [14..30]  salt [u8;16](明文,Argon2 盐)
//! [30..32]  payload_len u16(seal 输出:nonce||密文||tag 的总长)
//! [32..32+payload_len] seal(header_key, HeaderPayload, aad=magic||version||page_size||salt)
//! ```
//! 密码正确性判断:先由明文 salt 派生 master/header_key,再尝试认证解密;
//! tag 校验失败即密码错误或文件损坏。HeaderPayload 内含随机 verifier,
//! 保证即使两库密码相同密文也不同。

use nebula_core::codec::{BinaryDecode, BinaryEncode, Reader, Writer};
use nebula_core::Result;

use crate::page::{FORMAT_VERSION, HEADER_PREFIX_LEN, MAGIC};

/// 文件头密文载荷(盐已置于明文前缀,不再加密)。
#[derive(Debug, Clone, PartialEq)]
pub struct HeaderPayload {
    /// 随机校验数,防止密文可预测。
    pub verifier: [u8; 32],
    /// 建库时的页数(仅信息性,打开时以文件实际大小为准)。
    pub page_count: u64,
}

impl HeaderPayload {
    pub fn new(page_count: u64) -> Self {
        use rand::RngCore;
        let mut verifier = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut verifier);
        HeaderPayload {
            verifier,
            page_count,
        }
    }
}

impl BinaryEncode for HeaderPayload {
    fn encode(&self, w: &mut Writer) {
        w.bytes(&self.verifier);
        self.page_count.encode(w);
    }
}

impl BinaryDecode for HeaderPayload {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let verifier_bytes = r.bytes()?;
        if verifier_bytes.len() != 32 {
            return Err(nebula_core::Error::Codec(
                "bad header payload field length".into(),
            ));
        }
        let mut verifier = [0u8; 32];
        verifier.copy_from_slice(&verifier_bytes);
        Ok(HeaderPayload {
            verifier,
            page_count: r.u64()?,
        })
    }
}

/// 目录页(page 1)密文载荷 —— 引擎层的可变元数据都集中在这里。
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogPayload {
    /// 下一条记忆的自增 ID(已用掉的最后一个 +1)。
    pub next_record_id: u64,
    /// 空闲页链头(0 = 无空闲页)。
    pub free_head: u64,
    /// 索引快照链头(0 = 无快照)。
    pub index_root: u64,
    /// 记忆条数。
    pub memory_count: u64,
    /// 检查点序号,每次写快照 +1。
    pub checkpoint_seq: u64,
}

impl CatalogPayload {
    pub fn empty() -> Self {
        CatalogPayload {
            next_record_id: 1,
            free_head: 0,
            index_root: 0,
            memory_count: 0,
            checkpoint_seq: 0,
        }
    }
}

impl BinaryEncode for CatalogPayload {
    fn encode(&self, w: &mut Writer) {
        w.str("NBC1");
        self.next_record_id.encode(w);
        self.free_head.encode(w);
        self.index_root.encode(w);
        self.memory_count.encode(w);
        self.checkpoint_seq.encode(w);
    }
}

impl BinaryDecode for CatalogPayload {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let magic = r.str()?;
        if magic != "NBC1" {
            return Err(nebula_core::Error::Codec(
                "catalog magic mismatch".into(),
            ));
        }
        Ok(CatalogPayload {
            next_record_id: r.u64()?,
            free_head: r.u64()?,
            index_root: r.u64()?,
            memory_count: r.u64()?,
            checkpoint_seq: r.u64()?,
        })
    }
}

/// 把 HeaderPayload 序列化并包装成完整的文件头页(明文前缀 + seal 输出 + 零填充)。
pub fn build_header_page(
    header_key: &[u8; 32],
    page_size: u32,
    salt: &[u8; 16],
    payload: &HeaderPayload,
) -> nebula_core::Result<Vec<u8>> {
    use nebula_crypto::seal;

    let mut page = vec![0u8; page_size as usize];
    page[0..8].copy_from_slice(MAGIC);
    page[8..10].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    page[10..14].copy_from_slice(&page_size.to_le_bytes());
    page[14..30].copy_from_slice(salt);

    let mut aad = Vec::with_capacity(30);
    aad.extend_from_slice(MAGIC);
    aad.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    aad.extend_from_slice(&page_size.to_le_bytes());
    aad.extend_from_slice(salt);

    let plain = nebula_core::codec::to_vec(payload);
    let blob = seal(header_key, &plain, &aad);
    let blob_len = blob.len();
    if HEADER_PREFIX_LEN + blob_len > page_size as usize {
        return Err(nebula_core::Error::Storage(
            "header payload does not fit in page".into(),
        ));
    }
    page[30..32].copy_from_slice(&(blob_len as u16).to_le_bytes());
    page[32..32 + blob_len].copy_from_slice(&blob);
    Ok(page)
}
