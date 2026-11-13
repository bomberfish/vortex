//! AST → matcher tree: field resolution and per-field value compilation.

use super::jsonpath::{parse_path, JsonPred, JsonRoot, JsonTest};
use super::node::{CollectionSet, DidSet, Node, NumField, NumOp, StrMatcher};
use super::parse::{err, CmpOp, Expr, FilterError, Term, Value};
use crate::did::Did;
use crate::event::{Field, Flags, Kind, Op};
use crate::nsid::Nsid;
use crate::time::{parse_duration_us, parse_rfc3339_us};
use memchr::memmem;

/// Field reference shown in the UI's help.
pub const FIELD_HELP: &[(&str, &str)] = &[
    ("word \"a phrase\"", "text contains (case-insensitive)"),
    ("text: name: alt:", "post text / display name / image alt"),
    ("type: collection:", "post like repost follow block profile … or NSID, globs ok"),
    ("did: from:", "author DID; did:@/path/file loads a list"),
    ("kind: op:", "commit identity account sync / create update delete"),
    ("lang: tag: #tag", "language (en matches en-US) / hashtag"),
    ("mention: link: domain:", "mentioned DID / link URL / link host"),
    ("has: is:", "reply quote media image video link mention tag label alt embed gif root …"),
    ("reply-to: root: quote: subject:", "referenced AT-URI contains (a DID works)"),
    ("handle: status: active:", "identity / account event fields"),
    ("len images tags mentions links langs bytes", "counts: len>200 images:2 tags:1..3"),
    ("time created skew", "time>2026-01-01T00:00Z  skew>1h"),
    ("record.path json.path", "any JSON field: record.embed.$type:video  record.a[*].b>3"),
    ("exists: raw: uri: rkey: label: word:", "path exists / raw frame contains / at-uri / …"),
    ("sample:", "keep a fraction: sample:10%"),
    ("a b · a OR b · -a · ( )", "and · or · not · grouping"),
    (": = ~ != !~ > >= < <=", "match · exact · regex · negated · numeric;  a,b = any of"),
];

const TYPE_ALIASES: &[(&str, &str)] = &[
    ("post", "app.bsky.feed.post"),
    ("like", "app.bsky.feed.like"),
    ("repost", "app.bsky.feed.repost"),
    ("follow", "app.bsky.graph.follow"),
    ("block", "app.bsky.graph.block"),
    ("profile", "app.bsky.actor.profile"),
    ("list", "app.bsky.graph.list"),
    ("listitem", "app.bsky.graph.listitem"),
    ("listblock", "app.bsky.graph.listblock"),
    ("feedgen", "app.bsky.feed.generator"),
    ("generator", "app.bsky.feed.generator"),
    ("threadgate", "app.bsky.feed.threadgate"),
    ("postgate", "app.bsky.feed.postgate"),
    ("starterpack", "app.bsky.graph.starterpack"),
    ("labeler", "app.bsky.labeler.service"),
    ("verification", "app.bsky.graph.verification"),
    ("chat", "chat.bsky.actor.declaration"),
    ("bsky", "app.bsky.*"),
];

const KNOWN_FIELDS: &[&str] = &[
    "kind", "op", "collection", "type", "did", "from", "rkey", "text", "word", "name", "alt", "subject", "to",
    "parent", "reply-to", "root", "quote", "raw", "handle", "status", "active", "uri", "lang", "tag", "mention",
    "link", "domain", "label", "has", "is", "embed", "len", "bytes", "images", "langs", "tags", "mentions", "links",
    "seq", "time", "created", "skew", "sample", "exists", "record.", "json.",
];

/// Largest list a `@file` value may expand to.
const MAX_FILE_VALUES: usize = 100_000;

pub(crate) fn compile(expr: &Expr) -> Result<Node, FilterError> {
    Ok(match expr {
        Expr::All => Node::True,
        Expr::And(items) => Node::And(items.iter().map(compile).collect::<Result<Vec<_>, _>>()?.into_boxed_slice()),
        Expr::Or(items) => Node::Or(items.iter().map(compile).collect::<Result<Vec<_>, _>>()?.into_boxed_slice()),
        Expr::Not(inner) => Node::Not(Box::new(compile(inner)?)),
        Expr::Term(t) => {
            let expanded;
            let t = if t.values.iter().any(is_file_ref) {
                expanded = expand_files(t)?;
                &expanded
            } else {
                t
            };
            let node = compile_term(t)?;
            if t.negated {
                Node::Not(Box::new(node))
            } else {
                node
            }
        }
    })
}

