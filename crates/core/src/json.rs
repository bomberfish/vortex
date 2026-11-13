//! A minimal zero-allocation JSON pull scanner.
//!
//! The decode hot path only needs to pick a handful of fields out of each
//! frame and skip everything else, so instead of building a value tree (or
//! paying for a general-purpose deserializer's allocations) the scanner walks
//! the text once. String bodies are located with `memchr`, which is where most
//! of a frame's bytes live. Input is `&str`, so UTF-8 validity is established
//! once per frame and every slice handed back is valid by construction.
//!
//! The scanner is lenient about structure it does not need to understand
//! (it is not a validator) but never panics or reads out of bounds.

use memchr::memchr2;
use std::borrow::Cow;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonError {
    pub pos: usize,
    pub msg: &'static str,
}

impl std::fmt::Display for JsonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} at byte {}", self.msg, self.pos)
    }
}
impl std::error::Error for JsonError {}

pub type Result<T> = std::result::Result<T, JsonError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    String,
    Object,
    Array,
    Number,
    Bool,
    Null,
}

/// A JSON string token: the text between the quotes, still escaped.
#[derive(Debug, Clone, Copy)]
pub struct Str<'a> {
    raw: &'a str,
    escaped: bool,
}

impl<'a> Str<'a> {
    /// The raw (still escaped) contents.
    #[inline]
    pub fn raw(&self) -> &'a str {
        self.raw
    }

    #[inline]
    pub fn has_escapes(&self) -> bool {
        self.escaped
    }

    /// Compare against a literal that itself needs no escaping.
    #[inline]
    pub fn is(&self, lit: &str) -> bool {
        if !self.escaped {
            self.raw == lit
        } else {
            self.to_cow() == lit
        }
    }

    /// Append the unescaped value to `out`.
    #[inline]
    pub fn push_to(&self, out: &mut String) {
        if !self.escaped {
            out.push_str(self.raw);
        } else {
            unescape_into(self.raw, out);
        }
    }

    pub fn to_cow(&self) -> Cow<'a, str> {
        if !self.escaped {
            Cow::Borrowed(self.raw)
        } else {
            let mut s = String::with_capacity(self.raw.len());
            unescape_into(self.raw, &mut s);
            Cow::Owned(s)
        }
    }
}

fn hex4(b: &[u8]) -> Option<u32> {
    if b.len() < 4 {
        return None;
    }
    let mut v = 0u32;
    for &c in &b[..4] {
        v = (v << 4) | (c as char).to_digit(16)?;
    }
    Some(v)
}

/// Unescape the body of a JSON string. Invalid escapes degrade to U+FFFD
/// rather than failing: this is display/filter data, not a round-trip.
pub fn unescape_into(raw: &str, out: &mut String) {
    let bytes = raw.as_bytes();
    let mut pos = 0;
    while pos < bytes.len() {
        match memchr::memchr(b'\\', &bytes[pos..]) {
            None => {
                out.push_str(&raw[pos..]);
                return;
            }
            Some(i) => {
                out.push_str(&raw[pos..pos + i]);
                pos += i + 1;
                let Some(&esc) = bytes.get(pos) else {
                    out.push('\u{fffd}');
                    return;
                };
                pos += 1;
                match esc {
                    b'n' => out.push('\n'),
                    b't' => out.push('\t'),
                    b'r' => out.push('\r'),
                    b'b' => out.push('\u{8}'),
                    b'f' => out.push('\u{c}'),
                    b'u' => {
                        let Some(hi) = hex4(&bytes[pos..]) else {
                            out.push('\u{fffd}');
                            continue;
                        };
                        pos += 4;
                        let cp = if (0xd800..0xdc00).contains(&hi) {
                            // High surrogate: needs a following \uXXXX low surrogate.
                            match (bytes.get(pos), bytes.get(pos + 1), hex4(bytes.get(pos + 2..).unwrap_or(&[]))) {
                                (Some(b'\\'), Some(b'u'), Some(lo)) if (0xdc00..0xe000).contains(&lo) => {
                                    pos += 6;
                                    0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00)
                                }
                                _ => 0xfffd,
                            }
                        } else {
                            hi
                        };
                        out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                    }
                    // `"`, `\`, `/` and anything unknown: take the byte literally.
                    // `esc` is ASCII here or the start of a multi-byte char; step
                    // back so the general copy handles multi-byte correctly.
                    c if c.is_ascii() => out.push(c as char),
                    _ => pos -= 1,
                }
            }
        }
    }
}

