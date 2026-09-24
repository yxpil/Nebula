//! Nebula 有线协议:v2(多用户)。
//!
//! # 握手(两步身份声明 + 挑战 - 应答,防重放)
//!
//! ```text
//! client ──▶ hello:   b"NEBULA2"(7)
//! server ──▶ ready:   0x02(1 字节,版本确认)
//! client ──▶ identity:varint 用户名长度 || 用户名(UTF-8)
//! server ──▶ challenge:salt(16) || challenge(32)         共 48 字节
//! client ──▶ proof:   HMAC-SHA256(session_key, challenge) 共 32 字节
//! server ──▶ status:  0x01 认证通过 / 0x00 认证失败(随后断开)
//! ```
//!
//! 两步身份声明的原因:集群模式每个用户有独立盐,客户端必须先告知用户名,
//! 服务端才能把该用户记录中的盐原样带回。
//!
//! 客户端只需知道用户名与密码:
//! - `master = Argon2id(password, salt)`(盐在 challenge 中原样带回,公开值)
//! - `session_key = HKDF-SHA256(master, "nebula/session/v2" || challenge)`
//! - `proof = HMAC-SHA256(session_key, challenge)`
//!
//! 单文件模式只接受 user="admin",salt 为文件头盐;集群模式由用户目录
//! (nebula-cluster 的 _admin.ndb)解析盐与 Argon2id 验证器。
//!
//! challenge 每次连接新鲜 → proof 不可重放;密码不明文上网。
//!
//! # 帧格式(认证后)
//!
//! ```text
//! [u32 BE 长度] || ChaCha20-Poly1305(nonce || 密文 || tag)
//! ```
//!
//! AAD = `"nebula/frame/up|down" || seq(u64 BE)`,序号收发双方各自递增,
//! 重放/重排/注入的帧无法通过认证。载荷为 serde_json 的 [`Request`]/[`Response`]。

use std::io::{Read, Write};
use std::net::TcpStream;

use nebula_core::{Error, Result};
use nebula_crypto::{ct_eq, hkdf_sha256, hmac_sha256, open, seal, KEY_LEN, SALT_LEN};

/// 协议魔数。
pub const PROTOCOL_MAGIC: &[u8; 7] = b"NEBULA2";
/// 版本确认字节。
pub const READY_BYTE: u8 = 0x02;
/// 用户名最大长度。
pub const MAX_USER_NAME: usize = 64;
/// 挑战长度。
pub const CHALLENGE_LEN: usize = 32;
/// challenge 帧总长:salt(16) + challenge(32)。
pub const CHALLENGE_FRAME_LEN: usize = SALT_LEN + CHALLENGE_LEN;
/// proof 长度(HMAC-SHA256 输出)。
pub const PROOF_LEN: usize = KEY_LEN;
/// 认证通过状态字节。
pub const STATUS_OK: u8 = 1;
/// 认证失败状态字节。
pub const STATUS_FAIL: u8 = 0;
/// 单帧上限(16 MiB),防止恶意长度字段耗尽内存。
pub const MAX_FRAME: usize = 16 * 1024 * 1024;
/// 会话密钥派生信息前缀。
const SESSION_INFO: &[u8] = b"nebula/session/v2";
/// 帧 AAD 域分离前缀:client → server / server → client 两个方向各自独立编号。
const FRAME_INFO_UP: &[u8] = b"nebula/frame/up";
const FRAME_INFO_DOWN: &[u8] = b"nebula/frame/down";

/// 帧方向(防止把对端发来的帧重放到本端发送通道上)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// client → server。
    Up,
    /// server → client。
    Down,
}

/// 客户端请求。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// 执行一条 SQL。
    Sql { sql: String },
    /// 连通性探测。
    Ping,
    /// 优雅关闭会话。
    Close,
}

/// 服务端响应(错误也走 `ok=false`,不关闭连接)。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub columns: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<Vec<Vec<String>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default)]
    pub affected: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 多语句脚本的全部结果(主字段保留最后一个,便于单语句客户端兼容)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<Vec<Response>>,
}

impl Response {
    /// 纯文字结果(DML/元语句)。
    pub fn text(message: impl Into<String>, affected: u64) -> Self {
        Response {
            ok: true,
            message: Some(message.into()),
            affected,
            ..Default::default()
        }
    }

    /// 结果集。
    pub fn rows(columns: Vec<String>, rows: Vec<Vec<String>>) -> Self {
        Response {
            ok: true,
            columns: Some(columns),
            rows: Some(rows),
            ..Default::default()
        }
    }

