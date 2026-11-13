//! Turning events into display rows, and JSON into highlighted lines.

use crate::theme::Theme;
use gpui::{HighlightStyle, Hsla, SharedString};
use std::ops::Range;
use vortex_core::event::{Event, Field, Flags, Kind, Op};
use vortex_core::time::civil_from_us;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Green,
    Yellow,
    Red,
    Blue,
    Purple,
    Dim,
}

impl Tone {
    pub fn color(self, t: &Theme) -> Hsla {
        match self {
            Tone::Green => t.green,
            Tone::Yellow => t.yellow,
            Tone::Red => t.red,
            Tone::Blue => t.blue,
            Tone::Purple => t.purple,
            Tone::Dim => t.fg_faint,
        }
    }
}

/// Everything a list row needs, detached from the store lock.
#[derive(Debug, Clone)]
pub struct RowData {
    pub id: u64,
    pub time: SharedString,
    pub glyph: &'static str,
    pub tone: Tone,
    /// Collection (shortened) or event kind.
    pub what: SharedString,
    /// Full collection NSID, empty for non-commits.
    pub collection: &'static str,
    pub did: SharedString,
    pub text: SharedString,
    /// The preview is a reference (subject, handle…), not authored text.
    pub text_is_ref: bool,
    /// Compact markers: reply, quote, media, link.
    pub marks: SharedString,
}

pub fn clock(us: i64) -> String {
    let c = civil_from_us(us);
    format!("{:02}:{:02}:{:02}.{:03}", c.hour, c.minute, c.second, c.micros / 1000)
}

/// `app.bsky.feed.post` → `feed.post`; other namespaces are kept whole.
pub fn short_collection(nsid: &str) -> &str {
    nsid.strip_prefix("app.bsky.").unwrap_or(nsid)
}

/// `did:plc:7e6kocyzb77xkncplrkoojej` → `7e6kocyzb77xkncp`; others keep their method.
pub fn short_did(did: &str) -> &str {
    did.strip_prefix("did:plc:").map(|id| &id[..id.len().min(16)]).unwrap_or_else(|| did.strip_prefix("did:").unwrap_or(did))
}

fn preview(s: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max_chars * 2));
    for (n, c) in s.chars().enumerate() {
        if n >= max_chars {
            out.push('…');
            break;
        }
        out.push(if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c });
    }
    out
}

pub fn row_data(e: &Event) -> RowData {
    let (glyph, tone) = match (e.kind, e.op) {
        (Kind::Commit, Op::Create) => ("+", Tone::Green),
        (Kind::Commit, Op::Update) => ("~", Tone::Yellow),
        (Kind::Commit, Op::Delete) => ("-", Tone::Red),
        (Kind::Commit, Op::None) => ("·", Tone::Dim),
        (Kind::Identity, _) => ("@", Tone::Blue),
        (Kind::Account, _) => ("!", Tone::Purple),
        (Kind::Sync, _) => ("%", Tone::Dim),
    };
    let collection = e.collection_str();
    let what: SharedString = if collection.is_empty() { e.kind.as_str().into() } else { short_collection(collection).into() };

    let (text, text_is_ref) = if !e.text().is_empty() {
        (preview(e.text(), 400), false)
    } else if !e.field(Field::Name).is_empty() {
        (preview(e.field(Field::Name), 200), false)
    } else if !e.field(Field::Subject).is_empty() {
        (e.field(Field::Subject).trim_start_matches("at://").to_owned(), true)
    } else if e.kind == Kind::Account {
        let status = e.field(Field::Aux);
        let active = if e.flags.contains(Flags::ACTIVE) { "active" } else { "inactive" };
        (if status.is_empty() { active.to_owned() } else { format!("{active} {status}") }, true)
    } else {
        (e.field(Field::Aux).to_owned(), true)
    };

    let mut marks = String::new();
    for (flag, mark) in [
        (Flags::REPLY, '↩'),
        (Flags::QUOTE, '❝'),
        (Flags::IMAGES, '▣'),
        (Flags::VIDEO, '▶'),
        (Flags::LINK, '∞'),
    ] {
        if e.flags.contains(flag) {
            marks.push(mark);
        }
    }

    let did = e.did.to_string();
    RowData {
        id: e.id,
        time: clock(e.time_us).into(),
        glyph,
        tone,
        what,
        collection,
        did: short_did(&did).to_owned().into(),
        text: text.into(),
        text_is_ref,
        marks: marks.into(),
    }
}