pub struct Scanner<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Scanner<'a> {
    #[inline]
    pub fn new(src: &'a str) -> Self {
        Scanner { src, pos: 0 }
    }

    #[inline]
    pub fn pos(&self) -> usize {
        self.pos
    }

    #[inline]
    fn err<T>(&self, msg: &'static str) -> Result<T> {
        Err(JsonError { pos: self.pos, msg })
    }

    #[inline]
    fn ws(&mut self) {
        let b = self.src.as_bytes();
        while let Some(&c) = b.get(self.pos) {
            if c == b' ' || c == b'\n' || c == b'\t' || c == b'\r' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    /// Next non-whitespace byte, without consuming it.
    #[inline]
    pub fn peek(&mut self) -> Option<u8> {
        self.ws();
        self.src.as_bytes().get(self.pos).copied()
    }

    /// True once only whitespace remains.
    pub fn at_end(&mut self) -> bool {
        self.peek().is_none()
    }

    pub fn kind(&mut self) -> Result<ValueKind> {
        match self.peek() {
            Some(b'"') => Ok(ValueKind::String),
            Some(b'{') => Ok(ValueKind::Object),
            Some(b'[') => Ok(ValueKind::Array),
            Some(b't' | b'f') => Ok(ValueKind::Bool),
            Some(b'n') => Ok(ValueKind::Null),
            Some(b'-' | b'0'..=b'9') => Ok(ValueKind::Number),
            Some(_) => self.err("unexpected character"),
            None => self.err("unexpected end of input"),
        }
    }

    /// Scan a string token starting at the opening quote.
    #[inline]
    fn string_token(&mut self) -> Result<Str<'a>> {
        let bytes = self.src.as_bytes();
        debug_assert_eq!(bytes.get(self.pos), Some(&b'"'));
        let start = self.pos + 1;
        let mut p = start;
        let mut escaped = false;
        loop {
            match memchr2(b'"', b'\\', bytes.get(p..).unwrap_or(&[])) {
                None => {
                    self.pos = bytes.len();
                    return self.err("unterminated string");
                }
                Some(i) => {
                    if bytes[p + i] == b'"' {
                        let end = p + i;
                        self.pos = end + 1;
                        // Both ends sit on ASCII quotes, so this is a char boundary.
                        return Ok(Str { raw: &self.src[start..end], escaped });
                    }
                    escaped = true;
                    p += i + 2;
                }
            }
        }
    }

    /// If the next value is a string, consume and return it; otherwise skip
    /// the value and return `None`.
    #[inline]
    pub fn str(&mut self) -> Result<Option<Str<'a>>> {
        if self.peek() == Some(b'"') {
            self.string_token().map(Some)
        } else {
            self.skip()?;
            Ok(None)
        }
    }

    fn scalar_token(&mut self) -> &'a str {
        let bytes = self.src.as_bytes();
        let start = self.pos;
        while let Some(&c) = bytes.get(self.pos) {
            if matches!(c, b',' | b'}' | b']' | b' ' | b'\n' | b'\t' | b'\r' | b':' | b'"' | b'{' | b'[') {
                break;
            }
            self.pos += 1;
        }
        // Delimiters are ASCII, so both ends are char boundaries.
        &self.src[start..self.pos]
    }

    /// If the next value is an integer that fits `i64`, return it; otherwise
    /// skip the value and return `None`.
    pub fn i64(&mut self) -> Result<Option<i64>> {
        match self.peek() {
            Some(b'-' | b'0'..=b'9') => Ok(self.scalar_token().parse().ok()),
            _ => {
                self.skip()?;
                Ok(None)
            }
        }
    }

    /// If the next value is a number, return it as `f64`; otherwise skip.
    pub fn f64(&mut self) -> Result<Option<f64>> {
        match self.peek() {
            Some(b'-' | b'0'..=b'9') => Ok(self.scalar_token().parse().ok()),
            _ => {
                self.skip()?;
                Ok(None)
            }
        }
    }

    /// If the next value is a boolean, return it; otherwise skip.
    pub fn bool(&mut self) -> Result<Option<bool>> {
        match self.peek() {
            Some(b't' | b'f') => Ok(match self.scalar_token() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            }),
            _ => {
                self.skip()?;
                Ok(None)
            }
        }
    }

    /// Skip one value of any kind.
    pub fn skip(&mut self) -> Result<()> {
        match self.peek() {
            Some(b'"') => self.string_token().map(|_| ()),
            Some(b'{' | b'[') => self.skip_container(),
            Some(b',' | b'}' | b']' | b':') => self.err("expected a value"),
            Some(_) => {
                self.scalar_token();
                Ok(())
            }
            None => self.err("unexpected end of input"),
        }
    }

    fn skip_container(&mut self) -> Result<()> {
        let bytes = self.src.as_bytes();
        let mut depth = 0usize;
        loop {
            let Some(&c) = bytes.get(self.pos) else {
                return self.err("unterminated container");
            };
            match c {
                b'"' => {
                    self.string_token()?;
                    continue;
                }
                b'{' | b'[' => depth += 1,
                b'}' | b']' => {
                    depth -= 1;
                    if depth == 0 {
                        self.pos += 1;
                        return Ok(());
                    }
                }
                _ => {}
            }
            self.pos += 1;
        }
    }

    /// Skip one value and return its raw text.
    pub fn raw(&mut self) -> Result<&'a str> {
        self.ws();
        let start = self.pos;
        self.skip()?;
        Ok(&self.src[start..self.pos])
    }

    /// If the next value is an object, consume `{` and return true. Otherwise
    /// skip the value and return false. Iterate with [`Scanner::key`].
    #[inline]
    pub fn object(&mut self) -> Result<bool> {
        if self.peek() == Some(b'{') {
            self.pos += 1;
            Ok(true)
        } else {
            self.skip()?;
            Ok(false)
        }
    }

    /// Next key of the current object (raw, still escaped), positioned at its
    /// value, or `None` after consuming the closing `}`. The caller must
    /// consume or skip the value before calling again.
    #[inline]
    pub fn key(&mut self) -> Result<Option<&'a str>> {
        if self.peek() == Some(b',') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'}') => {
                self.pos += 1;
                Ok(None)
            }
            Some(b'"') => {
                let k = self.string_token()?;
                if self.peek() != Some(b':') {
                    return self.err("expected ':'");
                }
                self.pos += 1;
                Ok(Some(k.raw))
            }
            Some(_) => self.err("expected object key"),
            None => self.err("unterminated object"),
        }
    }

    /// If the next value is an array, consume `[` and return true. Otherwise
    /// skip the value and return false. Iterate with [`Scanner::elem`].
    #[inline]
    pub fn array(&mut self) -> Result<bool> {
        if self.peek() == Some(b'[') {
            self.pos += 1;
            Ok(true)
        } else {
            self.skip()?;
            Ok(false)
        }
    }

    /// True if another element follows (positioned at it); false after
    /// consuming the closing `]`.
    #[inline]
    pub fn elem(&mut self) -> Result<bool> {
        if self.peek() == Some(b',') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b']') => {
                self.pos += 1;
                Ok(false)
            }
            Some(_) => Ok(true),
            None => self.err("unterminated array"),
        }
    }
}

