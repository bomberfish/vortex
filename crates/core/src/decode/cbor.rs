//! A minimal DAG-CBOR reader and CBOR → JSON transcoder.
//!
//! DAG-CBOR is a strict CBOR subset (definite lengths, text map keys, tag 42
//! for CID links only), so a full CBOR implementation is unnecessary. The
//! reader is zero-copy; the transcoder streams straight into a `String` using
//! the atproto JSON conventions (`{"$link":…}` for CIDs, `{"$bytes":…}` for
//! byte strings) without building a value tree.

//!
//! Transcoding is the hot path of the firehose decoder and comes in two
//! forms. [`Reader::to_json_unvalidated`] appends raw bytes and copies text
//! strings without looking at their encoding, so a caller can validate one
//! finished buffer in bulk instead of dozens of short strings one by one.
//! [`Reader::to_json`] is the self-contained form: it runs the same fast path
//! and, should the input turn out to be malformed, replays it through the
//! string-at-a-time transcoder so errors and partial output are exactly what
//! that one produces.

use super::DecodeError;
use crate::did::{base32_lower, base32_lower_bytes};
use crate::json::{write_string, write_string_bytes};

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

    #[inline]
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

    #[inline]
    fn uint(&mut self, info: u8) -> Result<u64> {
        if info < 24 {
            return Ok(info as u64);
        }
        self.uint_wide(info)
    }

    fn uint_wide(&mut self, info: u8) -> Result<u64> {
        Ok(match info {
            24 => self.take(1)?[0] as u64,
            25 => u16::from_be_bytes(self.take(2)?.try_into().unwrap()) as u64,
            26 => u32::from_be_bytes(self.take(4)?.try_into().unwrap()) as u64,
            27 => u64::from_be_bytes(self.take(8)?.try_into().unwrap()),
            _ => return err("indefinite or reserved length"),
        })
    }

    #[inline]
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
    #[inline]
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

    /// Read a map key (must be text) as raw bytes, without checking that it
    /// is UTF-8. Matching it against ASCII literals needs no check; a caller
    /// that falls through to "unknown key" should run [`check_utf8`] to keep
    /// the strictness of [`Reader::key`].
    #[inline]
    pub fn key_bytes(&mut self) -> Result<&'a [u8]> {
        match self.head()? {
            Head::Text(n) => self.take(n),
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
    ///
    /// On error `out` holds whatever was written before the offending item,
    /// as with any streaming writer.
    pub fn to_json(&mut self, out: &mut String) -> Result<()> {
        let start_pos = self.pos;
        let start_len = out.len();
        let mut buf = std::mem::take(out).into_bytes();
        let fast = self.to_json_unvalidated(&mut buf);
        // Validating the whole buffer re-reads the caller's prefix, which is
        // cheap next to checking every string separately.
        let mut prefix = match String::from_utf8(buf) {
            Ok(text) if fast.is_ok() => {
                *out = text;
                return Ok(());
            }
            Ok(text) => text.into_bytes(),
            Err(e) => e.into_bytes(),
        };
        // Malformed input. Start over with the checking transcoder, which
        // stops at the first problem in document order.
        prefix.truncate(start_len);
        // The prefix came out of a `String`, so this cannot fail.
        *out = String::from_utf8(prefix).unwrap_or_default();
        self.pos = start_pos;
        self.transcode_checked(out, 0)
    }

    /// Transcode one item to JSON, appending raw bytes to `out`.
    ///
    /// Text strings are copied (and escaped) without UTF-8 validation: only
    /// ASCII is added around and inside them, so if this returns `Ok` the
    /// appended bytes are exactly what [`Reader::to_json`] would have
    /// produced **if and only if they are valid UTF-8**; otherwise that one
    /// would have failed. The caller must validate before using them as text.
    /// On `Err` the output is partial and should be discarded.
    pub fn to_json_unvalidated(&mut self, out: &mut Vec<u8>) -> Result<()> {
        self.transcode(out, 0)
    }

    fn transcode(&mut self, out: &mut Vec<u8>, depth: u32) -> Result<()> {
        if depth > MAX_DEPTH {
            return err("nesting too deep");
        }
        match self.head()? {
            Head::Uint(n) => push_u64(out, n),
            Head::Nint(n) => {
                // -1 - n; the magnitude n + 1 overflows u64 only for n = MAX.
                out.push(b'-');
                match n.checked_add(1) {
                    Some(m) => push_u64(out, m),
                    None => out.extend_from_slice(b"18446744073709551616"),
                }
            }
            Head::Bytes(n) => {
                let b = self.take(n)?;
                out.extend_from_slice(b"{\"$bytes\":\"");
                base64_unpadded_bytes(b, out);
                out.extend_from_slice(b"\"}");
            }
            Head::Text(n) => write_string_bytes(out, self.take(n)?),
            Head::Array(n) => {
                out.push(b'[');
                for i in 0..n {
                    if i > 0 {
                        out.push(b',');
                    }
                    self.transcode(out, depth + 1)?;
                }
                out.push(b']');
            }
            Head::Map(n) => {
                out.push(b'{');
                for i in 0..n {
                    if i > 0 {
                        out.push(b',');
                    }
                    write_string_bytes(out, self.key_bytes()?);
                    out.push(b':');
                    self.transcode(out, depth + 1)?;
                }
                out.push(b'}');
            }
            Head::Tag(42) => match self.head()? {
                Head::Bytes(n) => {
                    let b = self.take(n)?;
                    out.extend_from_slice(b"{\"$link\":\"");
                    cid_to_bytes(b.strip_prefix(&[0]).unwrap_or(b), out);
                    out.extend_from_slice(b"\"}");
                }
                _ => return err("tag 42 without bytes"),
            },
            Head::Tag(_) => self.transcode(out, depth + 1)?,
            Head::Bool(b) => out.extend_from_slice(if b { b"true" } else { b"false" }),
            Head::Null => out.extend_from_slice(b"null"),
            Head::Float(f) => {
                if f.is_finite() {
                    use std::io::Write;
                    // Same `Display` as the string transcoder; writing to a
                    // `Vec` cannot fail.
                    let _ = write!(out, "{f}");
                } else {
                    out.extend_from_slice(b"null");
                }
            }
        }
        Ok(())
    }

    /// The string-at-a-time transcoder: validates each text as it goes and
    /// therefore defines the error behaviour. Only reached for malformed
    /// input; [`Reader::transcode`] must produce the same bytes on success.
    fn transcode_checked(&mut self, out: &mut String, depth: u32) -> Result<()> {
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
                    self.transcode_checked(out, depth + 1)?;
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
                    self.transcode_checked(out, depth + 1)?;
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
            Head::Tag(_) => self.transcode_checked(out, depth + 1)?,
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

/// The UTF-8 check [`Reader::key`] applies, for keys read with
/// [`Reader::key_bytes`].
#[inline]
pub fn check_utf8(text: &[u8]) -> Result<()> {
    match std::str::from_utf8(text) {
        Ok(_) => Ok(()),
        Err(_) => err("text is not UTF-8"),
    }
}

const DIGIT_PAIRS: &[u8; 200] = b"0001020304050607080910111213141516171819\
2021222324252627282930313233343536373839\
4041424344454647484950515253545556575859\
6061626364656667686970717273747576777879\
8081828384858687888990919293949596979899";

/// Append `n` in decimal, two digits per division and without the `fmt`
/// machinery.
pub fn push_u64(out: &mut Vec<u8>, mut n: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    while n >= 100 {
        let d = (n % 100) as usize * 2;
        n /= 100;
        i -= 2;
        buf[i..i + 2].copy_from_slice(&DIGIT_PAIRS[d..d + 2]);
    }
    if n >= 10 {
        let d = n as usize * 2;
        i -= 2;
        buf[i..i + 2].copy_from_slice(&DIGIT_PAIRS[d..d + 2]);
    } else {
        i -= 1;
        buf[i] = b'0' + n as u8;
    }
    out.extend_from_slice(&buf[i..]);
}

/// Append `n` in decimal.
pub fn push_i64(out: &mut Vec<u8>, n: i64) {
    if n < 0 {
        out.push(b'-');
    }
    push_u64(out, n.unsigned_abs());
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

/// [`cid_to_string`] into a byte buffer.
pub fn cid_to_bytes(cid: &[u8], out: &mut Vec<u8>) {
    out.push(b'b');
    base32_lower_bytes(cid, out);
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

/// [`base64_unpadded`] into a byte buffer, a whole 3-byte group at a time.
pub fn base64_unpadded_bytes(data: &[u8], out: &mut Vec<u8>) {
    out.reserve(data.len().div_ceil(3) * 4);
    let mut chunks = data.chunks_exact(3);
    for c in &mut chunks {
        let n = (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32;
        out.extend_from_slice(&[
            B64[(n >> 18) as usize & 63],
            B64[(n >> 12) as usize & 63],
            B64[(n >> 6) as usize & 63],
            B64[n as usize & 63],
        ]);
    }
    match *chunks.remainder() {
        [a] => out.extend_from_slice(&[B64[(a >> 2) as usize], B64[((a & 3) << 4) as usize]]),
        [a, b] => out.extend_from_slice(&[
            B64[(a >> 2) as usize],
            B64[((a & 3) << 4 | b >> 4) as usize],
            B64[((b & 15) << 2) as usize],
        ]),
        _ => {}
    }
}

/// Read an unsigned LEB128 varint (as used by CAR and CIDs).
#[inline]
pub fn read_varint(buf: &[u8], pos: &mut usize) -> Result<u64> {
    // One byte covers most CID fields and short block lengths.
    if let Some(&b) = buf.get(*pos) {
        if b < 0x80 {
            *pos += 1;
            return Ok(b as u64);
        }
    }
    read_varint_long(buf, pos)
}

fn read_varint_long(buf: &[u8], pos: &mut usize) -> Result<u64> {
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

    /// Text that exercises the JSON escaper: quotes, backslashes, controls,
    /// multi-byte characters, and runs long enough to cross word boundaries.
    pub fn random_text(rng: &mut fastrand::Rng) -> String {
        const ALPHABET: &[&str] = &[
            "a", "b", "z", " ", "0", "/", ":", ".", "#", "@", "-", "_", "\"", "\\", "\n", "\r", "\t", "\u{0}", "\u{1}",
            "\u{8}", "\u{c}", "\u{1f}", "\u{7f}", "\u{80}", "é", "ß", "日", "本", "語", "😀", "\u{2028}", "\u{feff}", "\u{ffff}",
        ];
        let len = match rng.u8(..10) {
            0 => 0,
            1..=6 => rng.usize(1..12),
            7 | 8 => rng.usize(12..80),
            _ => rng.usize(80..400),
        };
        // Mostly plain ASCII with the odd special, like real records.
        let spice = rng.u8(..4) == 0;
        (0..len)
            .map(|_| if spice || rng.u8(..12) == 0 { ALPHABET[rng.usize(..ALPHABET.len())] } else { ALPHABET[rng.usize(..12)] })
            .collect()
    }

    fn random_len_arg(rng: &mut fastrand::Rng) -> u64 {
        *rng.choice(&[0u64, 1, 23, 24, 255, 256, 65_535, 65_536, u32::MAX as u64, u32::MAX as u64 + 1, i64::MAX as u64, u64::MAX])
            .unwrap()
    }

    /// One random item. With `hostile` set, some items are deliberately
    /// outside DAG-CBOR (invalid UTF-8, non-text keys, bad tags, reserved
    /// simple values, indefinite or impossible lengths).
    pub fn random_value(rng: &mut fastrand::Rng, out: &mut Vec<u8>, depth: u32, hostile: bool) {
        if hostile && rng.u8(..40) == 0 {
            match rng.u8(..7) {
                0 => {
                    // Text that is not UTF-8.
                    let bad: &[u8] = rng
                        .choice(&[&b"\xff"[..], b"ok\xe3\x81", b"\xe3\"\x81", b"\xc0\x80", b"a\xed\xa0\x80b", b"\xf0\x9f\x98", b"\x80"])
                        .unwrap();
                    head(out, 3, bad.len() as u64);
                    out.extend(bad);
                }
                1 => {
                    // Map with a non-text key.
                    map(out, 1);
                    uint(out, 1);
                    uint(out, 2);
                }
                2 => {
                    // Tag 42 without a byte string.
                    out.extend([0xd8, 42]);
                    uint(out, 7);
                }
                3 => out.push(*rng.choice(&[0xe0u8, 0xf8, 0xf0, 0xff, 0xfc]).unwrap()),
                4 => out.push(*rng.choice(&[0x5fu8, 0x7f, 0x9f, 0xbf, 0x1c, 0x3f]).unwrap()),
                5 => head(out, rng.u8(2..6), random_len_arg(rng)),
                _ => {
                    // Map whose key is not UTF-8.
                    map(out, 1);
                    head(out, 3, 2);
                    out.extend([0xc3, 0x28]);
                    null(out);
                }
            }
            return;
        }
        let leaf = depth >= 6 || rng.u8(..3) != 0;
        if leaf {
            match rng.u8(..16) {
                0 | 1 => uint(out, rng.u64(..1000)),
                2 => uint(out, random_len_arg(rng)),
                3 => head(out, 1, rng.u64(..1000)),
                4 => head(out, 1, random_len_arg(rng)),
                5 => {
                    let n = if rng.u8(..20) == 0 { rng.usize(1000..9000) } else { rng.usize(..40) };
                    bytes(out, &(0..n).map(|_| rng.u8(..)).collect::<Vec<_>>());
                }
                6 => {
                    // A link, with a realistic CID or arbitrary bytes.
                    if rng.bool() {
                        cid(out, &super::super::car::testenc::fake_cid(rng.u8(..)));
                    } else {
                        let n = rng.usize(..50);
                        out.extend([0xd8, 42]);
                        bytes(out, &(0..n).map(|_| rng.u8(..2)).collect::<Vec<_>>());
                    }
                }
                7 => bool(out, rng.bool()),
                8 => out.push(if rng.bool() { 0xf6 } else { 0xf7 }),
                9 => {
                    // Floats of every width, including the non-finite ones.
                    match rng.u8(..3) {
                        0 => {
                            out.push(0xf9);
                            let h = *rng.choice(&[0u16, 1, 0x3c00, 0xc000, 0x7bff, 0x8000, 0x7c00, 0xfc00, 0x7e00, 0x03ff, 0x0400]).unwrap();
                            out.extend(if rng.bool() { h } else { rng.u16(..) }.to_be_bytes());
                        }
                        1 => {
                            out.push(0xfa);
                            let f = *rng.choice(&[0.0f32, -0.0, 1.5, 3.4e38, 1e-45, f32::NAN, f32::INFINITY, 16_777_216.0]).unwrap();
                            out.extend(if rng.bool() { f } else { f32::from_bits(rng.u32(..)) }.to_be_bytes());
                        }
                        _ => {
                            out.push(0xfb);
                            let f = *rng
                                .choice(&[0.0f64, -0.0, 1.5, 0.1, 1e300, 5e-324, 1e21, 1e-7, f64::NAN, f64::NEG_INFINITY, 9_007_199_254_740_993.0])
                                .unwrap();
                            out.extend(if rng.bool() { f } else { f64::from_bits(rng.u64(..)) }.to_be_bytes());
                        }
                    }
                }
                _ => text(out, &random_text(rng)),
            }
            return;
        }
        match rng.u8(..8) {
            0..=2 => {
                let n = rng.u64(..5);
                array(out, n);
                for _ in 0..n {
                    random_value(rng, out, depth + 1, hostile);
                }
            }
            3 => {
                // A non-link tag is transparent.
                head(out, 6, *rng.choice(&[0u64, 1, 41, 43, 55_799]).unwrap());
                random_value(rng, out, depth + 1, hostile);
            }
            _ => {
                let n = rng.u64(..6);
                map(out, n);
                for _ in 0..n {
                    let key = if rng.bool() {
                        (*rng.choice(&["$type", "text", "uri", "cid", "createdAt", "subject", "langs", "embed", "facets"]).unwrap()).to_owned()
                    } else {
                        random_text(rng)
                    };
                    text(out, &key);
                    random_value(rng, out, depth + 1, hostile);
                }
            }
        }
    }
}

/// The transcoder exactly as it was before the byte-oriented fast path:
/// string-at-a-time validation, `fmt`-based numbers, char-at-a-time escaping.
/// It is the oracle for the differential tests here and in `firehose`.
#[cfg(test)]
pub(crate) mod reference {
    use super::*;
    use crate::json::reference::write_string;

    pub fn to_json(r: &mut Reader, out: &mut String) -> Result<()> {
        transcode_checked(r, out, 0)
    }

    fn key<'a>(r: &mut Reader<'a>) -> Result<&'a str> {
        match r.head()? {
            Head::Text(n) => r.text(n),
            _ => err("map key is not text"),
        }
    }

    fn transcode_checked(r: &mut Reader, out: &mut String, depth: u32) -> Result<()> {
        if depth > MAX_DEPTH {
            return err("nesting too deep");
        }
        match r.head()? {
            Head::Uint(n) => push_display(out, n),
            Head::Nint(n) => push_display(out, -1i128 - n as i128),
            Head::Bytes(n) => {
                let b = r.take(n)?;
                out.push_str("{\"$bytes\":\"");
                base64_unpadded(b, out);
                out.push_str("\"}");
            }
            Head::Text(n) => write_string(out, r.text(n)?),
            Head::Array(n) => {
                out.push('[');
                for i in 0..n {
                    if i > 0 {
                        out.push(',');
                    }
                    transcode_checked(r, out, depth + 1)?;
                }
                out.push(']');
            }
            Head::Map(n) => {
                out.push('{');
                for i in 0..n {
                    if i > 0 {
                        out.push(',');
                    }
                    write_string(out, key(r)?);
                    out.push(':');
                    transcode_checked(r, out, depth + 1)?;
                }
                out.push('}');
            }
            Head::Tag(42) => match r.head()? {
                Head::Bytes(n) => {
                    let b = r.take(n)?;
                    out.push_str("{\"$link\":\"");
                    cid_to_string(b.strip_prefix(&[0]).unwrap_or(b), out);
                    out.push_str("\"}");
                }
                _ => return err("tag 42 without bytes"),
            },
            Head::Tag(_) => transcode_checked(r, out, depth + 1)?,
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

    /// Run the production transcoder and the reference over the same bytes
    /// and require identical results: output (also the partial output of a
    /// failure), error and final reader position.
    fn assert_same_as_reference(input: &[u8], prefix: &str) {
        let mut want = prefix.to_owned();
        let mut wr = Reader::new(input);
        let want_res = reference::to_json(&mut wr, &mut want).map_err(|e| e.to_string());

        let mut got = prefix.to_owned();
        let mut gr = Reader::new(input);
        let got_res = gr.to_json(&mut got).map_err(|e| e.to_string());
        assert_eq!(got_res, want_res, "{input:x?}");
        assert_eq!(got, want, "{input:x?}");
        assert_eq!(gr.pos(), wr.pos(), "{input:x?}");

        // The raw form: succeeds with valid UTF-8 exactly when the reference
        // succeeds, and then with the same bytes.
        let mut raw = prefix.as_bytes().to_vec();
        let mut rr = Reader::new(input);
        let raw_ok = rr.to_json_unvalidated(&mut raw).is_ok() && std::str::from_utf8(&raw).is_ok();
        assert_eq!(raw_ok, want_res.is_ok(), "{input:x?}");
        if raw_ok {
            assert_eq!(raw, want.as_bytes(), "{input:x?}");
            assert_eq!(rr.pos(), wr.pos());
        }
    }

    fn json_of(input: &[u8]) -> String {
        assert_same_as_reference(input, "");
        let mut out = String::new();
        Reader::new(input).to_json(&mut out).unwrap();
        out
    }

    #[test]
    fn differential_random_well_formed() {
        let mut rng = fastrand::Rng::with_seed(0xcb02);
        let mut ok = 0;
        for i in 0..20_000 {
            let mut b = Vec::new();
            e::random_value(&mut rng, &mut b, 0, false);
            assert_same_as_reference(&b, if i % 7 == 0 { "{\"record\":" } else { "" });
            let mut out = String::new();
            if Reader::new(&b).to_json(&mut out).is_ok() {
                ok += 1;
                // Whatever comes out must be JSON.
                serde_json::from_str::<serde_json::Value>(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
            }
        }
        // Only length arguments beyond the input can fail here.
        assert!(ok > 15_000, "{ok}");
    }

    #[test]
    fn differential_random_hostile() {
        let mut rng = fastrand::Rng::with_seed(0xbad);
        let mut failed = 0;
        for _ in 0..20_000 {
            let mut b = Vec::new();
            e::random_value(&mut rng, &mut b, 0, true);
            // Then damage some of them: truncate, flip, splice.
            match rng.u8(..6) {
                0 if !b.is_empty() => b.truncate(rng.usize(..b.len())),
                1 if !b.is_empty() => {
                    for _ in 0..rng.usize(1..4) {
                        let i = rng.usize(..b.len());
                        b[i] = rng.u8(..);
                    }
                }
                2 if !b.is_empty() => {
                    let i = rng.usize(..b.len());
                    b.insert(i, rng.u8(..));
                }
                _ => {}
            }
            assert_same_as_reference(&b, "");
            failed += Reader::new(&b).to_json(&mut String::new()).is_err() as u32;
            let _ = Reader::new(&b).skip();
        }
        assert!(failed > 2_000, "{failed}");
    }

    #[test]
    fn differential_every_two_byte_prefix() {
        // Every initial byte (all majors, all argument widths), followed by
        // enough payload to satisfy short lengths.
        for first in 0..=255u8 {
            for second in [0u8, 1, 0x17, 0x18, 0x41, 0x61, 0x7f, 0x80, 0xc3, 0xff] {
                let mut b = vec![first, second];
                b.extend(b"abcdefgh\"\\\n\x01ijklmnopqrstuvwxyz0123456789");
                assert_same_as_reference(&b, "");
                assert_same_as_reference(&b[..2], "x");
                assert_same_as_reference(&b[..1], "");
            }
        }
    }

    #[test]
    fn strings_escape_like_serde() {
        for s in [
            "",
            "plain",
            "q\"uo\\te",
            "line\nbreak\ttab\rcr",
            "\u{0}\u{1}\u{2}\u{1f} controls \u{7f}",
            "é ß 日本語 😀 \u{2028}\u{2029}",
            "exactly8",
            "sixteen bytes!!!",
            "a\"b\"c\"d\"e\"f\"g\"h\"i",
            "\\\\\\\\\\\\\\\\",
        ] {
            for pad in 0..9 {
                let s = format!("{}{s}", "·".repeat(pad));
                let mut b = Vec::new();
                e::map(&mut b, 1);
                e::text(&mut b, &s);
                e::text(&mut b, &s);
                let out = json_of(&b);
                let v: serde_json::Value = serde_json::from_str(&out).unwrap();
                assert_eq!(v[&s], s.as_str());
            }
        }
        // Escapes use the short forms and lowercase \u00xx, nothing else.
        let mut b = Vec::new();
        e::text(&mut b, "\"\\/\u{8}\u{c}\n\r\t\u{1}\u{1f}\u{7f}é");
        assert_eq!(json_of(&b), r#""\"\\/\u0008\u000c\n\r\t\u0001\u001f\u{7f}é""#.replace("\\u{7f}", "\u{7f}"));
    }

    #[test]
    fn invalid_utf8_is_an_error_at_the_offending_string() {
        // In a value: everything before it has been written.
        let mut b = Vec::new();
        e::map(&mut b, 3);
        e::text(&mut b, "ok");
        e::text(&mut b, "fine");
        e::text(&mut b, "bad");
        e::head(&mut b, 3, 3);
        b.extend(b"a\xff\"");
        e::text(&mut b, "after");
        e::uint(&mut b, 1);
        assert_same_as_reference(&b, "");
        let mut out = String::from("pre");
        let err = Reader::new(&b).to_json(&mut out).unwrap_err();
        assert_eq!(err.to_string(), "cbor: text is not UTF-8");
        assert_eq!(out, r#"pre{"ok":"fine","bad":"#);
        // The raw form does not notice, but its output is not UTF-8.
        let mut raw = Vec::new();
        Reader::new(&b).to_json_unvalidated(&mut raw).unwrap();
        assert!(std::str::from_utf8(&raw).is_err());

        // In a key.
        let mut b = Vec::new();
        e::map(&mut b, 1);
        e::head(&mut b, 3, 2);
        b.extend([0xc3, 0x28]);
        e::null(&mut b);
        assert_same_as_reference(&b, "");
        assert!(Reader::new(&b).to_json(&mut String::new()).is_err());

        // A sequence split across two strings is invalid in both, even
        // though the bytes would be valid if the strings were glued together.
        let mut b = Vec::new();
        e::array(&mut b, 2);
        e::head(&mut b, 3, 1);
        b.push(0xc3);
        e::head(&mut b, 3, 1);
        b.push(0xa9);
        assert_same_as_reference(&b, "");
        assert!(Reader::new(&b).to_json(&mut String::new()).is_err());

        // An earlier bad string wins over a later structural error.
        let mut b = Vec::new();
        e::array(&mut b, 2);
        e::head(&mut b, 3, 1);
        b.push(0x80);
        b.extend([0xd8, 42, 0x01]);
        assert_same_as_reference(&b, "");
        assert_eq!(Reader::new(&b).to_json(&mut String::new()).unwrap_err().to_string(), "cbor: text is not UTF-8");
    }

    #[test]
    fn integers_at_every_boundary() {
        let mut values = vec![0u64, 1, 9, 10, 23, 24, 99, 100, 255, 256, 999, 1000, 65_535, 65_536, u32::MAX as u64, u32::MAX as u64 + 1];
        values.extend([i64::MAX as u64, i64::MAX as u64 + 1, u64::MAX - 1, u64::MAX]);
        let mut p = 1u64;
        for _ in 0..19 {
            p *= 10;
            values.extend([p - 1, p, p + 1]);
        }
        for n in values {
            let mut out = Vec::new();
            push_u64(&mut out, n);
            assert_eq!(out, n.to_string().as_bytes());
            let mut b = Vec::new();
            e::uint(&mut b, n);
            assert_eq!(json_of(&b), n.to_string());
            let mut b = Vec::new();
            e::head(&mut b, 1, n);
            assert_eq!(json_of(&b), (-1i128 - n as i128).to_string());
        }
        for n in [0i64, 1, -1, 42, -42, i64::MAX, i64::MIN, i64::MIN + 1] {
            let mut out = Vec::new();
            push_i64(&mut out, n);
            assert_eq!(out, n.to_string().as_bytes());
        }
        let mut rng = fastrand::Rng::with_seed(64);
        for _ in 0..20_000 {
            let n = rng.u64(..) >> rng.u32(..64);
            let mut out = Vec::new();
            push_u64(&mut out, n);
            assert_eq!(out, n.to_string().as_bytes());
        }
    }

    #[test]
    fn floats_including_half_precision() {
        let half = |h: u16| {
            let mut b = vec![0xf9];
            b.extend(h.to_be_bytes());
            json_of(&b)
        };
        assert_eq!(half(0x0000), "0");
        assert_eq!(half(0x8000), "-0");
        assert_eq!(half(0x3c00), "1");
        assert_eq!(half(0xc000), "-2");
        assert_eq!(half(0x3e00), "1.5");
        assert_eq!(half(0x7bff), "65504");
        assert_eq!(half(0x0001), "0.00000005960464477539063");
        assert_eq!(half(0x0400), "0.00006103515625");
        assert_eq!(half(0x7c00), "null");
        assert_eq!(half(0xfc00), "null");
        assert_eq!(half(0x7e00), "null");
        // Every half-precision value agrees with the reference.
        for h in 0..=u16::MAX {
            let mut b = vec![0xf9];
            b.extend(h.to_be_bytes());
            assert_same_as_reference(&b, "");
        }
        let mut b = vec![0xfa];
        b.extend(0.1f32.to_be_bytes());
        assert_eq!(json_of(&b), (0.1f32 as f64).to_string());
        for (f, want) in [(1.5f64, "1.5"), (-0.0, "-0"), (1e21, "1000000000000000000000"), (0.1, "0.1"), (f64::NAN, "null"), (f64::INFINITY, "null")] {
            let mut b = vec![0xfb];
            b.extend(f.to_be_bytes());
            assert_eq!(json_of(&b), want);
        }
    }

    #[test]
    fn bytes_of_every_length_and_large() {
        let mut rng = fastrand::Rng::with_seed(0xb64);
        for len in (0..70usize).chain([255, 256, 257, 4095, 4096, 65_536, 300_001]) {
            let data: Vec<u8> = (0..len).map(|_| rng.u8(..)).collect();
            let mut want = String::new();
            base64_unpadded(&data, &mut want);
            let mut got = Vec::new();
            base64_unpadded_bytes(&data, &mut got);
            assert_eq!(got, want.as_bytes(), "len {len}");
            assert_eq!(want.len(), (len * 4).div_ceil(3));
            let mut b = Vec::new();
            e::bytes(&mut b, &data);
            assert_eq!(json_of(&b), format!("{{\"$bytes\":\"{want}\"}}"));
        }
        // Known vector with every 6-bit value.
        let all: Vec<u8> = (0..48u32).flat_map(|i| [(i * 4 * 4 + 3) as u8]).collect();
        let mut got = Vec::new();
        base64_unpadded_bytes(&all, &mut got);
        let mut want = String::new();
        base64_unpadded(&all, &mut want);
        assert_eq!(got, want.as_bytes());
    }

    #[test]
    fn links_of_every_shape() {
        let mut rng = fastrand::Rng::with_seed(0xc1d);
        // Real-shaped CIDv1, a CIDv0, and odd lengths with and without the
        // identity-multibase prefix.
        let mut cids: Vec<Vec<u8>> = vec![super::super::car::testenc::fake_cid(7), {
            let mut c = vec![0x12, 0x20];
            c.extend([9u8; 32]);
            c
        }];
        for len in 0..45 {
            cids.push((0..len).map(|_| rng.u8(..)).collect());
        }
        for c in &cids {
            let mut want = String::new();
            cid_to_string(c, &mut want);
            let mut got = Vec::new();
            cid_to_bytes(c, &mut got);
            assert_eq!(got, want.as_bytes());
            let mut b = Vec::new();
            e::cid(&mut b, c);
            assert_eq!(json_of(&b), format!("{{\"$link\":\"{want}\"}}"));
            // Without the 0x00 prefix the bytes are taken as they are.
            let mut b = vec![0xd8, 42];
            e::bytes(&mut b, c);
            assert_same_as_reference(&b, "");
        }
        assert!(json_of(&{
            let mut b = Vec::new();
            e::cid(&mut b, &cids[0]);
            b
        })
        .starts_with("{\"$link\":\"bafyrei"));
    }

    #[test]
    fn nesting_limit_is_unchanged() {
        for depth in [1usize, 2, 127, 128, 129, 130, 1000] {
            // Arrays, maps and transparent tags all count.
            let mut arrays = vec![0x81u8; depth];
            arrays.push(0x01);
            let mut maps = Vec::new();
            for _ in 0..depth {
                maps.extend([0xa1, 0x61, b'k']);
            }
            maps.push(0xf6);
            let mut tags = vec![0xc1u8; depth];
            tags.push(0x01);
            for b in [arrays, maps, tags] {
                assert_same_as_reference(&b, "");
                let res = Reader::new(&b).to_json(&mut String::new());
                assert_eq!(res.is_ok(), depth <= MAX_DEPTH as usize, "depth {depth}");
                assert_eq!(Reader::new(&b).skip().is_ok(), depth <= MAX_DEPTH as usize);
            }
        }
    }

    #[test]
    fn key_bytes_matches_key_on_valid_input() {
        let mut b = Vec::new();
        e::text(&mut b, "ops");
        e::head(&mut b, 3, 1);
        b.push(0xff);
        e::uint(&mut b, 1);
        let mut r = Reader::new(&b);
        assert_eq!(r.key_bytes().unwrap(), b"ops");
        let bad = r.key_bytes().unwrap();
        assert!(check_utf8(bad).is_err());
        assert!(check_utf8(b"ok").is_ok());
        assert!(r.key_bytes().is_err());
        let mut r = Reader::new(&b);
        r.key().unwrap();
        assert_eq!(r.key().unwrap_err().to_string(), check_utf8(bad).unwrap_err().to_string());
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
