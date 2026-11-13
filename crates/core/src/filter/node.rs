//! Compiled filter tree: evaluation, cost model and optimizer.
//!
//! Evaluation is a short-circuiting walk over a small tree. What makes it
//! fast is what happens before it runs:
//!
//! * every predicate reads a field pre-extracted at decode time (bit tests,
//!   pointer compares, slices of the event blob) — nothing is re-parsed
//!   unless a JSON-path predicate explicitly asks;
//! * the optimizer merges sibling predicates into set/automaton lookups
//!   (`a OR b OR c` on text becomes one Aho-Corasick pass, DID lists become
//!   one hash probe, kind/flag tests collapse into single mask checks);
//! * children are ordered cheapest-first, so the expensive scans only run on
//!   events that survived the cheap tests.

use super::jsonpath::JsonPred;
use crate::did::Did;
use crate::event::{url_host, Event, Field, Flags, Kind};
use crate::nsid::Nsid;
use aho_corasick::AhoCorasick;
use memchr::memmem;
use rustc_hash::FxHashSet;

pub(crate) const ALL_KINDS: u8 = 0b1111;
pub(crate) const ALL_OPS: u8 = 0b1111;

pub(crate) enum StrMatcher {
    /// Field is non-empty.
    Any,
    /// Exact, case-sensitive.
    Eq(Box<str>),
    /// Exact, ignoring case (needle stored lowercased).
    EqCi(Box<str>),
    /// Exact any-of, case-sensitive.
    Set(FxHashSet<Box<str>>),
    /// Case-sensitive substring.
    Contains(memmem::Finder<'static>),
    /// Any of several substrings, ASCII case-insensitive, in one pass. A
    /// single needle skips the automaton for a memchr-driven scan.
    Ac { ac: Option<AhoCorasick>, single: Option<CiFinder>, pats: Vec<Box<str>> },
    Regex(regex::Regex),
}

impl StrMatcher {
    pub(crate) fn ac(pats: Vec<Box<str>>) -> StrMatcher {
        if let [one] = pats.as_slice() {
            return StrMatcher::Ac { ac: None, single: Some(CiFinder::new(one)), pats };
        }
        let ac = AhoCorasick::builder()
            .ascii_case_insensitive(true)
            .build(pats.iter().map(|p| p.as_bytes()))
            .expect("literal patterns always build");
        StrMatcher::Ac { ac: Some(ac), single: None, pats }
    }

    #[inline]
    pub(crate) fn is_match(&self, s: &str) -> bool {
        match self {
            StrMatcher::Any => !s.is_empty(),
            StrMatcher::Eq(v) => **v == *s,
            StrMatcher::EqCi(v) => eq_ci(s, v),
            StrMatcher::Set(set) => set.contains(s),
            StrMatcher::Contains(f) => f.find(s.as_bytes()).is_some(),
            StrMatcher::Ac { ac, single, .. } => {
                // Most events have no text at all; skip the searcher setup.
                if s.is_empty() {
                    return false;
                }
                match (single, ac) {
                    (Some(f), _) => f.is_match(s.as_bytes()),
                    (None, Some(ac)) => ac.is_match(s),
                    (None, None) => false,
                }
            }
            StrMatcher::Regex(re) => re.is_match(s),
        }
    }

    fn cost(&self) -> u32 {
        match self {
            StrMatcher::Any => 1,
            StrMatcher::Eq(_) | StrMatcher::EqCi(_) => 3,
            StrMatcher::Set(_) => 6,
            StrMatcher::Contains(_) => 12,
            StrMatcher::Ac { .. } => 16,
            StrMatcher::Regex(_) => 60,
        }
    }
}

/// ASCII case-insensitive search for one needle: memchr finds candidates for
/// the first byte (in either case), then the remainder is compared in place.
pub(crate) struct CiFinder {
    needle: Box<[u8]>,
    first: (u8, u8),
}

impl CiFinder {
    pub(crate) fn new(needle: &str) -> CiFinder {
        let needle: Box<[u8]> = needle.as_bytes().into();
        let b = needle.first().copied().unwrap_or(0);
        CiFinder { first: (b.to_ascii_lowercase(), b.to_ascii_uppercase()), needle }
    }

