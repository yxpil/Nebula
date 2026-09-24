//! 页式布局常量与页载荷加解密。
//!
//! # 文件布局(类 MySQL 的页式存储)
//!
//! ```text
//! page 0    文件头页   [明文 magic/version/page_size/nonce][密文 HeaderPayload]
//! page 1    目录页     [密文 CatalogPayload]  (next_record_id/free 链/快照根/计数)
//! page 2..  数据页/快照页/空闲页,全部整页加密
//! ```
//!
//! 页密文布局:`len(u16 明文长度前缀) || nonce(12) || ciphertext || tag(16) || 零填充`。
/// 长度前缀是明文的,页内其余部分零填充到页大小。
///
/// 每页 AEAD 的 AAD = 页码 || 页角色,防止页被整体搬移后仍能通过认证。

use nebula_core::Error;
use nebula_crypto::{open, seal, KEY_LEN, NONCE_LEN, SALT_LEN, TAG_LEN};

/// 明文魔数。
pub const MAGIC: &[u8; 8] = b"NEBULADB";
/// 文件格式版本。
pub const FORMAT_VERSION: u16 = 1;

/// 文件头页号。
pub const PAGE_HEADER: u64 = 0;
/// 目录页号。
pub const PAGE_CATALOG: u64 = 1;

/// 页角色(AAD 第二段),用于域分离。
pub const ROLE_CATALOG: u8 = 1;
pub const ROLE_DATA: u8 = 2;
pub const ROLE_SNAPSHOT: u8 = 3;
pub const ROLE_FREE: u8 = 4;

/// 页密文固定开销:长度前缀(2) + nonce(12) + tag(16)。
pub const PAGE_OVERHEAD: usize = 2 + NONCE_LEN + TAG_LEN;

/// 加密一个页载荷并输出整页密文(含长度前缀与零填充由调用方补齐)。
/// 返回 `len(u16) || nonce || ciphertext || tag`。
pub fn seal_page(page_key: &[u8; KEY_LEN], page_no: u64, role: u8, payload: &[u8]) -> Vec<u8> {
    let mut aad = Vec::with_capacity(9);
    aad.extend_from_slice(&page_no.to_le_bytes());
    aad.push(role);
    let blob = seal(page_key, payload, &aad);
    let mut out = Vec::with_capacity(2 + blob.len());
    out.extend_from_slice(&(blob.len() as u16).to_le_bytes());
    out.extend_from_slice(&blob);
    out
}

/// 从整页原始字节解密页载荷。校验失败说明密码错误、文件损坏或页被篡改/搬移。
pub fn open_page(
    page_key: &[u8; KEY_LEN],
    page_no: u64,
    role: u8,
    raw: &[u8],
) -> nebula_core::Result<Vec<u8>> {
    if raw.len() < PAGE_OVERHEAD {
        return Err(Error::Storage("page too small".into()));
    }
    let blob_len = u16::from_le_bytes([raw[0], raw[1]]) as usize;
    if raw.len() < 2 + blob_len {
        return Err(Error::Storage("sealed blob length exceeds page".into()));
    }
    let mut aad = Vec::with_capacity(9);
    aad.extend_from_slice(&page_no.to_le_bytes());
    aad.push(role);
    open(page_key, &raw[2..2 + blob_len], &aad).map_err(nebula_core::Error::Storage)
}

/// 文件头页的明文前缀长度:magic(8) + version(2) + page_size(4) + salt(16) + payload_len(2)。
/// 密封输出(nonce||密文||tag)紧随其后。
pub const HEADER_PREFIX_LEN: usize = 8 + 2 + 4 + SALT_LEN + 2;

/// 盐在头页明文前缀中的偏移(magic + version + page_size 之后)。
pub const SALT_OFFSET: usize = 8 + 2 + 4;