fn is_file_ref(v: &Value) -> bool {
    matches!(v, Value::Bare(s) if s.starts_with("@/") || s.starts_with("@~/") || s.starts_with("@./"))
}

/// Replace `@/path` values with the file's non-empty, non-comment lines.
fn expand_files(t: &Term) -> Result<Term, FilterError> {
    let mut values = Vec::new();
    for v in &t.values {
        if !is_file_ref(v) {
            values.push(v.clone());
            continue;
        }
        let raw = &v.text()[1..];
        let path = match raw.strip_prefix("~/") {
            Some(rest) => std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(rest)).unwrap_or_else(|| raw.into()),
            None => raw.into(),
        };
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => return err(format!("cannot read {}: {e}", path.display()), t.span.clone()),
        };
        for line in content.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            values.push(Value::Quoted(line.to_owned()));
            if values.len() > MAX_FILE_VALUES {
                return err(format!("{} has more than {MAX_FILE_VALUES} entries", path.display()), t.span.clone());
            }
        }
    }
    if values.is_empty() {
        return err("value list is empty", t.span.clone());
    }
    Ok(Term { values, ..t.clone() })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Substring, ignoring case.
    Contains,
    /// Substring, case-sensitive: for identifier-like fields (AT-URIs, DIDs)
    /// where case is significant and a plain memmem is fastest.
    ContainsCs,
    Exact,
}

fn any_of(t: &Term, mut f: impl FnMut(&Value) -> Result<Node, FilterError>) -> Result<Node, FilterError> {
    let mut nodes = t.values.iter().map(&mut f).collect::<Result<Vec<_>, _>>()?;
    Ok(if nodes.len() == 1 { nodes.pop().unwrap() } else { Node::Or(nodes.into_boxed_slice()) })
}

fn text_only(t: &Term) -> Result<(), FilterError> {
    match t.op {
        CmpOp::Gt | CmpOp::Ge | CmpOp::Lt | CmpOp::Le => {
            err(format!("'{}' is not numeric; use : = or ~", t.field), t.span.clone())
        }
        _ => Ok(()),
    }
}

fn exact_only(t: &Term) -> Result<(), FilterError> {
    match t.op {
        CmpOp::Match | CmpOp::Eq => Ok(()),
        _ => err(format!("'{}' only supports : and =", t.field), t.span.clone()),
    }
}

fn build_regex(pattern: &str, flags: &str, t: &Term) -> Result<regex::Regex, FilterError> {
    let mut b = regex::RegexBuilder::new(pattern);
    b.size_limit(4 << 20);
    for f in flags.chars() {
        match f {
            'i' => b.case_insensitive(true),
            'm' => b.multi_line(true),
            's' => b.dot_matches_new_line(true),
            'x' => b.ignore_whitespace(true),
            'U' => b.swap_greed(true),
            other => return err(format!("unknown regex flag '{other}'"), t.span.clone()),
        };
    }
    b.build().map_err(|e| {
        let msg = e.to_string();
        let last = msg.lines().last().unwrap_or("invalid regex").trim().trim_start_matches("error: ");
        FilterError { message: format!("regex: {last}"), span: t.span.clone() }
    })
}

fn glob_regex(glob: &str, anchored: bool, ci: bool, t: &Term) -> Result<regex::Regex, FilterError> {
    let mut re = String::with_capacity(glob.len() + 16);
    if anchored {
        re.push('^');
    }
    for (i, part) in glob.split('*').enumerate() {
        if i > 0 {
            re.push_str(".*");
        }
        re.push_str(&regex::escape(part));
    }
    if anchored {
        re.push('$');
    }
    build_regex(&re, if ci { "si" } else { "s" }, t)
}

fn lower(s: &str) -> String {
    s.chars().flat_map(char::to_lowercase).collect()
}

