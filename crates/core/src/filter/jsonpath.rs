//! JSON-path predicates: the escape hatch for anything not pre-extracted.
//!
//! `record.embed.images.*.alt:cat`, `record.reply.parent.uri~…`,
//! `json.payload.rev=…`, `exists:record.via`.
//!
//! The path is walked directly over the stored raw JSON with the pull
//! scanner — no value tree, no allocation unless a matched string contains
//! escapes. A predicate is true if ANY value the path selects passes.

use super::node::{NumOp, StrMatcher};
use super::parse::{err, FilterError};
use crate::event::Event;
use crate::json::{Result as JsonResult, Scanner, ValueKind};
use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JsonRoot {
    /// The record object (portable across sources).
    Record,
    /// The whole frame as received.
    Frame,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Seg {
    Key(Box<str>),
    Index(usize),
    /// Every array element / object value.
    Wild,
}

pub(crate) enum JsonTest {
    Exists,
    /// Applied to string contents, or to the literal token of a scalar.
    Str(StrMatcher),
    Num(NumOp, f64),
}

pub(crate) struct JsonPred {
    pub root: JsonRoot,
    pub path: Box<[Seg]>,
    pub test: JsonTest,
}

/// Parse `a.b[0].c.*.d` (without the root prefix).
pub(crate) fn parse_path(path: &str, span: Range<usize>) -> Result<Box<[Seg]>, FilterError> {
    let mut segs = Vec::new();
    if path.is_empty() {
        return Ok(segs.into_boxed_slice());
    }
    for part in path.split('.') {
        let (name, mut brackets) = match part.find('[') {
            Some(i) => (&part[..i], &part[i..]),
            None => (part, ""),
        };
        match name {
            "" if !brackets.is_empty() => {}
            "" => return err("empty path segment", span),
            "*" => segs.push(Seg::Wild),
            n => match n.parse::<usize>() {
                Ok(i) => segs.push(Seg::Index(i)),
                Err(_) => segs.push(Seg::Key(n.into())),
            },
        }
        while !brackets.is_empty() {
            let Some(close) = brackets.find(']') else {
                return err("unclosed '[' in path", span);
            };
            match &brackets[1..close] {
                "*" | "" => segs.push(Seg::Wild),
                n => match n.parse::<usize>() {
                    Ok(i) => segs.push(Seg::Index(i)),
                    Err(_) => return err(format!("bad index '{n}' in path"), span),
                },
            }
            brackets = &brackets[close + 1..];
            if !brackets.is_empty() && !brackets.starts_with('[') {
                return err("unexpected text after ']' in path", span);
            }
        }
    }
    if segs.len() > 32 {
        return err("path too long", span);
    }
    Ok(segs.into_boxed_slice())
}

impl JsonPred {
    pub(crate) fn eval(&self, e: &Event) -> bool {
        let src = match self.root {
            JsonRoot::Record => match e.record_json() {
                Some(r) => r,
                None => return false,
            },
            JsonRoot::Frame => e.raw(),
        };
        let mut s = Scanner::new(src);
        walk(&mut s, &self.path, &self.test).unwrap_or(false)
    }
}

/// Returns `Ok(true)` as soon as a selected value passes (the scanner is then
/// abandoned mid-value); `Ok(false)` only after fully consuming the value.
fn walk(s: &mut Scanner, path: &[Seg], test: &JsonTest) -> JsonResult<bool> {
    let Some((seg, rest)) = path.split_first() else {
        return leaf(s, test);
    };
    match seg {
        Seg::Key(k) => {
            if !s.object()? {
                return Ok(false);
            }
            let mut seen = false;
            while let Some(key) = s.key()? {
                if !seen && key == &**k {
                    seen = true;
                    if walk(s, rest, test)? {
                        return Ok(true);
                    }
                } else {
                    s.skip()?;
                }
            }
            Ok(false)
        }
        Seg::Index(i) => {
            match s.kind()? {
                ValueKind::Array => {
                    s.array()?;
                    let mut idx = 0;
                    while s.elem()? {
                        if idx == *i {
                            if walk(s, rest, test)? {
                                return Ok(true);
                            }
                        } else {
                            s.skip()?;
                        }
                        idx += 1;
                    }
                }
                // A numeric segment also addresses an object key ("0").
                ValueKind::Object => {
                    s.object()?;
                    let mut buf = [0u8; 20];
                    let want = fmt_usize(*i, &mut buf);
                    while let Some(key) = s.key()? {
                        if key == want {
                            if walk(s, rest, test)? {
                                return Ok(true);
                            }
                        } else {
                            s.skip()?;
                        }
                    }
                }
                _ => s.skip()?,
            }
            Ok(false)
        }
        Seg::Wild => {
            match s.kind()? {
                ValueKind::Array => {
                    s.array()?;
                    while s.elem()? {
                        if walk(s, rest, test)? {
                            return Ok(true);
                        }
                    }
                }
                ValueKind::Object => {
                    s.object()?;
                    while s.key()?.is_some() {
                        if walk(s, rest, test)? {
                            return Ok(true);
                        }
                    }
                }
                _ => s.skip()?,
            }
            Ok(false)
        }
    }
}

fn fmt_usize(mut n: usize, buf: &mut [u8; 20]) -> &str {
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    std::str::from_utf8(&buf[i..]).unwrap_or("0")
}

fn leaf(s: &mut Scanner, test: &JsonTest) -> JsonResult<bool> {
    let kind = s.kind()?;
    match test {
        JsonTest::Exists => {
            s.skip()?;
            Ok(kind != ValueKind::Null)
        }
        JsonTest::Str(m) => match kind {
            ValueKind::String => Ok(s.str()?.is_some_and(|v| m.is_match(&v.to_cow()))),
            ValueKind::Number | ValueKind::Bool | ValueKind::Null => Ok(m.is_match(s.raw()?)),
            ValueKind::Object | ValueKind::Array => {
                s.skip()?;
                Ok(false)
            }
        },
        JsonTest::Num(op, want) => match kind {
            ValueKind::Number => Ok(s.f64()?.is_some_and(|v| op.test(v, *want))),
            _ => {
                s.skip()?;
                Ok(false)
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segs(p: &str) -> Vec<Seg> {
        parse_path(p, 0..0).unwrap().into_vec()
    }

    fn key(k: &str) -> Seg {
        Seg::Key(k.into())
    }

    #[test]
    fn parses_paths() {
        assert_eq!(segs("a.b"), [key("a"), key("b")]);
        assert_eq!(segs("a[0].b[*]"), [key("a"), Seg::Index(0), key("b"), Seg::Wild]);
        assert_eq!(segs("a.*.b.2"), [key("a"), Seg::Wild, key("b"), Seg::Index(2)]);
        assert_eq!(segs("embed.$type"), [key("embed"), key("$type")]);
        assert_eq!(segs("a[0][1]"), [key("a"), Seg::Index(0), Seg::Index(1)]);
        assert_eq!(segs(""), []);
        for bad in ["a..b", "a[", "a[x]", "a[0]b", "."] {
            assert!(parse_path(bad, 0..0).is_err(), "{bad}");
        }
    }

    fn run(json: &str, path: &str, test: JsonTest) -> bool {
        let path = parse_path(path, 0..0).unwrap();
        let mut s = Scanner::new(json);
        walk(&mut s, &path, &test).unwrap()
    }

    fn eq(v: &str) -> JsonTest {
        JsonTest::Str(StrMatcher::Eq(v.into()))
    }

    const DOC: &str = r#"{"a":{"b":[{"c":"x"},{"c":"y\n"},{"d":3}],"n":42,"t":true,"z":null,"0":"zero"},"s":"top","arr":[[1,2],[3,4]]}"#;

    #[test]
    fn selects_and_tests() {
        assert!(run(DOC, "s", eq("top")));
        assert!(!run(DOC, "s", eq("nope")));
        assert!(run(DOC, "a.b[0].c", eq("x")));
        assert!(run(DOC, "a.b.1.c", eq("y\n")));
        assert!(run(DOC, "a.b.*.c", eq("y\n")));
        assert!(run(DOC, "a.b[*].c", eq("x")));
        assert!(!run(DOC, "a.b[*].c", eq("z")));
        assert!(!run(DOC, "a.b[9].c", eq("x")));
        assert!(run(DOC, "a.n", eq("42")));
        assert!(run(DOC, "a.t", eq("true")));
        assert!(run(DOC, "a.z", eq("null")));
        assert!(run(DOC, "a.0", eq("zero")));
        assert!(run(DOC, "a.n", JsonTest::Num(NumOp::Gt, 41.0)));
        assert!(!run(DOC, "a.n", JsonTest::Num(NumOp::Lt, 41.0)));
        assert!(run(DOC, "a.b.*.d", JsonTest::Num(NumOp::Eq, 3.0)));
        assert!(run(DOC, "arr.*.*", JsonTest::Num(NumOp::Eq, 4.0)));
        assert!(run(DOC, "arr[1][0]", JsonTest::Num(NumOp::Eq, 3.0)));
        assert!(run(DOC, "*.n", JsonTest::Num(NumOp::Eq, 42.0)));
    }

    #[test]
    fn exists_semantics() {
        assert!(run(DOC, "a.b", JsonTest::Exists));
        assert!(run(DOC, "a.b.*.d", JsonTest::Exists));
        assert!(!run(DOC, "a.missing", JsonTest::Exists));
        assert!(!run(DOC, "a.z", JsonTest::Exists));
        assert!(!run(DOC, "s.deeper", JsonTest::Exists));
        assert!(run(DOC, "", JsonTest::Exists));
    }

    #[test]
    fn containers_do_not_match_string_tests() {
        assert!(!run(DOC, "a", eq("x")));
        assert!(!run(DOC, "a.b", eq("x")));
    }
}
