//! 手写二进制编解码器。所有落盘结构(页载荷、记忆记录、索引快照)
//! 都通过本模块定义的紧凑格式序列化,保证跨版本可解析:
//!
//! | 类型     | 布局                            |
//! |----------|---------------------------------|
//! | 定长整数 | 小端固定宽度                     |
//! | 变长整数 | LEB128(varint),非负值            |
//! | 字符串   | varint 长度 + UTF-8 字节         |
//! | 字节块   | varint 长度 + 原始字节           |
//! | Vec<T>   | varint 元素数 + 逐元素编码       |

use crate::error::{Error, Result};
use std::fmt;
use std::io::Write;

/// 向字节流写入定长/变长字段的编码器。
#[derive(Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn bool(&mut self, v: bool) {
        self.buf.push(v as u8);
    }

    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn f32(&mut self, v: f32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// LEB128 无符号变长整数。
    pub fn varint(&mut self, mut v: u64) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.buf.push(byte);
                break;
            }
            self.buf.push(byte | 0x80);
        }
    }

    pub fn bytes(&mut self, b: &[u8]) {
        self.varint(b.len() as u64);
        self.buf.extend_from_slice(b);
    }

    /// 原样追加字节(不带长度前缀;用于快照魔数等定长标记)。
    pub fn raw(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    pub fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }
}

/// 从字节流解码字段,与 [`Writer`] 完全对称。
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(Error::Codec("unexpected end of buffer".into()));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn bool(&mut self) -> Result<bool> {
        Ok(self.u8()? != 0)
    }

    pub fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }

    pub fn i64(&mut self) -> Result<i64> {
        Ok(self.u64()? as i64)
    }

    pub fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_bits(self.u32()?))
    }

    pub fn varint(&mut self) -> Result<u64> {
        let mut result: u64 = 0;
        let mut shift = 0;
        loop {
            let byte = self.u8()?;
            result |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
            if shift >= 64 {
                return Err(Error::Codec("varint too long".into()));
            }
        }
    }

    pub fn bytes(&mut self) -> Result<Vec<u8>> {
        let n = self.varint()? as usize;
        Ok(self.take(n)?.to_vec())
    }

    pub fn str(&mut self) -> Result<String> {
        let b = self.bytes()?;
        String::from_utf8(b).map_err(|e| Error::Codec(format!("invalid utf-8: {e}")))
    }
}

/// 可二进制编码的类型。
pub trait BinaryEncode {
    fn encode(&self, w: &mut Writer);

    fn encoded_len(&self) -> usize {
        let mut probe = Writer::new();
        self.encode(&mut probe);
        probe.len()
    }
}

/// 可二进制解码的类型。
pub trait BinaryDecode: Sized {
    fn decode(r: &mut Reader<'_>) -> Result<Self>;
}

impl<T: BinaryEncode> BinaryEncode for Vec<T> {
    fn encode(&self, w: &mut Writer) {
        w.varint(self.len() as u64);
        for item in self {
            item.encode(w);
        }
    }
}

impl BinaryEncode for (u64, u64) {
    fn encode(&self, w: &mut Writer) {
        self.0.encode(w);
        self.1.encode(w);
    }
}
impl BinaryDecode for (u64, u64) {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Ok((r.u64()?, r.u64()?))
    }
}

impl BinaryEncode for (u64, u32, u64) {
    fn encode(&self, w: &mut Writer) {
        self.0.encode(w);
        self.1.encode(w);
        self.2.encode(w);
    }
}
impl BinaryDecode for (u64, u32, u64) {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Ok((r.u64()?, r.u32()?, r.u64()?))
    }
}

impl BinaryEncode for (String, u64, f32) {
    fn encode(&self, w: &mut Writer) {
        self.0.encode(w);
        self.1.encode(w);
        self.2.encode(w);
    }
}
impl BinaryDecode for (String, u64, f32) {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Ok((String::decode(r)?, r.u64()?, r.f32()?))
    }
}

impl BinaryEncode for (String, Vec<u64>) {
    fn encode(&self, w: &mut Writer) {
        self.0.encode(w);
        self.1.encode(w);
    }
}
impl BinaryDecode for (String, Vec<u64>) {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Ok((String::decode(r)?, Vec::<u64>::decode(r)?))
    }
}

impl<T: BinaryDecode> BinaryDecode for Vec<T> {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let n = r.varint()? as usize;
        let mut v = Vec::with_capacity(n.min(4096));
        for _ in 0..n {
            v.push(T::decode(r)?);
        }
        Ok(v)
    }
}

impl<T: BinaryEncode> BinaryEncode for Option<T> {
    fn encode(&self, w: &mut Writer) {
        match self {
            Some(v) => {
                w.bool(true);
                v.encode(w);
            }
            None => w.bool(false),
        }
    }
}