/// Case-insensitive substring matcher. ASCII needles go to Aho-Corasick
/// (mergeable across OR branches); needles with cased non-ASCII letters need
/// full Unicode folding, which the regex engine provides.
fn contains_ci(s: &str, t: &Term) -> Result<StrMatcher, FilterError> {
    if s.is_empty() {
        return Ok(StrMatcher::Eq("".into()));
    }
    if s.chars().any(|c| !c.is_ascii() && (c.is_lowercase() || c.is_uppercase())) {
        return Ok(StrMatcher::Regex(build_regex(&regex::escape(s), "i", t)?));
    }
    Ok(StrMatcher::ac(vec![s.into()]))
}

fn str_matcher(t: &Term, v: &Value, mode: Mode) -> Result<StrMatcher, FilterError> {
    text_only(t)?;
    Ok(match (t.op, v) {
        (_, Value::Regex { pattern, flags }) => StrMatcher::Regex(build_regex(pattern, flags, t)?),
        (CmpOp::Regex, v) => StrMatcher::Regex(build_regex(v.text(), "i", t)?),
        (CmpOp::Eq, Value::Bare(s)) if s.contains('*') => StrMatcher::Regex(glob_regex(s, true, false, t)?),
        (CmpOp::Eq, v) => StrMatcher::Eq(v.text().into()),
        (_, Value::Bare(s)) if s == "*" => StrMatcher::Any,
        (_, Value::Bare(s)) if s.contains('*') => StrMatcher::Regex(glob_regex(s, mode == Mode::Exact, true, t)?),
        (_, v) => match mode {
            Mode::Contains => contains_ci(v.text(), t)?,
            Mode::ContainsCs => StrMatcher::Contains(memmem::Finder::new(v.text().as_bytes()).into_owned()),
            Mode::Exact => StrMatcher::EqCi(lower(v.text()).into()),
        },
    })
}

fn str_node(t: &Term, field: Field, mode: Mode) -> Result<Node, FilterError> {
    any_of(t, |v| Ok(Node::Str(field, str_matcher(t, v, mode)?)))
}

fn list_node(t: &Term, field: Field, mode: Mode) -> Result<Node, FilterError> {
    any_of(t, |v| Ok(Node::List(field, str_matcher(t, v, mode)?)))
}

fn and2(a: Node, b: Node) -> Node {
    Node::And(vec![a, b].into_boxed_slice())
}

fn resolve_alias(name: &str) -> Option<&'static str> {
    let l = name.to_ascii_lowercase();
    TYPE_ALIASES.iter().find(|(a, _)| *a == l).map(|(_, full)| *full)
}

fn collection_set(t: &Term, v: &Value) -> Result<CollectionSet, FilterError> {
    text_only(t)?;
    let mut set = CollectionSet::default();
    if let Value::Regex { pattern, flags } = v {
        set.regexes.push(build_regex(pattern, flags, t)?);
        set.patterns.push(format!("/{pattern}/").into());
        return Ok(set);
    }
    let text = v.text();
    if t.op == CmpOp::Regex {
        set.regexes.push(build_regex(text, "i", t)?);
        set.patterns.push(format!("/{text}/").into());
        return Ok(set);
    }
    let name = if text.contains('.') || text.contains('*') {
        text
    } else {
        match resolve_alias(text) {
            Some(full) => full,
            None => {
                let known: Vec<&str> = TYPE_ALIASES.iter().map(|(a, _)| *a).collect();
                return err(format!("unknown type '{text}' (try an NSID, or: {})", known.join(" ")), t.span.clone());
            }
        }
    };
    match name.strip_suffix(".*") {
        Some(prefix) if !prefix.contains('*') && !prefix.is_empty() => {
            set.prefixes.push(format!("{prefix}.").into());
        }
        _ if name.contains('*') => set.regexes.push(glob_regex(name, true, false, t)?),
        _ => set.exact.push(Nsid::intern(name)),
    }
    set.patterns.push(name.into());
    Ok(set)
}

fn normalize_did(s: &str) -> String {
    if s.starts_with("did:") {
        s.to_owned()
    } else {
        format!("did:{s}")
    }
}

fn did_node(t: &Term) -> Result<Node, FilterError> {
    text_only(t)?;
    any_of(t, |v| {
        let pattern_like = matches!(v, Value::Regex { .. })
            || t.op == CmpOp::Regex
            || matches!(v, Value::Bare(s) if s.contains('*'));
        if pattern_like {
            return Ok(Node::DidStr(str_matcher(t, v, Mode::Exact)?));
        }
        Ok(Node::Did(DidSet::One(Did::parse(&normalize_did(v.text())))))
    })
}