    /// 错误响应。
    pub fn error(message: impl Into<String>) -> Self {
        Response {
            ok: false,
            error: Some(message.into()),
            ..Default::default()
        }
    }
}

/// 由主密钥与挑战派生会话密钥。
pub fn session_key(master: &[u8; KEY_LEN], challenge: &[u8; CHALLENGE_LEN]) -> [u8; KEY_LEN] {
    let mut info = Vec::with_capacity(SESSION_INFO.len() + CHALLENGE_LEN);
    info.extend_from_slice(SESSION_INFO);
    info.extend_from_slice(challenge);
    hkdf_sha256(master, &info)
}

/// 编码 LEB128 varint。
pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// 解码 LEB128 varint。
pub fn take_varint(bytes: &[u8]) -> Result<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    for (i, &byte) in bytes.iter().enumerate() {
        result |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Ok((result, i + 1));
        }
        shift += 7;
        if shift >= 64 {
            return Err(Error::Protocol("varint too long".into()));
        }
    }
    Err(Error::Protocol("unexpected end of varint".into()))
}

/// 客户端写身份帧:varint 长度 + 用户名。
pub fn write_identity(stream: &mut TcpStream, user: &str) -> Result<()> {
    let bytes = user.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_USER_NAME {
        return Err(Error::Protocol(format!(
            "user name must be 1..{MAX_USER_NAME} bytes"
        )));
    }
    let mut frame = Vec::with_capacity(bytes.len() + 1);
    put_varint(&mut frame, bytes.len() as u64);
    frame.extend_from_slice(bytes);
    stream.write_all(&frame)?;
    stream.flush()?;
    Ok(())
}

/// 服务端读身份帧。
pub fn read_identity(stream: &mut TcpStream) -> Result<String> {
    // varint 长度 ≤ 64,第一字节即完整 varint。
    let mut len_buf = [0u8; 1];
    stream
        .read_exact(&mut len_buf)
        .map_err(|e| wrap_eof(e, "identity length"))?;
    let (n, used) = take_varint(&len_buf)?;
    debug_assert_eq!(used, 1);
    let n = n as usize;
    if n == 0 || n > MAX_USER_NAME {
        return Err(Error::Protocol(format!(
            "user name must be 1..{MAX_USER_NAME} bytes"
        )));
    }
    let mut name = vec![0u8; n];
    stream
        .read_exact(&mut name)
        .map_err(|e| wrap_eof(e, "identity body"))?;
    String::from_utf8(name)
        .map_err(|e| Error::Protocol(format!("invalid user name utf-8: {e}")))
}

/// 认证证明:HMAC-SHA256(session_key, challenge)。
pub fn auth_proof(session_key: &[u8; KEY_LEN], challenge: &[u8; CHALLENGE_LEN]) -> [u8; KEY_LEN] {
    hmac_sha256(session_key, challenge)
}

/// 帧序号 AAD:方向域分离前缀 + 序号。
fn frame_aad(dir: Direction, seq: u64) -> Vec<u8> {
    let prefix = match dir {
        Direction::Up => FRAME_INFO_UP,
        Direction::Down => FRAME_INFO_DOWN,
    };
    let mut aad = Vec::with_capacity(prefix.len() + 8);
    aad.extend_from_slice(prefix);
    aad.extend_from_slice(&seq.to_be_bytes());
    aad
}

/// 加密一帧载荷(不含长度前缀)。
pub fn seal_frame(session_key: &[u8; KEY_LEN], dir: Direction, seq: u64, payload: &[u8]) -> Vec<u8> {
    seal(session_key, payload, &frame_aad(dir, seq))
}

/// 解密一帧载荷。
pub fn open_frame(
    session_key: &[u8; KEY_LEN],
    dir: Direction,
    seq: u64,
    blob: &[u8],
) -> Result<Vec<u8>> {
    open(session_key, blob, &frame_aad(dir, seq)).map_err(Error::Protocol)
}

/// 写一帧:`[u32 BE 长度][密文]`。
pub fn write_frame(
    stream: &mut TcpStream,
    session_key: &[u8; KEY_LEN],
    dir: Direction,
    seq: u64,
    payload: &[u8],
) -> Result<()> {
    let blob = seal_frame(session_key, dir, seq, payload);
    let len = u32::try_from(blob.len())
        .map_err(|_| Error::Protocol("frame exceeds u32 length".into()))?;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(&blob)?;
    stream.flush()?;
    Ok(())
}

