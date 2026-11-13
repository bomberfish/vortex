//! A minimal DAG-CBOR reader and CBOR → JSON transcoder.
//!
//! DAG-CBOR is a strict CBOR subset (definite lengths, text map keys, tag 42
//! for CID links only), so a full CBOR implementation is unnecessary. The
//! reader is zero-copy; the transcoder streams straight into a `String` using
//! the atproto JSON conventions (`{"$link":…}` for CIDs, `{"$bytes":…}` for
//! byte strings) without building a value tree.

use super::DecodeError;
use crate::did::base32_lower;
use crate::json::write_string;

type Result<T> = std::result::Result<T, DecodeError>;

const MAX_DEPTH: u32 = 128;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Head {
    Uint(u64),
    /// Negative integer `-1 - n`.
    Nint(u64),
    Bytes(usize),
    Text(usize),
    Array(usize),
    Map(usize),
    Tag(u64),
    Bool(bool),
    Null,
    Float(f64),
}

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

fn err<T>(msg: &'static str) -> Result<T> {
    Err(DecodeError::Cbor(msg))
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// A sub-slice of the input between two positions previously returned by
    /// [`Reader::pos`].
    pub fn slice(&self, start: usize, end: usize) -> &'a [u8] {
        self.buf.get(start..end).unwrap_or(&[])
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.buf.len());
        match end {
            Some(end) => {
                let s = &self.buf[self.pos..end];
                self.pos = end;
                Ok(s)
            }
            None => err("truncated"),
        }
    }

    fn uint(&mut self, info: u8) -> Result<u64> {
        Ok(match info {
            0..=23 => info as u64,
            24 => self.take(1)?[0] as u64,
            25 => u16::from_be_bytes(self.take(2)?.try_into().unwrap()) as u64,
            26 => u32::from_be_bytes(self.take(4)?.try_into().unwrap()) as u64,
            27 => u64::from_be_bytes(self.take(8)?.try_into().unwrap()),
            _ => return err("indefinite or reserved length"),
        })
    }

    fn len(&mut self, info: u8) -> Result<usize> {
        let n = self.uint(info)?;
        // A length can never exceed what is left; rejecting early keeps
        // hostile headers from driving large allocations or loops.
        if n > self.remaining() as u64 {
            return err("length exceeds input");
        }
        Ok(n as usize)
    }

    /// Read the next item header. For `Bytes`/`Text` the payload follows and
    /// must be consumed with [`Reader::take`].
    pub fn head(&mut self) -> Result<Head> {
        let b = self.take(1)?[0];
        let (major, info) = (b >> 5, b & 0x1f);
        Ok(match major {
            0 => Head::Uint(self.uint(info)?),
            1 => Head::Nint(self.uint(info)?),
            2 => Head::Bytes(self.len(info)?),
            3 => Head::Text(self.len(info)?),
            4 => Head::Array(self.len(info)?),
            5 => Head::Map(self.len(info)?),
            6 => Head::Tag(self.uint(info)?),
            _ => match info {
                20 => Head::Bool(false),
                21 => Head::Bool(true),
                22 | 23 => Head::Null,
                25 => {
                    let h = u16::from_be_bytes(self.take(2)?.try_into().unwrap());
                    Head::Float(half_to_f64(h))
                }
                26 => Head::Float(f32::from_be_bytes(self.take(4)?.try_into().unwrap()) as f64),
                27 => Head::Float(f64::from_be_bytes(self.take(8)?.try_into().unwrap())),
                _ => return err("unsupported simple value"),
            },
        })
    }

    pub fn text(&mut self, n: usize) -> Result<&'a str> {
        std::str::from_utf8(self.take(n)?).map_err(|_| DecodeError::Cbor("text is not UTF-8"))
    }

    /// Skip one complete item.
    pub fn skip(&mut self) -> Result<()> {
        self.skip_depth(0)
    }

    fn skip_depth(&mut self, depth: u32) -> Result<()> {
        if depth > MAX_DEPTH {
            return err("nesting too deep");
        }
        match self.head()? {
            Head::Bytes(n) | Head::Text(n) => {
                self.take(n)?;
            }
            Head::Array(n) => {
                for _ in 0..n {
                    self.skip_depth(depth + 1)?;
                }
            }
            Head::Map(n) => {
                for _ in 0..n {
                    self.skip_depth(depth + 1)?;
                    self.skip_depth(depth + 1)?;
                }
            }
            Head::Tag(_) => self.skip_depth(depth + 1)?,
            _ => {}
        }
        Ok(())
    }

    /// If the next item is a map, return its entry count; otherwise skip it.
    pub fn map(&mut self) -> Result<Option<usize>> {
        let save = self.pos;
        match self.head()? {
            Head::Map(n) => Ok(Some(n)),
            _ => {
                self.pos = save;
                self.skip()?;
                Ok(None)
            }
        }
    }

    /// If the next item is an array, return its length; otherwise skip it.
    pub fn array(&mut self) -> Result<Option<usize>> {
        let save = self.pos;
        match self.head()? {
            Head::Array(n) => Ok(Some(n)),
            _ => {
                self.pos = save;
                self.skip()?;
                Ok(None)
            }
        }
    }

    /// Read a map key (must be text).
    pub fn key(&mut self) -> Result<&'a str> {
        match self.head()? {
            Head::Text(n) => self.text(n),
            _ => err("map key is not text"),
        }
    }

    /// If the next item is text, return it; otherwise skip it.
    pub fn str(&mut self) -> Result<Option<&'a str>> {
        let save = self.pos;
        match self.head()? {
            Head::Text(n) => self.text(n).map(Some),
            _ => {
                self.pos = save;
                self.skip()?;
                Ok(None)
            }
        }
    }

    /// If the next item is a byte string, return it; otherwise skip it.
    pub fn bytes(&mut self) -> Result<Option<&'a [u8]>> {
        let save = self.pos;
        match self.head()? {
            Head::Bytes(n) => self.take(n).map(Some),
            _ => {
                self.pos = save;
                self.skip()?;
                Ok(None)
            }
        }
    }

    /// If the next item is an integer, return it; otherwise skip it.
    pub fn int(&mut self) -> Result<Option<i64>> {
        let save = self.pos;
        match self.head()? {
            Head::Uint(n) => Ok(i64::try_from(n).ok()),
            Head::Nint(n) => Ok(i64::try_from(n).ok().map(|n| -1 - n)),
            _ => {
                self.pos = save;
                self.skip()?;
                Ok(None)
            }
        }
    }

    /// If the next item is a CID link (tag 42), return the raw CID bytes
    /// (without the leading identity-multibase 0x00); otherwise skip it.
    pub fn cid(&mut self) -> Result<Option<&'a [u8]>> {
        let save = self.pos;
        if let Head::Tag(42) = self.head()? {
            if let Head::Bytes(n) = self.head()? {
                let b = self.take(n)?;
                return Ok(Some(b.strip_prefix(&[0]).unwrap_or(b)));
            }
        }
        self.pos = save;
        self.skip()?;
        Ok(None)
    }

    /// Transcode one item to JSON, appending to `out`.
    pub fn to_json(&mut self, out: &mut String) -> Result<()> {
        self.to_json_depth(out, 0)
    }

    fn to_json_depth(&mut self, out: &mut String, depth: u32) -> Result<()> {
        if depth > MAX_DEPTH {
            return err("nesting too deep");
        }
        match self.head()? {
            Head::Uint(n) => push_display(out, n),
            Head::Nint(n) => push_display(out, -1i128 - n as i128),
            Head::Bytes(n) => {
                let b = self.take(n)?;
                out.push_str("{\"$bytes\":\"");
                base64_unpadded(b, out);
                out.push_str("\"}");
            }
            Head::Text(n) => write_string(out, self.text(n)?),
            Head::Array(n) => {
                out.push('[');
                for i in 0..n {
                    if i > 0 {
                        out.push(',');
                    }
                    self.to_json_depth(out, depth + 1)?;
                }
                out.push(']');
            }
            Head::Map(n) => {
                out.push('{');
                for i in 0..n {
                    if i > 0 {
                        out.push(',');
                    }
                    write_string(out, self.key()?);
                    out.push(':');
                    self.to_json_depth(out, depth + 1)?;
                }
                out.push('}');
            }
            Head::Tag(42) => match self.head()? {
                Head::Bytes(n) => {
                    let b = self.take(n)?;
                    out.push_str("{\"$link\":\"");
                    cid_to_string(b.strip_prefix(&[0]).unwrap_or(b), out);
                    out.push_str("\"}");
                }
                _ => return err("tag 42 without bytes"),
            },
            Head::Tag(_) => self.to_json_depth(out, depth + 1)?,
            Head::Bool(b) => out.push_str(if b { "true" } else { "false" }),
            Head::Null => out.push_str("null"),
            Head::Float(f) => {
                if f.is_finite() {
                    push_display(out, f);
                } else {
                    out.push_str("null");
                }
            }
        }
        Ok(())
    }
}