fn flag_node(name: &str) -> Option<Node> {
    let all = Node::FlagsAll;
    Some(match name {
        "reply" => all(Flags::REPLY),
        "quote" => all(Flags::QUOTE),
        "image" | "images" | "img" => all(Flags::IMAGES),
        "video" => all(Flags::VIDEO),
        "media" => Node::FlagsAny(Flags::IMAGES | Flags::VIDEO),
        "external" | "card" => all(Flags::EXTERNAL),
        "link" | "links" | "url" => all(Flags::LINK),
        "mention" | "mentions" => all(Flags::MENTION),
        "tag" | "tags" | "hashtag" | "hashtags" => all(Flags::TAG),
        "label" | "labels" => all(Flags::LABELS),
        "lang" | "langs" => all(Flags::LANGS),
        "alt" => all(Flags::ALT),
        "embed" => all(Flags::EMBED),
        "text" => all(Flags::TEXT),
        "record" => all(Flags::RECORD),
        "facet" | "facets" => all(Flags::FACETS),
        "gif" => all(Flags::GIF),
        "handle" => all(Flags::HANDLE),
        "name" => Node::Str(Field::Name, StrMatcher::Any),
        "subject" => Node::Str(Field::Subject, StrMatcher::Any),
        "created" | "createdat" => Node::Num(NumField::CreatedUs, NumOp::Gt, 0),
        _ => return None,
    })
}

fn exact_collection(name: &str) -> Node {
    Node::Collection(CollectionSet { exact: vec![Nsid::intern(name)], patterns: vec![name.into()], ..Default::default() })
}

fn has_node(t: &Term, is: bool) -> Result<Node, FilterError> {
    exact_only(t)?;
    any_of(t, |v| {
        let name = v.text().to_ascii_lowercase();
        if let Some(n) = flag_node(&name) {
            return Ok(n);
        }
        if is {
            if let Some(k) = Kind::parse(&name) {
                return Ok(Node::Kind(k.mask()));
            }
            if let Some(o) = Op::parse(&name) {
                return Ok(Node::Op(o.mask()));
            }
            match name.as_str() {
                "active" => return Ok(and2(Node::Kind(Kind::Account.mask()), Node::FlagsAll(Flags::ACTIVE))),
                "root" | "top" | "toplevel" => {
                    return Ok(and2(
                        exact_collection("app.bsky.feed.post"),
                        Node::Not(Box::new(Node::FlagsAll(Flags::REPLY))),
                    ))
                }
                _ => {}
            }
            if resolve_alias(&name).is_some() {
                return Ok(Node::Collection(collection_set(t, v)?));
            }
        }
        err(
            format!(
                "unknown {}: value '{name}' (reply quote media image video link mention tag label lang alt embed gif text record{})",
                t.field,
                if is { " root active post like …" } else { "" }
            ),
            t.span.clone(),
        )
    })
}

fn embed_node(t: &Term) -> Result<Node, FilterError> {
    exact_only(t)?;
    any_of(t, |v| {
        Ok(match v.text().to_ascii_lowercase().as_str() {
            "image" | "images" => Node::FlagsAll(Flags::IMAGES),
            "video" => Node::FlagsAll(Flags::VIDEO),
            "external" | "link" | "card" => Node::FlagsAll(Flags::EXTERNAL),
            "record" | "quote" => Node::FlagsAll(Flags::QUOTE),
            "media" => Node::FlagsAny(Flags::IMAGES | Flags::VIDEO),
            "gif" => Node::FlagsAll(Flags::GIF),
            "any" | "*" => Node::FlagsAll(Flags::EMBED),
            "none" => and2(Node::FlagsAll(Flags::RECORD), Node::Not(Box::new(Node::FlagsAll(Flags::EMBED)))),
            other => {
                return err(
                    format!("unknown embed '{other}' (images video external record media gif any none)"),
                    t.span.clone(),
                )
            }
        })
    })
}

fn parse_int(s: &str) -> Option<i64> {
    s.replace('_', "").parse().ok()
}

fn parse_time(s: &str) -> Option<i64> {
    parse_rfc3339_us(s).or_else(|| parse_int(s))
}

