//! Query language → AST.
//!
//! ```text
//! expr    := and ( ("OR" | "||" | "|") and )*
//! and     := unary ( ("AND" | "&&")? unary )*        juxtaposition is AND
//! unary   := ("NOT" | "-" | "!") unary | primary
//! primary := "(" expr ")" | term
//! term    := field op values | "phrase" | /regex/flags | #tag | word
//! op      := ":" | "=" | "==" | "!=" | "~" | "!~" | ">" | ">=" | "<" | "<="
//! values  := value ("," value)*
//! value   := "quoted" | /regex/flags | bare
//! ```

use std::ops::Range;

#[derive(Debug, Clone, PartialEq)]
pub struct FilterError {
    pub message: String,
    /// Byte range in the query the error refers to.
    pub span: Range<usize>,
}

impl std::fmt::Display for FilterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (at {})", self.message, self.span.start)
    }
}
impl std::error::Error for FilterError {}

pub(crate) fn err<T>(message: impl Into<String>, span: Range<usize>) -> Result<T, FilterError> {
    Err(FilterError { message: message.into(), span })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    /// `:` — the field's natural match (contains / equals, ignoring case).
    Match,
    /// `=` — exact, case-sensitive.
    Eq,
    /// `~` — regular expression.
    Regex,
    Gt,
    Ge,
    Lt,
    Le,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bare(String),
    Quoted(String),
    Regex { pattern: String, flags: String },
}

impl Value {
    pub fn text(&self) -> &str {
        match self {
            Value::Bare(s) | Value::Quoted(s) => s,
            Value::Regex { pattern, .. } => pattern,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Term {
    pub field: String,
    pub op: CmpOp,
    /// `!=` / `!~`.
    pub negated: bool,
    pub values: Vec<Value>,
    pub span: Range<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    All,
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    Term(Term),
}

const MAX_DEPTH: u32 = 64;

pub fn parse(src: &str) -> Result<Expr, FilterError> {
    let mut p = Parser { src, pos: 0, depth: 0 };
    p.ws();
    if p.at_end() {
        return Ok(Expr::All);
    }
    let e = p.parse_or()?;
    p.ws();
    if !p.at_end() {
        return err("unexpected ')'", p.pos..p.pos + 1);
    }
    Ok(e)
}

struct Parser<'a> {
    src: &'a str,
    pos: usize,
    depth: u32,
}

fn is_field_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '$' | '[' | ']' | '*' | '-')
}