fn push_display(out: &mut String, v: impl std::fmt::Display) {
    use std::fmt::Write;
    let _ = write!(out, "{v}");
}

fn half_to_f64(h: u16) -> f64 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = ((h >> 10) & 0x1f) as i32;
    let frac = (h & 0x3ff) as f64;
    let v = match exp {
        0 => frac * 2f64.powi(-24),
        31 => {
            if frac == 0.0 {
                f64::INFINITY
            } else {
                f64::NAN
            }
        }
        _ => (1.0 + frac / 1024.0) * 2f64.powi(exp - 15),
    };
    sign * v
}

/// String form of a binary CID: CIDv1 as multibase base32 (`b…`).
pub fn cid_to_string(cid: &[u8], out: &mut String) {
    out.push('b');
    base32_lower(cid, out);
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard-alphabet base64 without padding (the atproto `$bytes` encoding).
pub fn base64_unpadded(data: &[u8], out: &mut String) {
    out.reserve(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(B64[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(B64[n as usize & 63] as char);
        }
    }
}

/// Read an unsigned LEB128 varint (as used by CAR and CIDs).
pub fn read_varint(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let Some(&b) = buf.get(*pos) else {
            return err("truncated varint");
        };
        *pos += 1;
        if shift >= 63 && b > 1 {
            return err("varint overflow");
        }
        v |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok(v);
        }
        shift += 7;
    }
}