/// Numeric comparison(s) for one value: `5`, `>5`, `<=5`, `1..5`, `1..`, `..5`.
fn num_node(t: &Term, field: NumField, parse: fn(&str) -> Option<i64>) -> Result<Node, FilterError> {
    let bad = |s: &str| -> FilterError {
        FilterError { message: format!("'{s}' is not a valid value for '{}'", t.field), span: t.span.clone() }
    };
    any_of(t, |v| {
        let s = v.text();
        let one = |op: NumOp, s: &str| parse(s).map(|n| Node::Num(field, op, n)).ok_or_else(|| bad(s));
        match t.op {
            CmpOp::Gt => one(NumOp::Gt, s),
            CmpOp::Ge => one(NumOp::Ge, s),
            CmpOp::Lt => one(NumOp::Lt, s),
            CmpOp::Le => one(NumOp::Le, s),
            CmpOp::Regex => err(format!("'{}' is numeric", t.field), t.span.clone()),
            CmpOp::Match | CmpOp::Eq => {
                if let Some(rest) = s.strip_prefix(">=") {
                    one(NumOp::Ge, rest)
                } else if let Some(rest) = s.strip_prefix("<=") {
                    one(NumOp::Le, rest)
                } else if let Some(rest) = s.strip_prefix('>') {
                    one(NumOp::Gt, rest)
                } else if let Some(rest) = s.strip_prefix('<') {
                    one(NumOp::Lt, rest)
                } else if let Some((lo, hi)) = s.split_once("..") {
                    let mut parts = Vec::new();
                    if !lo.is_empty() {
                        parts.push(one(NumOp::Ge, lo)?);
                    }
                    if !hi.is_empty() {
                        parts.push(one(NumOp::Le, hi)?);
                    }
                    if parts.is_empty() {
                        return Err(bad(s));
                    }
                    Ok(Node::And(parts.into_boxed_slice()))
                } else {
                    one(NumOp::Eq, s)
                }
            }
        }
    })
}

fn sample_node(t: &Term) -> Result<Node, FilterError> {
    exact_only(t)?;
    any_of(t, |v| {
        let s = v.text();
        let rate = if let Some(p) = s.strip_suffix('%') {
            p.parse::<f64>().ok().map(|p| p / 100.0)
        } else if let Some((a, b)) = s.split_once('/') {
            match (a.parse::<f64>(), b.parse::<f64>()) {
                (Ok(a), Ok(b)) if b > 0.0 => Some(a / b),
                _ => None,
            }
        } else {
            s.parse::<f64>().ok()
        };
        match rate {
            Some(r) if r > 0.0 && r <= 1.0 => Ok(Node::Sample((r * u64::MAX as f64) as u64)),
            _ => err("sample takes a fraction: 10%, 0.1 or 1/10", t.span.clone()),
        }
    })
}

fn split_json_root(field: &str) -> Option<(JsonRoot, &str)> {
    for (prefix, root) in
        [("record.", JsonRoot::Record), ("r.", JsonRoot::Record), ("json.", JsonRoot::Frame), ("frame.", JsonRoot::Frame)]
    {
        if let Some(path) = field.strip_prefix(prefix) {
            return Some((root, path));
        }
    }
    match field {
        "record" => Some((JsonRoot::Record, "")),
        _ => None,
    }
}

fn json_node(t: &Term, root: JsonRoot, path: &str) -> Result<Node, FilterError> {
    let path = parse_path(path, t.span.clone())?;
    any_of(t, |v| {
        let test = match t.op {
            CmpOp::Gt | CmpOp::Ge | CmpOp::Lt | CmpOp::Le => {
                let n: f64 = match v.text().parse() {
                    Ok(n) => n,
                    Err(_) => return err(format!("'{}' is not a number", v.text()), t.span.clone()),
                };
                let op = match t.op {
                    CmpOp::Gt => NumOp::Gt,
                    CmpOp::Ge => NumOp::Ge,
                    CmpOp::Lt => NumOp::Lt,
                    _ => NumOp::Le,
                };
                JsonTest::Num(op, n)
            }
            _ => match str_matcher(t, v, Mode::Contains)? {
                StrMatcher::Any => JsonTest::Exists,
                m => JsonTest::Str(m),
            },
        };
        Ok(Node::Json(JsonPred { root, path: path.clone(), test }))
    })
}