impl<'a> Parser<'a> {
    fn rest(&self) -> &'a str {
        &self.src[self.pos..]
    }

    fn at_end(&self) -> bool {
        self.pos >= self.src.len()
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn ws(&mut self) {
        let trimmed = self.rest().trim_start();
        self.pos = self.src.len() - trimmed.len();
    }

    fn eat(&mut self, s: &str) -> bool {
        if self.rest().starts_with(s) {
            self.pos += s.len();
            true
        } else {
            false
        }
    }

    /// Consume an uppercase keyword that stands alone as a word.
    fn eat_kw(&mut self, kw: &str) -> bool {
        let rest = self.rest();
        if rest.starts_with(kw) {
            let after = rest[kw.len()..].chars().next();
            if after.is_none_or(|c| c.is_whitespace() || c == '(' || c == ')') {
                self.pos += kw.len();
                return true;
            }
        }
        false
    }

    fn at_or(&self) -> bool {
        let rest = self.rest();
        rest.starts_with('|')
            || (rest.starts_with("OR") && rest[2..].chars().next().is_none_or(|c| c.is_whitespace() || c == '(' || c == ')'))
    }

    fn parse_or(&mut self) -> Result<Expr, FilterError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return err("expression nested too deeply", self.pos..self.pos);
        }
        let mut items = vec![self.parse_and()?];
        loop {
            self.ws();
            if self.eat_kw("OR") || self.eat("||") || self.eat("|") {
                items.push(self.parse_and()?);
            } else {
                break;
            }
        }
        self.depth -= 1;
        Ok(if items.len() == 1 { items.pop().unwrap() } else { Expr::Or(items) })
    }

    fn parse_and(&mut self) -> Result<Expr, FilterError> {
        let mut items = Vec::new();
        loop {
            self.ws();
            if self.at_end() || self.peek() == Some(')') || self.at_or() {
                break;
            }
            if self.eat_kw("AND") || self.eat("&&") {
                continue;
            }
            items.push(self.parse_unary()?);
        }
        match items.len() {
            0 => err("expected a term", self.pos..(self.pos + 1).min(self.src.len())),
            1 => Ok(items.pop().unwrap()),
            _ => Ok(Expr::And(items)),
        }
    }

    fn parse_unary(&mut self) -> Result<Expr, FilterError> {
        self.ws();
        if self.eat_kw("NOT") {
            return Ok(Expr::Not(Box::new(self.parse_unary()?)));
        }
        if let Some(c @ ('-' | '!')) = self.peek() {
            let next = self.rest()[c.len_utf8()..].chars().next();
            if next.is_some_and(|n| !n.is_whitespace() && n != '=' && n != '~') {
                self.pos += 1;
                return Ok(Expr::Not(Box::new(self.parse_unary()?)));
            }
        }
        if self.peek() == Some('(') {
            let open = self.pos;
            self.pos += 1;
            let e = self.parse_or()?;
            self.ws();
            if !self.eat(")") {
                return err("unclosed '('", open..open + 1);
            }
            return Ok(e);
        }
        self.parse_term().map(Expr::Term)
    }

    fn parse_term(&mut self) -> Result<Term, FilterError> {
        let start = self.pos;
        let text_term = |values: Vec<Value>, op: CmpOp, span: Range<usize>| Term {
            field: "text".into(),
            op,
            negated: false,
            values,
            span,
        };
        match self.peek() {
            Some('"') => {
                let v = self.parse_quoted()?;
                return Ok(text_term(vec![v], CmpOp::Match, start..self.pos));
            }
            Some('/') => {
                let v = self.parse_regex()?;
                return Ok(text_term(vec![v], CmpOp::Match, start..self.pos));
            }
            Some('#') => {
                self.pos += 1;
                let v = self.parse_bare(false);
                if v.is_empty() {
                    return err("expected a tag after '#'", start..self.pos);
                }
                return Ok(Term {
                    field: "tag".into(),
                    op: CmpOp::Match,
                    negated: false,
                    values: vec![Value::Bare(v)],
                    span: start..self.pos,
                });
            }
            _ => {}
        }

        // Try `field op values`.
        let field_len = self.rest().find(|c: char| !is_field_char(c)).unwrap_or(self.rest().len());
        if field_len > 0 {
            let field = &self.rest()[..field_len];
            let after = &self.rest()[field_len..];
            let op = [
                ("!=", CmpOp::Eq, true),
                ("!~", CmpOp::Regex, true),
                ("==", CmpOp::Eq, false),
                (">=", CmpOp::Ge, false),
                ("<=", CmpOp::Le, false),
                (":", CmpOp::Match, false),
                ("=", CmpOp::Eq, false),
                ("~", CmpOp::Regex, false),
                (">", CmpOp::Gt, false),
                ("<", CmpOp::Lt, false),
            ]
            .into_iter()
            .find(|(tok, _, _)| after.starts_with(tok));
            if let Some((tok, op, negated)) = op {
                let value_start = &after[tok.len()..];
                // `https://…` and friends are words, not a field named "https".
                let is_url = op == CmpOp::Match && value_start.starts_with("//");
                if !is_url {
                    self.pos += field_len + tok.len();
                    let values = self.parse_values()?;
                    return Ok(Term { field: field.to_ascii_lowercase(), op, negated, values, span: start..self.pos });
                }
            }
        }

        let word = self.parse_bare(false);
        if word.is_empty() {
            return err("expected a term", start..(start + 1).min(self.src.len()));
        }
        Ok(text_term(vec![Value::Bare(word)], CmpOp::Match, start..self.pos))
    }

    fn parse_values(&mut self) -> Result<Vec<Value>, FilterError> {
        let mut values = Vec::new();
        loop {
            let start = self.pos;
            let v = match self.peek() {
                Some('"') => self.parse_quoted()?,
                Some('/') if self.looks_like_regex() => self.parse_regex()?,
                _ => Value::Bare(self.parse_bare(true)),
            };
            if matches!(&v, Value::Bare(s) if s.is_empty()) {
                return err("expected a value", start..(start + 1).min(self.src.len()));
            }
            values.push(v);
            if !self.eat(",") {
                break;
            }
        }
        Ok(values)
    }

    /// A `/` starts a regex literal only if a closing `/` exists before the
    /// value would otherwise end.
    fn looks_like_regex(&self) -> bool {
        let rest = &self.rest()[1..];
        let mut prev = '\0';
        for c in rest.chars() {
            if c == '/' && prev != '\\' {
                return true;
            }
            prev = c;
        }
        false
    }

    fn parse_bare(&mut self, stop_at_comma: bool) -> String {
        let rest = self.rest();
        let end = rest
            .find(|c: char| c.is_whitespace() || c == ')' || (stop_at_comma && c == ','))
            .unwrap_or(rest.len());
        self.pos += end;
        rest[..end].to_owned()
    }

    fn parse_quoted(&mut self) -> Result<Value, FilterError> {
        let start = self.pos;
        self.pos += 1;
        let mut out = String::new();
        let mut chars = self.rest().char_indices();
        while let Some((i, c)) = chars.next() {
            match c {
                '"' => {
                    self.pos += i + 1;
                    return Ok(Value::Quoted(out));
                }
                '\\' => match chars.next() {
                    Some((_, 'n')) => out.push('\n'),
                    Some((_, 't')) => out.push('\t'),
                    Some((_, other)) => out.push(other),
                    None => break,
                },
                _ => out.push(c),
            }
        }
        err("unclosed quote", start..self.src.len())
    }

    fn parse_regex(&mut self) -> Result<Value, FilterError> {
        let start = self.pos;
        self.pos += 1;
        let mut pattern = String::new();
        let mut chars = self.rest().char_indices();
        while let Some((i, c)) = chars.next() {
            match c {
                '/' => {
                    self.pos += i + 1;
                    let rest = self.rest();
                    let flags_len = rest.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(rest.len());
                    let flags = rest[..flags_len].to_owned();
                    self.pos += flags_len;
                    return Ok(Value::Regex { pattern, flags });
                }
                '\\' => match chars.next() {
                    Some((_, '/')) => pattern.push('/'),
                    Some((_, other)) => {
                        pattern.push('\\');
                        pattern.push(other);
                    }
                    None => break,
                },
                _ => pattern.push(c),
            }
        }
        err("unclosed regex", start..self.src.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(e: &Expr) -> &Term {
        match e {
            Expr::Term(t) => t,
            other => panic!("not a term: {other:?}"),
        }
    }

    fn bare(s: &str) -> Value {
        Value::Bare(s.into())
    }

    #[test]
    fn empty_is_all() {
        assert_eq!(parse("").unwrap(), Expr::All);
        assert_eq!(parse("   ").unwrap(), Expr::All);
    }

    #[test]
    fn words_phrases_and_fields() {
        let e = parse(r#"hello "big world" lang:en,ja #Rust"#).unwrap();
        let Expr::And(items) = e else { panic!() };
        assert_eq!(items.len(), 4);
        assert_eq!((term(&items[0]).field.as_str(), &term(&items[0]).values), ("text", &vec![bare("hello")]));
        assert_eq!(term(&items[1]).values, vec![Value::Quoted("big world".into())]);
        assert_eq!((term(&items[2]).field.as_str(), &term(&items[2]).values), ("lang", &vec![bare("en"), bare("ja")]));
        assert_eq!((term(&items[3]).field.as_str(), &term(&items[3]).values), ("tag", &vec![bare("Rust")]));
    }

    #[test]
    fn precedence_and_grouping() {
        let e = parse("a b OR c").unwrap();
        let Expr::Or(items) = &e else { panic!("{e:?}") };
        assert!(matches!(&items[0], Expr::And(v) if v.len() == 2));
        assert!(matches!(&items[1], Expr::Term(_)));

        let e = parse("a (b | c) && d").unwrap();
        let Expr::And(items) = &e else { panic!("{e:?}") };
        assert_eq!(items.len(), 3);
        assert!(matches!(&items[1], Expr::Or(v) if v.len() == 2));
    }

    #[test]
    fn negation_forms() {
        for q in ["-has:media", "!has:media", "NOT has:media"] {
            let e = parse(q).unwrap();
            let Expr::Not(inner) = &e else { panic!("{q}: {e:?}") };
            assert_eq!(term(inner).field, "has");
        }
        let t = parse("lang!=en").unwrap();
        assert!(term(&t).negated);
        let e = parse("-(a OR b)").unwrap();
        assert!(matches!(e, Expr::Not(b) if matches!(*b, Expr::Or(_))));
        // A lone dash is just a word.
        assert_eq!(term(&parse("-").unwrap()).values, vec![bare("-")]);
    }

    #[test]
    fn operators() {
        for (q, op, neg) in [
            ("len>100", CmpOp::Gt, false),
            ("len>=100", CmpOp::Ge, false),
            ("len<100", CmpOp::Lt, false),
            ("len<=100", CmpOp::Le, false),
            ("rkey=abc", CmpOp::Eq, false),
            ("rkey==abc", CmpOp::Eq, false),
            ("rkey!=abc", CmpOp::Eq, true),
            ("text~fo+", CmpOp::Regex, false),
            ("text!~fo+", CmpOp::Regex, true),
            ("text:foo", CmpOp::Match, false),
        ] {
            let e = parse(q).unwrap();
            let t = term(&e);
            assert_eq!((t.op, t.negated), (op, neg), "{q}");
        }
        assert_eq!(term(&parse("len:>100").unwrap()).values, vec![bare(">100")]);
    }

    #[test]
    fn regex_literals_and_quotes() {
        let e = parse(r#"text:/a\/b\d+/i name:"say \"hi\"" /^x/"#).unwrap();
        let Expr::And(items) = e else { panic!() };
        assert_eq!(term(&items[0]).values, vec![Value::Regex { pattern: r"a/b\d+".into(), flags: "i".into() }]);
        assert_eq!(term(&items[1]).values, vec![Value::Quoted("say \"hi\"".into())]);
        assert_eq!(term(&items[2]).field, "text");
        // A path-like value with a single slash is not a regex.
        assert_eq!(term(&parse("link:/about").unwrap()).values, vec![bare("/about")]);
    }

    #[test]
    fn dids_urls_and_paths() {
        let e = parse("did:did:plc:abc").unwrap();
        assert_eq!((term(&e).field.as_str(), &term(&e).values), ("did", &vec![bare("did:plc:abc")]));
        let e = parse("https://example.com/x").unwrap();
        assert_eq!((term(&e).field.as_str(), &term(&e).values), ("text", &vec![bare("https://example.com/x")]));
        let e = parse("record.embed.$type=app.bsky.embed.images").unwrap();
        assert_eq!(term(&e).field, "record.embed.$type");
        let e = parse("record.facets[0].features[*].tag:x").unwrap();
        assert_eq!(term(&e).field, "record.facets[0].features[*].tag");
    }

    #[test]
    fn errors_carry_spans() {
        for (q, at) in [("(a", 0), ("a)", 1), ("text:", 5), ("\"open", 0), ("a OR", 4), ("()", 1), ("lang:en,", 8)] {
            let e = parse(q).unwrap_err();
            assert_eq!(e.span.start, at, "{q}: {e:?}");
        }
        let deep = "(".repeat(200);
        assert!(parse(&deep).is_err());
    }

    #[test]
    fn keywords_need_word_boundaries() {
        // "ORANGE" and "ANDROID" are words, not operators.
        let e = parse("ORANGE ANDROID NOTE").unwrap();
        assert!(matches!(&e, Expr::And(v) if v.len() == 3), "{e:?}");
    }
}