    #[inline]
    pub(crate) fn is_match(&self, hay: &[u8]) -> bool {
        let n = self.needle.len();
        if n == 0 {
            return true;
        }
        if hay.len() < n {
            return false;
        }
        // Only positions where the whole needle still fits can start a match.
        let last_start = hay.len() - n;
        let mut from = 0;
        while from <= last_start {
            let window = &hay[from..=last_start];
            let hit = if self.first.0 == self.first.1 {
                memchr::memchr(self.first.0, window)
            } else {
                memchr::memchr2(self.first.0, self.first.1, window)
            };
            let Some(i) = hit else { return false };
            let at = from + i;
            if hay[at + 1..at + n].eq_ignore_ascii_case(&self.needle[1..]) {
                return true;
            }
            from = at + 1;
        }
        false
    }
}

/// Compare `s` to an already-lowercased needle, ignoring case.
#[inline]
fn eq_ci(s: &str, lower: &str) -> bool {
    if s.is_ascii() && lower.is_ascii() {
        s.eq_ignore_ascii_case(lower)
    } else {
        s.chars().flat_map(char::to_lowercase).eq(lower.chars())
    }
}

#[derive(Default)]
pub(crate) struct CollectionSet {
    pub exact: Vec<Nsid>,
    /// Prefixes including the trailing dot (`app.bsky.feed.`).
    pub prefixes: Vec<Box<str>>,
    /// Any other glob shape.
    pub regexes: Vec<regex::Regex>,
    /// Source patterns, for pushdown and display.
    pub patterns: Vec<Box<str>>,
}

impl CollectionSet {
    #[inline]
    fn matches(&self, c: Option<Nsid>) -> bool {
        let Some(c) = c else { return false };
        if self.exact.contains(&c) {
            return true;
        }
        if self.prefixes.is_empty() && self.regexes.is_empty() {
            return false;
        }
        let name = c.as_str();
        self.prefixes.iter().any(|p| name.starts_with(&**p)) || self.regexes.iter().any(|r| r.is_match(name))
    }

    fn merge(&mut self, other: CollectionSet) {
        for n in other.exact {
            if !self.exact.contains(&n) {
                self.exact.push(n);
            }
        }
        self.prefixes.extend(other.prefixes);
        self.regexes.extend(other.regexes);
        self.patterns.extend(other.patterns);
    }
}

pub(crate) enum DidSet {
    One(Did),
    Many(FxHashSet<Did>),
}

impl DidSet {
    #[inline]
    fn contains(&self, d: &Did) -> bool {
        match self {
            DidSet::One(x) => x == d,
            DidSet::Many(s) => s.contains(d),
        }
    }

    pub(crate) fn iter(&self) -> Box<dyn Iterator<Item = &Did> + '_> {
        match self {
            DidSet::One(d) => Box::new(std::iter::once(d)),
            DidSet::Many(s) => Box::new(s.iter()),
        }
    }