impl<T: BinaryDecode> BinaryDecode for Option<T> {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        if r.bool()? {
            Ok(Some(T::decode(r)?))
        } else {
            Ok(None)
        }
    }
}

impl BinaryEncode for u16 {
    fn encode(&self, w: &mut Writer) {
        w.u16(*self)
    }
}
impl BinaryDecode for u16 {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        r.u16()
    }
}

impl BinaryEncode for u32 {
    fn encode(&self, w: &mut Writer) {
        w.u32(*self)
    }
}
impl BinaryDecode for u32 {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        r.u32()
    }
}

impl BinaryEncode for u64 {
    fn encode(&self, w: &mut Writer) {
        w.u64(*self)
    }
}
impl BinaryDecode for u64 {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        r.u64()
    }
}

impl BinaryEncode for i64 {
    fn encode(&self, w: &mut Writer) {
        w.i64(*self)
    }
}
impl BinaryDecode for i64 {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        r.i64()
    }
}

impl BinaryEncode for f32 {
    fn encode(&self, w: &mut Writer) {
        w.f32(*self)
    }
}
impl BinaryDecode for f32 {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        r.f32()
    }
}

impl BinaryEncode for bool {
    fn encode(&self, w: &mut Writer) {
        w.bool(*self)
    }
}
impl BinaryDecode for bool {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        r.bool()
    }
}

impl BinaryEncode for String {
    fn encode(&self, w: &mut Writer) {
        w.str(self)
    }
}
impl BinaryDecode for String {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        r.str()
    }
}

impl BinaryEncode for &str {
    fn encode(&self, w: &mut Writer) {
        w.str(self)
    }
}

impl BinaryEncode for Vec<u8> {
    fn encode(&self, w: &mut Writer) {
        w.bytes(self)
    }
}
impl BinaryDecode for Vec<u8> {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        r.bytes()
    }
}

/// 便捷函数:编码任意实现了 BinaryEncode 的值。
pub fn to_vec<T: BinaryEncode>(v: &T) -> Vec<u8> {
    let mut w = Writer::new();
    v.encode(&mut w);
    w.into_vec()
}

/// 便捷函数:从任意切片解码 T,并要求缓冲区被完整消费。
pub fn from_slice<T: BinaryDecode>(buf: &[u8]) -> Result<T> {
    let mut r = Reader::new(buf);
    let v = T::decode(&mut r)?;
    if r.remaining() != 0 {
        return Err(Error::Codec("trailing bytes after decode".into()));
    }
    Ok(v)
}

/// 便捷函数:从缓冲区前缀解码 T(允许尾部多余字节)。
pub fn from_slice_prefix<T: BinaryDecode>(buf: &[u8]) -> Result<T> {
    let mut r = Reader::new(buf);
    T::decode(&mut r)
}

/// 把实现 ToString 的值写入 writer(用于错误路径)。
pub fn write_debug<W: Write, T: fmt::Debug>(w: &mut W, v: &T) -> std::io::Result<()> {
    write!(w, "{v:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct Demo {
        a: u32,
        b: String,
        c: Vec<u64>,
    }

    impl BinaryEncode for Demo {
        fn encode(&self, w: &mut Writer) {
            self.a.encode(w);
            self.b.encode(w);
            self.c.encode(w);
        }
    }

    impl BinaryDecode for Demo {
        fn decode(r: &mut Reader<'_>) -> Result<Self> {
            Ok(Demo {
                a: u32::decode(r)?,
                b: String::decode(r)?,
                c: Vec::<u64>::decode(r)?,
            })
        }
    }

    #[test]
    fn roundtrip_primitives() {
        let mut w = Writer::new();
        w.u8(7);
        w.u16(1000);
        w.u32(70000);
        w.u64(1 << 50);
        w.i64(-42);
        w.f32(1.5);
        w.bool(true);
        w.varint(300);
        w.str("你好 nebula");
        w.bytes(&[1, 2, 3]);
        let buf = w.into_vec();

        let mut r = Reader::new(&buf);
        assert_eq!(r.u8().unwrap(), 7);
        assert_eq!(r.u16().unwrap(), 1000);
        assert_eq!(r.u32().unwrap(), 70000);
        assert_eq!(r.u64().unwrap(), 1 << 50);
        assert_eq!(r.i64().unwrap(), -42);
        assert_eq!(r.f32().unwrap(), 1.5);
        assert!(r.bool().unwrap());
        assert_eq!(r.varint().unwrap(), 300);
        assert_eq!(r.str().unwrap(), "你好 nebula");
        assert_eq!(r.bytes().unwrap(), vec![1, 2, 3]);
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn roundtrip_struct() {
        let d = Demo {
            a: 9,
            b: "记忆".into(),
            c: vec![1, 2, 3],
        };
        let buf = to_vec(&d);
        let back: Demo = from_slice(&buf).unwrap();
        assert_eq!(d, back);
    }
}