#[cfg(test)]
pub(crate) mod testenc {
    //! Tiny CBOR encoder for building test fixtures.
    pub fn head(out: &mut Vec<u8>, major: u8, n: u64) {
        let m = major << 5;
        match n {
            0..=23 => out.push(m | n as u8),
            24..=0xff => out.extend([m | 24, n as u8]),
            0x100..=0xffff => {
                out.push(m | 25);
                out.extend((n as u16).to_be_bytes());
            }
            0x1_0000..=0xffff_ffff => {
                out.push(m | 26);
                out.extend((n as u32).to_be_bytes());
            }
            _ => {
                out.push(m | 27);
                out.extend(n.to_be_bytes());
            }
        }
    }
    pub fn text(out: &mut Vec<u8>, s: &str) {
        head(out, 3, s.len() as u64);
        out.extend(s.as_bytes());
    }
    pub fn bytes(out: &mut Vec<u8>, b: &[u8]) {
        head(out, 2, b.len() as u64);
        out.extend(b);
    }
    pub fn uint(out: &mut Vec<u8>, n: u64) {
        head(out, 0, n);
    }
    pub fn map(out: &mut Vec<u8>, n: u64) {
        head(out, 5, n);
    }
    pub fn array(out: &mut Vec<u8>, n: u64) {
        head(out, 4, n);
    }
    pub fn cid(out: &mut Vec<u8>, cid: &[u8]) {
        out.extend([0xd8, 42]);
        head(out, 2, cid.len() as u64 + 1);
        out.push(0);
        out.extend(cid);
    }
    pub fn bool(out: &mut Vec<u8>, b: bool) {
        out.push(if b { 0xf5 } else { 0xf4 });
    }
    pub fn null(out: &mut Vec<u8>) {
        out.push(0xf6);
    }
}

