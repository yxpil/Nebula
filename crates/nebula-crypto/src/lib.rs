//! Nebula 密码学模块:Argon2id 密钥派生、HKDF、HMAC-SHA256、ChaCha20-Poly1305 AEAD。

use argon2::Argon2;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::ChaCha20Poly1305;
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;

/// 主密钥长度(256 bit)。
pub const KEY_LEN: usize = 32;
/// AEAD nonce 长度(96 bit)。
pub const NONCE_LEN: usize = 12;
/// AEAD 认证标签长度。
pub const TAG_LEN: usize = 16;
/// 盐长度。
pub const SALT_LEN: usize = 16;

type HmacSha256 = Hmac<Sha256>;

/// 从密码派生 256-bit 主密钥(Argon2id)。
///
/// 参数遵循 OWASP 推荐(m=19 MiB, t=2, p=1),每次派生约 50~200ms,
/// 对暴力破解有足够成本,同时对交互式打开数据库足够快。
pub fn derive_master_key(password: &str, salt: &[u8; SALT_LEN]) -> [u8; KEY_LEN] {
    let params = argon2::Params::new(19 * 1024, 2, 1, Some(KEY_LEN))
        .expect("fixed argon2 params are valid");
    let argon = Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut key = [0u8; KEY_LEN];
    argon
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .expect("argon2 key derivation cannot fail with fixed params");
    key
}

/// HKDF-SHA256 从主密钥派生子密钥。`info` 提供域分离。
pub fn hkdf_sha256(master: &[u8; KEY_LEN], info: &[u8]) -> [u8; KEY_LEN] {
    let hk = Hkdf::<Sha256>::new(None, master);
    let mut out = [0u8; KEY_LEN];
    hk.expand(info, &mut out).expect("32 bytes is valid HKDF output");
    out
}

/// HMAC-SHA256 认证摘要。
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; KEY_LEN] {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(msg);
    let out = mac.finalize().into_bytes();
    let mut result = [0u8; KEY_LEN];
    result.copy_from_slice(&out);
    result
}

/// 常量时间比较两个 32 字节摘要。
pub fn ct_eq(a: &[u8; KEY_LEN], b: &[u8; KEY_LEN]) -> bool {
    let mut diff = 0u8;
    for i in 0..KEY_LEN {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// 生成密码学安全随机字节。
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    rand::thread_rng().fill_bytes(&mut out);
    out
}

/// 生成密码学安全随机盐。
pub fn random_salt() -> [u8; SALT_LEN] {
    random_bytes()
}

/// 生成密码学安全随机 nonce(每次写页/每会话唯一)。
pub fn random_nonce() -> [u8; NONCE_LEN] {
    random_bytes()
}

/// ChaCha20-Poly1305 加密。成功时返回 nonce || ciphertext || tag。
pub fn seal(key: &[u8; KEY_LEN], plaintext: &[u8], aad: &[u8]) -> Vec<u8> {
    let cipher = ChaCha20Poly1305::new(key.into());
    let nonce = random_nonce();
    let payload = Payload {
        msg: plaintext,
        aad,
    };
    let ct = cipher
        .encrypt(&nonce.into(), payload)
        .expect("aead encrypt cannot fail");
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    out
}

/// ChaCha20-Poly1305 解密。输入为 nonce || ciphertext || tag。
pub fn open(key: &[u8; KEY_LEN], blob: &[u8], aad: &[u8]) -> Result<Vec<u8>, String> {
    if blob.len() < NONCE_LEN + TAG_LEN {
        return Err("ciphertext too short".into());
    }
    let (nonce_bytes, rest) = blob.split_at(NONCE_LEN);
    let mut nonce = [0u8; NONCE_LEN];
    nonce.copy_from_slice(nonce_bytes);
    let cipher = ChaCha20Poly1305::new(key.into());
    let payload = Payload { msg: rest, aad };
    cipher
        .decrypt(&nonce.into(), payload)
        .map_err(|_| "aead authentication failed".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrip() {
        let key = [7u8; KEY_LEN];
        let aad = b"page-42";
        let blob = seal(&key, b"hello nebula", aad);
        let pt = open(&key, &blob, aad).unwrap();
        assert_eq!(pt, b"hello nebula");
    }

    #[test]
    fn wrong_key_fails() {
        let key1 = [1u8; KEY_LEN];
        let key2 = [2u8; KEY_LEN];
        let blob = seal(&key1, b"secret", b"");
        assert!(open(&key2, &blob, b"").is_err());
    }

    #[test]
    fn tampered_aad_fails() {
        let key = [9u8; KEY_LEN];
        let blob = seal(&key, b"secret", b"a");
        assert!(open(&key, &blob, b"b").is_err());
    }

    #[test]
    fn truncated_blob_fails() {
        let key = [3u8; KEY_LEN];
        let blob = seal(&key, b"secret", b"");
        assert!(open(&key, &blob[..10], b"").is_err());
    }

    #[test]
    fn hkdf_domain_separation() {
        let master = [5u8; KEY_LEN];
        assert_ne!(hkdf_sha256(&master, b"a"), hkdf_sha256(&master, b"b"));
    }

    #[test]
    fn derive_key_is_deterministic_and_salted() {
        let salt = random_salt();
        let k1 = derive_master_key("pw", &salt);
        let k2 = derive_master_key("pw", &salt);
        assert_eq!(k1, k2);
        let mut salt2 = salt;
        salt2[0] ^= 1;
        assert_ne!(k1, derive_master_key("pw", &salt2));
        assert_ne!(k1, derive_master_key("pw2", &salt));
    }
}