/// Re-indent compact JSON for display. Tolerates malformed input (it only
/// tracks strings and brackets).
pub fn pretty(src: &str, indent: usize) -> String {
    let mut out = String::with_capacity(src.len() * 2);
    let bytes = src.as_bytes();
    let mut depth = 0usize;
    let mut i = 0;
    let newline = |out: &mut String, depth: usize| {
        out.push('\n');
        for _ in 0..depth * indent {
            out.push(' ');
        }
    };
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'"' => {
                let start = i;
                i += 1;
                while i < bytes.len() {
                    match bytes[i] {
                        b'\\' => i += 2,
                        b'"' => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
                let end = i.min(bytes.len());
                out.push_str(src.get(start..end).unwrap_or_default());
                continue;
            }
            b'{' | b'[' => {
                // Keep empty containers on one line.
                let mut j = i + 1;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < bytes.len() && (bytes[j] == b'}' || bytes[j] == b']') {
                    out.push(c as char);
                    out.push(bytes[j] as char);
                    i = j + 1;
                    continue;
                }
                depth += 1;
                out.push(c as char);
                newline(&mut out, depth);
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                newline(&mut out, depth);
                out.push(c as char);
            }
            b',' => {
                out.push(',');
                newline(&mut out, depth);
            }
            b':' => out.push_str(": "),
            b' ' | b'\n' | b'\t' | b'\r' => {}
            _ => {
                // Scalars are ASCII; copy the run.
                let start = i;
                while i < bytes.len()
                    && !matches!(bytes[i], b',' | b'}' | b']' | b' ' | b'\n' | b'\t' | b'\r' | b':' | b'"' | b'{' | b'[')
                {
                    i += 1;
                }
                out.push_str(src.get(start..i).unwrap_or_default());
                if i == start {
                    i += 1;
                }
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Append `s` to `out` as a JSON string literal (with quotes).
pub fn write_string(out: &mut String, s: &str) {
    out.push('"');
    let bytes = s.as_bytes();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let esc: &str = match b {
            b'"' => "\\\"",
            b'\\' => "\\\\",
            b'\n' => "\\n",
            b'\r' => "\\r",
            b'\t' => "\\t",
            0x00..=0x1f => {
                out.push_str(&s[start..i]);
                out.push_str("\\u00");
                out.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
                out.push(char::from_digit((b & 15) as u32, 16).unwrap_or('0'));
                start = i + 1;
                continue;
            }
            _ => continue,
        };
        out.push_str(&s[start..i]);
        out.push_str(esc);
        start = i + 1;
    }
    out.push_str(&s[start..]);
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walks_objects_and_arrays() {
        let src = r#" {"a": 1, "b" : [1, "two", {"x": null}, []], "c": {"d": "e\n\"q\""}, "t": true, "n": -42} "#;
        let mut s = Scanner::new(src);
        assert!(s.object().unwrap());
        assert_eq!(s.key().unwrap(), Some("a"));
        assert_eq!(s.i64().unwrap(), Some(1));
        assert_eq!(s.key().unwrap(), Some("b"));
        assert!(s.array().unwrap());
        let mut n = 0;
        while s.elem().unwrap() {
            s.skip().unwrap();
            n += 1;
        }
        assert_eq!(n, 4);
        assert_eq!(s.key().unwrap(), Some("c"));
        assert!(s.object().unwrap());
        assert_eq!(s.key().unwrap(), Some("d"));
        let v = s.str().unwrap().unwrap();
        assert!(v.has_escapes());
        assert_eq!(v.to_cow(), "e\n\"q\"");
        assert_eq!(s.key().unwrap(), None);
        assert_eq!(s.key().unwrap(), Some("t"));
        assert_eq!(s.bool().unwrap(), Some(true));
        assert_eq!(s.key().unwrap(), Some("n"));
        assert_eq!(s.i64().unwrap(), Some(-42));
        assert_eq!(s.key().unwrap(), None);
        assert!(s.at_end());
    }

    #[test]
    fn type_mismatches_skip_cleanly() {
        let mut s = Scanner::new(r#"{"a":{"deep":[1,2,{"x":"}"}]},"b":"ok"}"#);
        assert!(s.object().unwrap());
        assert_eq!(s.key().unwrap(), Some("a"));
        assert!(s.str().unwrap().is_none());
        assert_eq!(s.key().unwrap(), Some("b"));
        assert!(s.i64().unwrap().is_none());
        assert_eq!(s.key().unwrap(), None);
    }

    #[test]
    fn raw_returns_exact_slice() {
        let mut s = Scanner::new(r#"{"r": {"k":[1,2,"]"]} ,"z":0}"#);
        s.object().unwrap();
        s.key().unwrap();
        assert_eq!(s.raw().unwrap(), r#"{"k":[1,2,"]"]}"#);
        assert_eq!(s.key().unwrap(), Some("z"));
    }

    #[test]
    fn unescapes_like_serde_json() {
        let cases = [
            r#""plain""#,
            r#""tab\tnew\nline""#,
            r#""quote\" slash\/ back\\""#,
            r#""é中文""#,
            r#""emoji 😀 pair""#,
            r#""html <b> &""#,
            r#""mixed 日本語 \n ok""#,
            r#""""#,
        ];
        for c in cases {
            let want: String = serde_json::from_str(c).unwrap();
            let got = Scanner::new(c).str().unwrap().unwrap().to_cow().into_owned();
            assert_eq!(got, want, "{c}");
        }
    }

    #[test]
    fn bad_escapes_do_not_panic() {
        for c in [r#""\ud83d""#, r#""\ud83dA""#, r#""\u12""#, r#""\x""#, r#""\é""#] {
            let _ = Scanner::new(c).str().unwrap().unwrap().to_cow();
        }
        let mut out = String::new();
        unescape_into("trailing\\", &mut out);
        assert!(out.starts_with("trailing"));
    }

    #[test]
    fn malformed_input_errors_without_panicking() {
        for c in ["", "{", "[1,2", r#"{"a""#, r#"{"a":"#, r#""open"#, r#"{"a":1,"#, "}", r#"{"a" 1}"#, "{1:2}"] {
            let mut s = Scanner::new(c);
            let r: Result<()> = (|| {
                if s.object()? {
                    while let Some(_) = s.key()? {
                        s.skip()?;
                    }
                }
                Ok(())
            })();
            let _ = r;
            let mut s = Scanner::new(c);
            let _ = s.skip();
        }
    }

    #[test]
    fn pretty_matches_structure() {
        let src = r#"{"a":1,"b":[true,null,"x,y:{z}"],"c":{},"d":[]}"#;
        let p = pretty(src, 2);
        let a: serde_json::Value = serde_json::from_str(src).unwrap();
        let b: serde_json::Value = serde_json::from_str(&p).unwrap();
        assert_eq!(a, b);
        assert!(p.contains("\n  \"a\": 1,"));
        assert!(p.contains("\"c\": {}"));
    }

    #[test]
    fn write_string_round_trips() {
        for s in ["plain", "q\"b\\s", "nl\n\ttab\r", "ctl\u{1}\u{1f}", "日本語 😀"] {
            let mut out = String::new();
            write_string(&mut out, s);
            let back: String = serde_json::from_str(&out).unwrap();
            assert_eq!(back, s);
        }
    }
}