/// 读一帧。客户端正常断开(EOF)时返回 [`Error::Protocol`] 且含 "connection closed"。
pub fn read_frame(
    stream: &mut TcpStream,
    session_key: &[u8; KEY_LEN],
    dir: Direction,
    seq: u64,
) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .map_err(|e| wrap_eof(e, "length prefix"))?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME {
        return Err(Error::Protocol(format!(
            "frame length {len} exceeds limit {MAX_FRAME}"
        )));
    }
    let mut blob = vec![0u8; len];
    stream
        .read_exact(&mut blob)
        .map_err(|e| wrap_eof(e, "frame body"))?;
    open_frame(session_key, dir, seq, &blob)
}

/// 把 EOF 类 IO 错误统一改写为"连接关闭"协议错误,便于调用方区分正常断开。
fn wrap_eof(e: std::io::Error, stage: &str) -> Error {
    match e.kind() {
        std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset => {
            Error::Protocol("connection closed".into())
        }
        _ => Error::Io(std::io::Error::new(
            e.kind(),
            format!("failed to read {stage}: {e}"),
        )),
    }
}

/// 校验客户端 proof 是否匹配(常量时间比较)。
pub fn check_proof(
    master: &[u8; KEY_LEN],
    challenge: &[u8; CHALLENGE_LEN],
    proof: &[u8; PROOF_LEN],
) -> bool {
    let expected = auth_proof(&session_key(master, challenge), challenge);
    ct_eq(&expected, proof)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn master() -> [u8; KEY_LEN] {
        [42u8; KEY_LEN]
    }

    #[test]
    fn proof_roundtrip_and_reject() {
        let m = master();
        let challenge = [7u8; CHALLENGE_LEN];
        let proof = auth_proof(&session_key(&m, &challenge), &challenge);
        assert!(check_proof(&m, &challenge, &proof));
        // 错误密码派生出的 master,proof 不匹配
        let m2 = [43u8; KEY_LEN];
        assert!(!check_proof(&m2, &challenge, &proof));
        // 篡改 challenge → 失配(防重放)
        let mut c2 = challenge;
        c2[0] ^= 1;
        assert!(!check_proof(&m, &c2, &proof));
    }

    #[test]
    fn request_json_roundtrip() {
        let req = Request::Sql {
            sql: "SELECT * FROM memories".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""type":"sql""#), "{json}");
        let back: Request = serde_json::from_slice(json.as_bytes()).unwrap();
        match back {
            Request::Sql { sql } => assert_eq!(sql, "SELECT * FROM memories"),
            _ => panic!("wrong variant"),
        }
        let ping: Request = serde_json::from_slice(br#"{"type":"ping"}"#).unwrap();
        assert!(matches!(ping, Request::Ping));
        let close: Request = serde_json::from_slice(br#"{"type":"close"}"#).unwrap();
        assert!(matches!(close, Request::Close));
    }

    #[test]
    fn frame_roundtrip_over_tcpstream_pair() {
        // 用 TcpListener/connect 建立一对真实流验证帧读写。
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let payload = read_frame(&mut stream, &master(), Direction::Up, 0).unwrap();
            let resp = Response::text("pong", 0);
            write_frame(
                &mut stream,
                &master(),
                Direction::Down,
                0,
                &serde_json::to_vec(&resp).unwrap(),
            )
            .unwrap();
            payload
        });
        let mut client = TcpStream::connect(addr).unwrap();
        let key = master();
        write_frame(&mut client, &key, Direction::Up, 0, br#"{"type":"ping"}"#).unwrap();
        let got = read_frame(&mut client, &key, Direction::Down, 0).unwrap();
        let resp: Response = serde_json::from_slice(&got).unwrap();
        assert_eq!(resp.message.as_deref(), Some("pong"));
        assert_eq!(server.join().unwrap(), br#"{"type":"ping"}"#);
    }

    #[test]
    fn wrong_seq_frame_rejected() {
        let key = master();
        let blob = seal_frame(&key, Direction::Up, 0, b"hello");
        // 序号错、方向错(把对端帧重放到发送通道)都必须失配
        assert!(open_frame(&key, Direction::Up, 1, &blob).is_err());
        assert!(open_frame(&key, Direction::Down, 0, &blob).is_err());
        assert_eq!(
            open_frame(&key, Direction::Up, 0, &blob).unwrap(),
            b"hello"
        );
    }
}