/// One pretty-printed JSON line with its syntax spans.
#[derive(Debug, Clone)]
pub struct JsonLine {
    pub text: SharedString,
    pub spans: Vec<(Range<usize>, JsonTok)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonTok {
    Key,
    Str,
    Num,
    Lit,
    Punct,
}

impl JsonTok {
    pub fn style(self, t: &Theme) -> HighlightStyle {
        let color = match self {
            JsonTok::Key => t.blue,
            JsonTok::Str => t.green,
            JsonTok::Num => t.purple,
            JsonTok::Lit => t.orange,
            JsonTok::Punct => t.fg_faint,
        };
        HighlightStyle { color: Some(color), ..Default::default() }
    }
}

/// Tokenize one line of pretty-printed JSON.
pub fn highlight_json_line(line: &str) -> Vec<(Range<usize>, JsonTok)> {
    let b = line.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b' ' => i += 1,
            b'"' => {
                let start = i;
                i += 1;
                while i < b.len() {
                    match b[i] {
                        b'\\' => i += 2,
                        b'"' => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
                let end = i.min(b.len());
                let is_key = b[end..].iter().find(|c| **c != b' ') == Some(&b':');
                spans.push((start..end, if is_key { JsonTok::Key } else { JsonTok::Str }));
            }
            b'{' | b'}' | b'[' | b']' | b',' | b':' => {
                spans.push((i..i + 1, JsonTok::Punct));
                i += 1;
            }
            c => {
                let start = i;
                while i < b.len() && !matches!(b[i], b' ' | b',' | b'}' | b']' | b':') {
                    i += 1;
                }
                let tok = if c == b'-' || c.is_ascii_digit() { JsonTok::Num } else { JsonTok::Lit };
                spans.push((start..i.max(start + 1), tok));
                i = i.max(start + 1);
            }
        }
    }
    // Spans must sit on char boundaries; strings may contain multi-byte text
    // but always start and end on ASCII quotes, so only clamp defensively.
    spans.retain(|(r, _)| line.is_char_boundary(r.start) && line.is_char_boundary(r.end) && r.end <= line.len());
    spans
}

/// Pretty-print and tokenize raw JSON, capped so a huge record cannot stall
/// the UI. Overlong lines are truncated for display.
pub fn json_lines(raw: &str, max_lines: usize, max_line_chars: usize) -> Vec<JsonLine> {
    let pretty = vortex_core::json::pretty(raw, 2);
    let mut out = Vec::new();
    for (n, line) in pretty.lines().enumerate() {
        if n >= max_lines {
            out.push(JsonLine { text: "…".into(), spans: Vec::new() });
            break;
        }
        let shown: String = if line.len() > max_line_chars {
            let mut s: String = line.chars().take(max_line_chars).collect();
            s.push('…');
            s
        } else {
            line.to_owned()
        };
        let spans = highlight_json_line(&shown);
        out.push(JsonLine { text: shown.into(), spans });
    }
    out
}

pub fn fmt_count(n: u64) -> String {
    match n {
        0..=9_999 => n.to_string(),
        10_000..=999_999 => format!("{:.0}k", n as f64 / 1e3),
        1_000_000..=9_999_999 => format!("{:.2}M", n as f64 / 1e6),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

pub fn fmt_rate(per_s: f64) -> String {
    if per_s < 10.0 {
        format!("{per_s:.1}")
    } else if per_s < 10_000.0 {
        format!("{per_s:.0}")
    } else {
        format!("{:.1}k", per_s / 1e3)
    }
}

pub fn fmt_bytes(b: f64) -> String {
    if b < 1024.0 {
        format!("{b:.0}B")
    } else if b < 1024.0 * 1024.0 {
        format!("{:.0}K", b / 1024.0)
    } else if b < 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1}M", b / (1024.0 * 1024.0))
    } else {
        format!("{:.2}G", b / (1024.0 * 1024.0 * 1024.0))
    }
}

pub fn fmt_duration_us(us: i64) -> String {
    let a = us.unsigned_abs() as f64;
    let sign = if us < 0 { "-" } else { "" };
    if a < 1e3 {
        format!("{sign}{a:.0}µs")
    } else if a < 1e6 {
        format!("{sign}{:.0}ms", a / 1e3)
    } else if a < 60e6 {
        format!("{sign}{:.1}s", a / 1e6)
    } else if a < 3600e6 {
        format!("{sign}{:.1}m", a / 60e6)
    } else if a < 86_400e6 {
        format!("{sign}{:.1}h", a / 3600e6)
    } else {
        format!("{sign}{:.1}d", a / 86_400e6)
    }
}

/// A web URL for viewing the record an event refers to.
pub fn web_url(e: &Event) -> Option<String> {
    let did = e.did.to_string();
    match e.collection_str() {
        "app.bsky.feed.post" => Some(format!("https://bsky.app/profile/{did}/post/{}", e.field(Field::Rkey))),
        "app.bsky.actor.profile" | "" => Some(format!("https://bsky.app/profile/{did}")),
        "app.bsky.feed.generator" => Some(format!("https://bsky.app/profile/{did}/feed/{}", e.field(Field::Rkey))),
        "app.bsky.graph.list" => Some(format!("https://bsky.app/profile/{did}/lists/{}", e.field(Field::Rkey))),
        _ => Some(pdsls_url(e)),
    }
}

/// The event's record on pdsls.dev, or the repo for events without one.
pub fn pdsls_url(e: &Event) -> String {
    match e.at_uri() {
        Some(uri) => format!("https://pdsls.dev/{uri}"),
        None => format!("https://pdsls.dev/at://{}", e.did),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vortex_core::decode::frame::decode_json_frame;
    use vortex_core::decode::Decoded;
    use vortex_core::event::EventBuilder;

    fn ev(src: &str) -> Event {
        match decode_json_frame(src, &mut EventBuilder::new()).unwrap() {
            Decoded::Event(e) => e,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn pdsls_links() {
        let like = ev(r#"{"did":"did:plc:7e6kocyzb77xkncplrkoojej","time_us":1,"kind":"commit","commit":{"operation":"create","collection":"app.bsky.feed.like","rkey":"3k","record":{}}}"#);
        assert_eq!(pdsls_url(&like), "https://pdsls.dev/at://did:plc:7e6kocyzb77xkncplrkoojej/app.bsky.feed.like/3k");
        let ident = ev(r#"{"did":"did:plc:7e6kocyzb77xkncplrkoojej","time_us":1,"kind":"identity","identity":{"did":"did:plc:7e6kocyzb77xkncplrkoojej","seq":1,"time":"2026-01-01T00:00:00Z"}}"#);
        assert_eq!(pdsls_url(&ident), "https://pdsls.dev/at://did:plc:7e6kocyzb77xkncplrkoojej");
    }

    #[test]
    fn rows_for_each_shape() {
        let post = ev(r#"{"did":"did:plc:7e6kocyzb77xkncplrkoojej","time_us":1786603663959305,"kind":"commit","commit":{"operation":"create","collection":"app.bsky.feed.post","rkey":"3k","record":{"text":"line one\nline two","reply":{"parent":{"uri":"at://x/y/z"}},"embed":{"images":[{"alt":""}]}}}}"#);
        let r = row_data(&post);
        assert_eq!((r.glyph, r.tone), ("+", Tone::Green));
        assert_eq!(r.what.as_ref(), "feed.post");
        assert_eq!(r.collection, "app.bsky.feed.post");
        assert_eq!(r.did.as_ref(), "7e6kocyzb77xkncp");
        assert_eq!(r.text.as_ref(), "line one line two");
        assert_eq!(r.time.as_ref(), "06:47:43.959");
        assert_eq!(r.marks.as_ref(), "↩▣");
        assert!(!r.text_is_ref);

        let like = ev(r#"{"did":"did:web:example.com","time_us":1,"kind":"commit","commit":{"operation":"delete","collection":"com.example.thing","rkey":"3k","record":{"subject":{"uri":"at://did:plc:abc/app.bsky.feed.post/3x"}}}}"#);
        let r = row_data(&like);
        assert_eq!((r.glyph, r.what.as_ref(), r.did.as_ref()), ("-", "com.example.thing", "web:example.com"));
        assert_eq!(r.text.as_ref(), "did:plc:abc/app.bsky.feed.post/3x");
        assert!(r.text_is_ref);

        let id = ev(r#"{"did":"did:plc:7e6kocyzb77xkncplrkoojej","time_us":1,"kind":"identity","identity":{"handle":"alice.test"}}"#);
        let r = row_data(&id);
        assert_eq!((r.glyph, r.what.as_ref(), r.text.as_ref(), r.collection), ("@", "identity", "alice.test", ""));

        let acct = ev(r#"{"did":"did:plc:7e6kocyzb77xkncplrkoojej","time_us":1,"kind":"account","account":{"active":false,"status":"takendown"}}"#);
        assert_eq!(row_data(&acct).text.as_ref(), "inactive takendown");
    }

    #[test]
    fn json_tokens() {
        let toks = |l: &str| highlight_json_line(l).into_iter().map(|(r, t)| (l[r].to_owned(), t)).collect::<Vec<_>>();
        assert_eq!(
            toks(r#"  "key": "va\"l: ue","#),
            [
                ("\"key\"".into(), JsonTok::Key),
                (":".into(), JsonTok::Punct),
                ("\"va\\\"l: ue\"".into(), JsonTok::Str),
                (",".into(), JsonTok::Punct)
            ]
        );
        assert_eq!(toks("  \"n\": -12.5e3"), [("\"n\"".into(), JsonTok::Key), (":".into(), JsonTok::Punct), ("-12.5e3".into(), JsonTok::Num)]);
        assert_eq!(toks("true,"), [("true".into(), JsonTok::Lit), (",".into(), JsonTok::Punct)]);
        assert_eq!(toks("  \"日本語\","), [("\"日本語\"".into(), JsonTok::Str), (",".into(), JsonTok::Punct)]);
        assert_eq!(toks("\"unterminated"), [("\"unterminated".into(), JsonTok::Str)]);
        assert!(toks("").is_empty());
    }

    #[test]
    fn json_lines_are_capped() {
        let raw = format!(r#"{{"a":[{}],"long":"{}"}}"#, vec!["1"; 50].join(","), "x".repeat(500));
        let lines = json_lines(&raw, 10, 80);
        assert_eq!(lines.len(), 11);
        assert_eq!(lines.last().unwrap().text.as_ref(), "…");
        let all = json_lines(&raw, 1000, 80);
        assert!(all.iter().all(|l| l.text.chars().count() <= 81));
        assert_eq!(all[0].text.as_ref(), "{");
    }

    #[test]
    fn formatters() {
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(213_456), "213k");
        assert_eq!(fmt_count(1_234_567), "1.23M");
        assert_eq!(fmt_rate(3.21), "3.2");
        assert_eq!(fmt_rate(412.4), "412");
        assert_eq!(fmt_rate(75_617.0), "75.6k");
        assert_eq!(fmt_bytes(96.0 * 1024.0), "96K");
        assert_eq!(fmt_bytes(194.0 * 1024.0 * 1024.0), "194.0M");
        assert_eq!(fmt_duration_us(120_000), "120ms");
        assert_eq!(fmt_duration_us(-3_600_000_000), "-1.0h");
        assert_eq!(short_collection("app.bsky.graph.follow"), "graph.follow");
        assert_eq!(short_did("did:plc:abc"), "abc");
    }

    #[test]
    fn web_urls() {
        let post = ev(r#"{"did":"did:plc:7e6kocyzb77xkncplrkoojej","time_us":1,"kind":"commit","commit":{"operation":"create","collection":"app.bsky.feed.post","rkey":"3kabc","record":{}}}"#);
        assert_eq!(web_url(&post).unwrap(), "https://bsky.app/profile/did:plc:7e6kocyzb77xkncplrkoojej/post/3kabc");
        let other = ev(r#"{"did":"did:plc:7e6kocyzb77xkncplrkoojej","time_us":1,"kind":"commit","commit":{"operation":"create","collection":"com.example.x","rkey":"r","record":{}}}"#);
        assert_eq!(web_url(&other).unwrap(), "https://pdsls.dev/at://did:plc:7e6kocyzb77xkncplrkoojej/com.example.x/r");
    }
}