fn uri_node(t: &Term) -> Result<Node, FilterError> {
    text_only(t)?;
    any_of(t, |v| {
        let plain = !matches!(v, Value::Regex { .. }) && t.op != CmpOp::Regex && !v.text().contains('*');
        if plain {
            // Decompose into pre-extracted fields instead of building the
            // URI string for every event.
            if let Some(rest) = v.text().strip_prefix("at://") {
                let mut parts = rest.splitn(3, '/');
                let mut nodes = Vec::new();
                if let Some(did) = parts.next().filter(|s| !s.is_empty()) {
                    nodes.push(Node::Did(DidSet::One(Did::parse(did))));
                }
                if let Some(coll) = parts.next().filter(|s| !s.is_empty()) {
                    nodes.push(exact_collection(coll));
                }
                if let Some(rkey) = parts.next().filter(|s| !s.is_empty()) {
                    nodes.push(Node::Str(Field::Rkey, StrMatcher::Eq(rkey.into())));
                }
                if !nodes.is_empty() {
                    nodes.push(Node::Kind(Kind::Commit.mask()));
                    return Ok(Node::And(nodes.into_boxed_slice()));
                }
            }
        }
        Ok(Node::Uri(str_matcher(t, v, Mode::Contains)?))
    })
}

fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            let sub = prev[j - 1] + usize::from(a[i - 1] != b[j - 1]);
            cur[j] = sub.min(prev[j] + 1).min(cur[j - 1] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

fn unknown_field(t: &Term) -> FilterError {
    let best = KNOWN_FIELDS
        .iter()
        .map(|k| (edit_distance(&t.field, k.trim_end_matches('.')), *k))
        .min()
        .filter(|(d, _)| *d <= 2);
    let message = match best {
        Some((_, k)) => format!("unknown field '{}' — did you mean '{}'?", t.field, k.trim_end_matches('.')),
        None => format!("unknown field '{}'", t.field),
    };
    FilterError { message, span: t.span.clone() }
}

fn compile_term(t: &Term) -> Result<Node, FilterError> {
    let field = t.field.as_str();
    if let Some((root, path)) = split_json_root(field) {
        return json_node(t, root, path);
    }
    match field {
        "kind" | "k" => {
            exact_only(t)?;
            any_of(t, |v| match Kind::parse(&v.text().to_ascii_lowercase()) {
                Some(k) => Ok(Node::Kind(k.mask())),
                None => err(format!("unknown kind '{}' (commit identity account sync)", v.text()), t.span.clone()),
            })
        }
        "op" | "action" | "operation" => {
            exact_only(t)?;
            any_of(t, |v| match Op::parse(&v.text().to_ascii_lowercase()) {
                Some(o) => Ok(Node::Op(o.mask())),
                None => err(format!("unknown op '{}' (create update delete)", v.text()), t.span.clone()),
            })
        }
        "collection" | "coll" | "c" | "nsid" | "type" => any_of(t, |v| Ok(Node::Collection(collection_set(t, v)?))),
        "did" | "from" | "author" | "by" | "repo" | "user" => did_node(t),
        "rkey" => str_node(t, Field::Rkey, Mode::Exact),
        "text" | "body" => str_node(t, Field::Text, Mode::Contains),
        "word" => {
            exact_only(t)?;
            let alts: Vec<String> = t.values.iter().map(|v| regex::escape(v.text())).collect();
            let re = build_regex(&format!(r"\b(?:{})\b", alts.join("|")), "i", t)?;
            Ok(Node::Str(Field::Text, StrMatcher::Regex(re)))
        }
        "name" | "displayname" | "title" => str_node(t, Field::Name, Mode::Contains),
        "alt" => str_node(t, Field::Alt, Mode::Contains),
        "subject" | "to" | "target" => str_node(t, Field::Subject, Mode::ContainsCs),
        "parent" | "reply-to" | "replyto" => str_node(t, Field::Parent, Mode::ContainsCs),
        "root" | "thread" => str_node(t, Field::Root, Mode::ContainsCs),
        "quote" | "quotes" | "quoting" => str_node(t, Field::Quote, Mode::ContainsCs),
        "raw" | "any" | "json" | "frame" => {
            // The raw frame is still JSON-escaped; search it case-sensitively
            // when asked for exactness, otherwise as a plain substring.
            text_only(t)?;
            any_of(t, |v| {
                Ok(Node::Str(
                    Field::Raw,
                    match (t.op, v) {
                        (CmpOp::Eq, Value::Bare(s) | Value::Quoted(s)) => {
                            StrMatcher::Contains(memmem::Finder::new(s.as_bytes()).into_owned())
                        }
                        _ => str_matcher(t, v, Mode::Contains)?,
                    },
                ))
            })
        }
        "handle" => Ok(and2(Node::Kind(Kind::Identity.mask()), str_node(t, Field::Aux, Mode::Exact)?)),
        "status" => Ok(and2(Node::Kind(Kind::Account.mask()), str_node(t, Field::Aux, Mode::Exact)?)),
        "active" => {
            exact_only(t)?;
            any_of(t, |v| {
                let flag = Node::FlagsAll(Flags::ACTIVE);
                let test = match v.text().to_ascii_lowercase().as_str() {
                    "true" | "yes" | "1" => flag,
                    "false" | "no" | "0" => Node::Not(Box::new(flag)),
                    other => return err(format!("active: expects true or false, got '{other}'"), t.span.clone()),
                };
                Ok(and2(Node::Kind(Kind::Account.mask()), test))
            })
        }
        "uri" | "at" => uri_node(t),
        "lang" | "language" => {
            exact_only(t)?;
            Ok(Node::Lang(t.values.iter().map(|v| v.text().to_ascii_lowercase().into()).collect()))
        }
        "tag" | "hashtag" => {
            text_only(t)?;
            any_of(t, |v| {
                let pattern_like = matches!(v, Value::Regex { .. })
                    || t.op == CmpOp::Regex
                    || matches!(v, Value::Bare(s) if s.contains('*'));
                if pattern_like {
                    return Ok(Node::List(Field::Tags, str_matcher(t, v, Mode::Exact)?));
                }
                // Tags are stored lowercased, so an exact compare suffices.
                let tag = lower(v.text().trim_start_matches('#'));
                Ok(Node::List(Field::Tags, StrMatcher::Eq(tag.into())))
            })
        }
        "mention" => {
            text_only(t)?;
            any_of(t, |v| {
                let pattern_like = matches!(v, Value::Regex { .. })
                    || t.op == CmpOp::Regex
                    || matches!(v, Value::Bare(s) if s.contains('*'));
                if pattern_like {
                    return Ok(Node::List(Field::Mentions, str_matcher(t, v, Mode::Exact)?));
                }
                Ok(Node::List(Field::Mentions, StrMatcher::Eq(normalize_did(v.text()).into())))
            })
        }
        "link" | "url" => list_node(t, Field::Links, Mode::Contains),
        "domain" | "host" | "site" => {
            exact_only(t)?;
            Ok(Node::Domain(
                t.values
                    .iter()
                    .map(|v| v.text().trim_start_matches("*.").trim_start_matches('.').to_ascii_lowercase().into())
                    .collect(),
            ))
        }
        "label" => list_node(t, Field::Labels, Mode::Exact),
        "has" => has_node(t, false),
        "is" => has_node(t, true),
        "embed" => embed_node(t),
        "len" | "chars" | "length" => num_node(t, NumField::TextChars, parse_int),
        "bytes" | "size" => num_node(t, NumField::RawBytes, parse_int),
        "images" => num_node(t, NumField::Images, parse_int),
        "langs" => num_node(t, NumField::Langs, parse_int),
        "tags" => num_node(t, NumField::Tags, parse_int),
        "mentions" => num_node(t, NumField::Mentions, parse_int),
        "links" => num_node(t, NumField::Links, parse_int),
        "seq" => num_node(t, NumField::Seq, parse_int),
        "time" => num_node(t, NumField::TimeUs, parse_time),
        "created" | "createdat" => num_node(t, NumField::CreatedUs, parse_time),
        "skew" => num_node(t, NumField::SkewUs, parse_duration_us),
        "sample" => sample_node(t),
        "exists" => {
            exact_only(t)?;
            any_of(t, |v| match split_json_root(v.text()) {
                Some((root, path)) => Ok(Node::Json(JsonPred {
                    root,
                    path: parse_path(path, t.span.clone())?,
                    test: JsonTest::Exists,
                })),
                None => err("exists: takes a path starting with record. or json.", t.span.clone()),
            })
        }
        _ => Err(unknown_field(t)),
    }
}