#[cfg(test)]
mod tests {
    use super::testenc as e;
    use super::*;

    #[test]
    fn transcodes_all_types() {
        let mut b = Vec::new();
        e::map(&mut b, 9);
        e::text(&mut b, "a");
        e::uint(&mut b, 1_000_000);
        e::text(&mut b, "n");
        e::head(&mut b, 1, 41); // -42
        e::text(&mut b, "s");
        e::text(&mut b, "q\"uo\nte ✓");
        e::text(&mut b, "l");
        e::array(&mut b, 3);
        e::bool(&mut b, true);
        e::bool(&mut b, false);
        e::null(&mut b);
        e::text(&mut b, "b");
        e::bytes(&mut b, b"hello");
        e::text(&mut b, "c");
        e::cid(&mut b, &[1, 0x71, 0x12, 0x20, 0xaa, 0xbb]);
        e::text(&mut b, "f");
        b.push(0xfb);
        b.extend(1.5f64.to_be_bytes());
        e::text(&mut b, "e");
        e::map(&mut b, 0);
        e::text(&mut b, "big");
        e::uint(&mut b, u64::MAX);

        let mut out = String::new();
        let mut r = Reader::new(&b);
        r.to_json(&mut out).unwrap();
        assert_eq!(r.remaining(), 0);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["a"], 1_000_000);
        assert_eq!(v["n"], -42);
        assert_eq!(v["s"], "q\"uo\nte ✓");
        assert_eq!(v["l"], serde_json::json!([true, false, null]));
        assert_eq!(v["b"]["$bytes"], "aGVsbG8");
        let mut want = String::from("b");
        base32_lower(&[1, 0x71, 0x12, 0x20, 0xaa, 0xbb], &mut want);
        assert!(want.starts_with("bafyrei"));
        assert_eq!(v["c"]["$link"], want.as_str());
        assert_eq!(v["f"], 1.5);
        assert_eq!(v["e"], serde_json::json!({}));
        assert_eq!(v["big"], u64::MAX);

        let mut r = Reader::new(&b);
        r.skip().unwrap();
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn base64_vectors() {
        for (i, want) in [("", ""), ("f", "Zg"), ("fo", "Zm8"), ("foo", "Zm9v"), ("foob", "Zm9vYg"), ("fooba", "Zm9vYmE")] {
            let mut s = String::new();
            base64_unpadded(i.as_bytes(), &mut s);
            assert_eq!(s, want);
        }
    }

    #[test]
    fn hostile_input_is_rejected() {
        // Huge declared lengths, truncation, indefinite lengths, deep nesting.
        let cases: Vec<Vec<u8>> = vec![
            vec![],
            vec![0x5b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
            vec![0x9b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
            vec![0xbf],
            vec![0x7f],
            vec![0x62, b'a'],
            vec![0xa1, 0x01, 0x02],
            vec![0x81; 4096],
            vec![0xd8, 42, 0x01],
            vec![0x62, 0xff, 0xfe],
        ];
        for c in cases {
            let mut out = String::new();
            assert!(Reader::new(&c).to_json(&mut out).is_err(), "{c:?}");
            let _ = Reader::new(&c).skip();
        }
    }

    #[test]
    fn varints() {
        let mut p = 0;
        assert_eq!(read_varint(&[0x7f], &mut p).unwrap(), 127);
        p = 0;
        assert_eq!(read_varint(&[0x80, 0x01], &mut p).unwrap(), 128);
        p = 0;
        assert!(read_varint(&[0x80], &mut p).is_err());
        p = 0;
        assert!(read_varint(&[0xff; 11], &mut p).is_err());
    }
}