    fn merge(self, other: DidSet) -> DidSet {
        let mut set: FxHashSet<Did> = match self {
            DidSet::One(d) => std::iter::once(d).collect(),
            DidSet::Many(s) => s,
        };
        match other {
            DidSet::One(d) => {
                set.insert(d);
            }
            DidSet::Many(s) => set.extend(s),
        }
        DidSet::Many(set)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NumField {
    TextChars,
    RawBytes,
    Images,
    Langs,
    Tags,
    Mentions,
    Links,
    Seq,
    TimeUs,
    CreatedUs,
    /// |createdAt − event time|; only defined when createdAt is present.
    SkewUs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NumOp {
    Eq,
    Lt,
    Le,
    Gt,
    Ge,
}

impl NumOp {
    #[inline]
    pub(crate) fn test<T: PartialOrd>(self, a: T, b: T) -> bool {
        match self {
            NumOp::Eq => a == b,
            NumOp::Lt => a < b,
            NumOp::Le => a <= b,
            NumOp::Gt => a > b,
            NumOp::Ge => a >= b,
        }
    }
}

pub(crate) enum Node {
    True,
    False,
    And(Box<[Node]>),
    Or(Box<[Node]>),
    Not(Box<Node>),
    /// Bitmask over [`Kind::mask`].
    Kind(u8),
    /// Bitmask over [`crate::event::Op::mask`].
    Op(u8),
    FlagsAll(Flags),
    FlagsAny(Flags),
    Collection(CollectionSet),
    Did(DidSet),
    /// Match on the DID's string form (globs, regexes).
    DidStr(StrMatcher),
    Str(Field, StrMatcher),
    /// Any item of a list field matches.
    List(Field, StrMatcher),
    /// Language tags, lowercased; `en` also matches `en-US`.
    Lang(Box<[Box<str>]>),
    /// Link host equals or is a subdomain of one of these (lowercased).
    Domain(Box<[Box<str>]>),
    /// Match on the computed `at://` URI.
    Uri(StrMatcher),
    Num(NumField, NumOp, i64),
    Json(JsonPred),
    /// Deterministic sampling: keep events whose hash falls under the threshold.
    Sample(u64),
}

#[inline]
fn mix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

fn lang_matches(item: &str, want: &str) -> bool {
    let (i, w) = (item.as_bytes(), want.as_bytes());
    if i.len() == w.len() {
        return i.eq_ignore_ascii_case(w);
    }
    i.len() > w.len() && i[w.len()] == b'-' && i[..w.len()].eq_ignore_ascii_case(w)
}

fn domain_matches(host: &str, want: &str) -> bool {
    let (h, w) = (host.as_bytes(), want.as_bytes());
    if h.len() == w.len() {
        return h.eq_ignore_ascii_case(w);
    }
    h.len() > w.len() && h[h.len() - w.len() - 1] == b'.' && h[h.len() - w.len()..].eq_ignore_ascii_case(w)
}

impl Node {
    pub(crate) fn eval(&self, e: &Event) -> bool {
        match self {
            Node::True => true,
            Node::False => false,
            Node::And(ns) => ns.iter().all(|n| n.eval(e)),
            Node::Or(ns) => ns.iter().any(|n| n.eval(e)),
            Node::Not(n) => !n.eval(e),
            Node::Kind(m) => m & e.kind.mask() != 0,
            Node::Op(m) => m & e.op.mask() != 0,
            Node::FlagsAll(f) => e.flags.contains(*f),
            Node::FlagsAny(f) => e.flags.intersects(*f),
            Node::Collection(set) => set.matches(e.collection),
            Node::Did(set) => set.contains(&e.did),
            Node::DidStr(m) => match &e.did {
                Did::Other(s) => m.is_match(s),
                did => m.is_match(&did.to_string()),
            },
            Node::Str(f, m) => m.is_match(e.field(*f)),
            Node::List(f, m) => e.list(*f).any(|item| m.is_match(item)),
            Node::Lang(wants) => e.list(Field::Langs).any(|item| wants.iter().any(|w| lang_matches(item, w))),
            Node::Domain(wants) => e.list(Field::Links).any(|url| {
                let host = url_host(url);
                wants.iter().any(|w| domain_matches(host, w))
            }),
            Node::Uri(m) => e.at_uri().is_some_and(|u| m.is_match(&u)),
            Node::Num(field, op, v) => {
                let actual = match field {
                    NumField::TextChars => e.text_chars as i64,
                    NumField::RawBytes => e.raw().len() as i64,
                    NumField::Images => e.n_images as i64,
                    NumField::Langs => e.n_langs as i64,
                    NumField::Tags => e.n_tags as i64,
                    NumField::Mentions => e.n_mentions as i64,
                    NumField::Links => e.n_links as i64,
                    NumField::Seq => e.seq,
                    NumField::TimeUs => e.time_us,
                    NumField::CreatedUs => {
                        if e.created_us == 0 {
                            return false;
                        }
                        e.created_us
                    }
                    NumField::SkewUs => {
                        if e.created_us == 0 {
                            return false;
                        }
                        (e.created_us - e.time_us).abs()
                    }
                };
                op.test(actual, *v)
            }
            Node::Json(p) => p.eval(e),
            Node::Sample(threshold) => mix64(e.seq as u64 ^ (e.time_us as u64).rotate_left(32)) < *threshold,
        }
    }

    /// Rough relative cost of evaluating this node on one event.
    pub(crate) fn cost(&self) -> u32 {
        match self {
            Node::True | Node::False => 0,
            Node::And(ns) | Node::Or(ns) => ns.iter().map(Node::cost).sum(),
            Node::Not(n) => n.cost(),
            Node::Kind(_) | Node::Op(_) | Node::FlagsAll(_) | Node::FlagsAny(_) | Node::Num(..) | Node::Sample(_) => 1,
            Node::Collection(s) => 2 + 3 * s.prefixes.len() as u32 + 40 * s.regexes.len() as u32,
            Node::Did(DidSet::One(_)) => 2,
            Node::Did(DidSet::Many(_)) => 6,
            Node::DidStr(m) => 30 + m.cost(),
            Node::Str(f, m) => {
                let scale = match f {
                    // Scans over the raw frame touch far more bytes.
                    Field::Raw => 6,
                    Field::Text | Field::Alt => 2,
                    _ => 1,
                };
                m.cost() * scale
            }
            Node::List(_, m) => 4 + m.cost(),
            Node::Lang(_) => 6,
            Node::Domain(_) => 14,
            Node::Uri(m) => 40 + m.cost(),
            Node::Json(_) => 400,
        }
    }

    /// Kinds this node can possibly match, as a mask. Used both to prune and
    /// to derive server-side `kinds`.
    pub(crate) fn kinds(&self) -> u8 {
        const COMMIT: u8 = 1 << Kind::Commit as u8;
        match self {
            Node::False => 0,
            Node::Kind(m) => *m,
            Node::And(ns) => ns.iter().fold(ALL_KINDS, |m, n| m & n.kinds()),
            Node::Or(ns) => ns.iter().fold(0, |m, n| m | n.kinds()),
            // Only commits have a collection, an operation, or record fields.
            Node::Collection(_) | Node::Uri(_) | Node::Lang(_) | Node::Domain(_) => COMMIT,
            Node::Op(m) => {
                if m & 1 == 0 {
                    COMMIT
                } else {
                    ALL_KINDS
                }
            }
            Node::List(..) => COMMIT,
            Node::Str(f, m) => match (f, m) {
                (Field::Raw | Field::Aux, _) => ALL_KINDS,
                // An empty needle could match an absent field.
                (_, StrMatcher::Eq(v) | StrMatcher::EqCi(v)) if v.is_empty() => ALL_KINDS,
                (_, StrMatcher::Set(set)) if set.contains("") => ALL_KINDS,
                (_, StrMatcher::Regex(_)) => ALL_KINDS,
                _ => COMMIT,
            },
            Node::FlagsAll(f) | Node::FlagsAny(f) => {
                if f.intersects(Flags::ACTIVE | Flags::HANDLE) {
                    ALL_KINDS
                } else {
                    COMMIT
                }
            }
            Node::Num(NumField::CreatedUs | NumField::SkewUs, ..) => COMMIT,
            _ => ALL_KINDS,
        }
    }

    pub(crate) fn optimize(self) -> Node {
        match self {
            Node::And(children) => optimize_and(children.into_vec()),
            Node::Or(children) => optimize_or(children.into_vec()),
            Node::Not(inner) => match inner.optimize() {
                Node::True => Node::False,
                Node::False => Node::True,
                Node::Not(x) => *x,
                Node::Kind(m) => Node::Kind(!m & ALL_KINDS),
                other => Node::Not(Box::new(other)),
            },
            Node::Kind(0) | Node::Op(0) => Node::False,
            Node::Kind(m) if m & ALL_KINDS == ALL_KINDS => Node::True,
            other => other,
        }
    }

    /// Human-readable form of the optimized tree (for debugging and tests).
    pub(crate) fn describe(&self, out: &mut String) {
        use std::fmt::Write;
        let list = |out: &mut String, name: &str, ns: &[Node]| {
            out.push_str(name);
            out.push('(');
            for (i, n) in ns.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                n.describe(out);
            }
            out.push(')');
        };
        match self {
            Node::True => out.push_str("true"),
            Node::False => out.push_str("false"),
            Node::And(ns) => list(out, "and", ns),
            Node::Or(ns) => list(out, "or", ns),
            Node::Not(n) => {
                out.push_str("not(");
                n.describe(out);
                out.push(')');
            }
            Node::Kind(m) => {
                let _ = write!(out, "kind[{m:04b}]");
            }
            Node::Op(m) => {
                let _ = write!(out, "op[{m:04b}]");
            }
            Node::FlagsAll(f) => {
                let _ = write!(out, "all[{f:?}]");
            }
            Node::FlagsAny(f) => {
                let _ = write!(out, "any[{f:?}]");
            }
            Node::Collection(s) => {
                let _ = write!(out, "collection{:?}", s.patterns);
            }
            Node::Did(DidSet::One(d)) => {
                let _ = write!(out, "did[{d}]");
            }
            Node::Did(DidSet::Many(s)) => {
                let _ = write!(out, "did[{} dids]", s.len());
            }
            Node::DidStr(m) => {
                let _ = write!(out, "did~{}", matcher_name(m));
            }
            Node::Str(f, m) => {
                let _ = write!(out, "{f:?}~{}", matcher_name(m));
            }
            Node::List(f, m) => {
                let _ = write!(out, "{f:?}[]~{}", matcher_name(m));
            }
            Node::Lang(l) => {
                let _ = write!(out, "lang{l:?}");
            }
            Node::Domain(d) => {
                let _ = write!(out, "domain{d:?}");
            }
            Node::Uri(m) => {
                let _ = write!(out, "uri~{}", matcher_name(m));
            }
            Node::Num(f, op, v) => {
                let _ = write!(out, "{f:?}{op:?}{v}");
            }
            Node::Json(_) => out.push_str("json"),
            Node::Sample(_) => out.push_str("sample"),
        }
    }
}

fn matcher_name(m: &StrMatcher) -> String {
    match m {
        StrMatcher::Any => "any".into(),
        StrMatcher::Eq(v) => format!("eq({v})"),
        StrMatcher::EqCi(v) => format!("eqci({v})"),
        StrMatcher::Set(s) => format!("set({})", s.len()),
        StrMatcher::Contains(_) => "contains".into(),
        StrMatcher::Ac { pats, .. } => format!("ac({})", pats.len()),
        StrMatcher::Regex(r) => format!("re({})", r.as_str()),
    }
}

fn sort_by_cost(nodes: &mut [Node]) {
    nodes.sort_by_key(Node::cost);
}

fn optimize_and(children: Vec<Node>) -> Node {
    let mut flat = Vec::with_capacity(children.len());
    for c in children {
        match c.optimize() {
            Node::True => {}
            Node::False => return Node::False,
            Node::And(inner) => flat.extend(inner.into_vec()),
            other => flat.push(other),
        }
    }

    // Collapse mask-style tests into one node each.
    let mut kind = ALL_KINDS;
    let mut op = ALL_OPS;
    let mut flags = Flags::empty();
    let mut rest = Vec::with_capacity(flat.len());
    for n in flat {
        match n {
            Node::Kind(m) => kind &= m,
            Node::Op(m) => op &= m,
            Node::FlagsAll(f) => flags |= f,
            Node::FlagsAny(f) if f.bits().count_ones() == 1 => flags |= f,
            other => rest.push(other),
        }
    }
    // Every conjunct constrains which kinds can match at all.
    let reachable = rest.iter().fold(kind, |m, n| m & n.kinds());
    if reachable == 0 || op == 0 {
        return Node::False;
    }
    if kind != ALL_KINDS {
        rest.push(Node::Kind(kind));
    }
    if op != ALL_OPS {
        rest.push(Node::Op(op));
    }
    if !flags.is_empty() {
        rest.push(Node::FlagsAll(flags));
    }
    sort_by_cost(&mut rest);
    match rest.len() {
        0 => Node::True,
        1 => rest.pop().unwrap(),
        _ => Node::And(rest.into_boxed_slice()),
    }
}

fn optimize_or(children: Vec<Node>) -> Node {
    let mut flat = Vec::with_capacity(children.len());
    for c in children {
        match c.optimize() {
            Node::False => {}
            Node::True => return Node::True,
            Node::Or(inner) => flat.extend(inner.into_vec()),
            other => flat.push(other),
        }
    }

    let mut kind = 0u8;
    let mut op = 0u8;
    let mut flags = Flags::empty();
    let mut collections: Option<CollectionSet> = None;
    let mut dids: Option<DidSet> = None;
    // Per-field literal groups, merged into one automaton / hash set.
    let mut ac: Vec<(Field, bool, Vec<Box<str>>)> = Vec::new();
    let mut eq: Vec<(Field, bool, FxHashSet<Box<str>>)> = Vec::new();
    let mut langs: Vec<Box<str>> = Vec::new();
    let mut domains: Vec<Box<str>> = Vec::new();
    let mut rest = Vec::with_capacity(flat.len());

    for n in flat {
        match n {
            Node::Kind(m) => kind |= m,
            Node::Op(m) => op |= m,
            Node::FlagsAny(f) => flags |= f,
            Node::FlagsAll(f) if f.bits().count_ones() == 1 => flags |= f,
            Node::Collection(s) => match &mut collections {
                Some(acc) => acc.merge(s),
                None => collections = Some(s),
            },
            Node::Did(s) => {
                dids = Some(match dids.take() {
                    Some(acc) => acc.merge(s),
                    None => s,
                })
            }
            Node::Lang(l) => langs.extend(l.into_vec()),
            Node::Domain(d) => domains.extend(d.into_vec()),
            Node::Str(f, StrMatcher::Ac { pats, .. }) => push_group(&mut ac, f, false, pats),
            Node::List(f, StrMatcher::Ac { pats, .. }) => push_group(&mut ac, f, true, pats),
            Node::Str(f, StrMatcher::Eq(v)) => push_set(&mut eq, f, false, std::iter::once(v)),
            Node::List(f, StrMatcher::Eq(v)) => push_set(&mut eq, f, true, std::iter::once(v)),
            Node::Str(f, StrMatcher::Set(s)) => push_set(&mut eq, f, false, s.into_iter()),
            Node::List(f, StrMatcher::Set(s)) => push_set(&mut eq, f, true, s.into_iter()),
            other => rest.push(other),
        }
    }

    if kind & ALL_KINDS == ALL_KINDS {
        return Node::True;
    }
    if kind != 0 {
        rest.push(Node::Kind(kind));
    }
    if op != 0 {
        rest.push(Node::Op(op));
    }
    if !flags.is_empty() {
        rest.push(Node::FlagsAny(flags));
    }
    if let Some(s) = collections {
        rest.push(Node::Collection(s));
    }
    if let Some(s) = dids {
        rest.push(Node::Did(s));
    }
    if !langs.is_empty() {
        rest.push(Node::Lang(langs.into_boxed_slice()));
    }
    if !domains.is_empty() {
        rest.push(Node::Domain(domains.into_boxed_slice()));
    }
    for (f, is_list, pats) in ac {
        let m = StrMatcher::ac(pats);
        rest.push(if is_list { Node::List(f, m) } else { Node::Str(f, m) });
    }
    for (f, is_list, mut set) in eq {
        let m = if set.len() == 1 {
            StrMatcher::Eq(set.drain().next().unwrap())
        } else {
            StrMatcher::Set(set)
        };
        rest.push(if is_list { Node::List(f, m) } else { Node::Str(f, m) });
    }

    sort_by_cost(&mut rest);
    match rest.len() {
        0 => Node::False,
        1 => rest.pop().unwrap(),
        _ => Node::Or(rest.into_boxed_slice()),
    }
}

fn push_group(groups: &mut Vec<(Field, bool, Vec<Box<str>>)>, f: Field, is_list: bool, pats: Vec<Box<str>>) {
    match groups.iter_mut().find(|(gf, gl, _)| *gf == f && *gl == is_list) {
        Some((_, _, acc)) => acc.extend(pats),
        None => groups.push((f, is_list, pats)),
    }
}

fn push_set(
    groups: &mut Vec<(Field, bool, FxHashSet<Box<str>>)>,
    f: Field,
    is_list: bool,
    values: impl Iterator<Item = Box<str>>,
) {
    match groups.iter_mut().find(|(gf, gl, _)| *gf == f && *gl == is_list) {
        Some((_, _, acc)) => acc.extend(values),
        None => groups.push((f, is_list, values.collect())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lang_and_domain_matching() {
        assert!(lang_matches("en", "en"));
        assert!(lang_matches("en-US", "en"));
        assert!(lang_matches("EN-us", "en"));
        assert!(!lang_matches("eng", "en"));
        assert!(!lang_matches("e", "en"));
        assert!(lang_matches("pt-BR", "pt-br"));
        assert!(!lang_matches("pt", "pt-br"));

        assert!(domain_matches("example.com", "example.com"));
        assert!(domain_matches("Sub.Example.com", "example.com"));
        assert!(!domain_matches("notexample.com", "example.com"));
        assert!(!domain_matches("example.com.evil.net", "example.com"));
    }

    #[test]
    fn ci_finder_agrees_with_naive_search() {
        let hays = ["", "a", "Hello World", "hello hello HELLO", "xxhellx hellO", "日本語 Bluesky ブルースカイ", "aaaaab", "-=-=42=-"];
        let needles = ["", "a", "hello", "HELLO", "o w", "world", "b", "bluesky", "スカイ", "aab", "42", "=-", "zzz", "hello world!"];
        for h in hays {
            for n in needles {
                let naive = h.to_ascii_lowercase().contains(&n.to_ascii_lowercase());
                assert_eq!(CiFinder::new(n).is_match(h.as_bytes()), naive, "{n:?} in {h:?}");
                let m = StrMatcher::ac(vec![n.into()]);
                assert_eq!(m.is_match(h), naive && !h.is_empty(), "matcher {n:?} in {h:?}");
            }
        }
    }

    #[test]
    fn eq_ci_handles_unicode() {
        assert!(eq_ci("HeLLo", "hello"));
        assert!(eq_ci("ÉCOLE", "école"));
        assert!(!eq_ci("ecole", "école"));
    }
}
