//! Event filtering: a query language compiled to an optimized matcher.
//!
//! ```text
//! query text ──parse──▶ AST ──compile──▶ matcher tree ──optimize──▶ Filter
//!                                                         │
//!                                                         └─▶ Pushdown (server-side filter)
//! ```
//!
//! See [`compile::FIELD_HELP`] for the field reference.

mod compile;
mod jsonpath;
mod node;
mod parse;
mod pushdown;

pub use compile::FIELD_HELP;
pub use parse::FilterError;
pub use pushdown::Pushdown;

use crate::event::Event;
use node::Node;

pub struct Filter {
    source: String,
    root: Node,
    pushdown: Pushdown,
}

impl Filter {
    /// A filter that matches everything.
    pub fn all() -> Filter {
        Filter { source: String::new(), root: Node::True, pushdown: Pushdown::default() }
    }

    pub fn parse(source: &str) -> Result<Filter, FilterError> {
        let ast = parse::parse(source)?;
        let root = compile::compile(&ast)?.optimize();
        let pushdown = pushdown::derive(&root);
        Ok(Filter { source: source.trim().to_owned(), root, pushdown })
    }

    #[inline]
    pub fn matches(&self, e: &Event) -> bool {
        self.root.eval(e)
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    /// True if the filter matches every event.
    pub fn is_all(&self) -> bool {
        matches!(self.root, Node::True)
    }

    /// True if the filter can never match.
    pub fn is_none(&self) -> bool {
        matches!(self.root, Node::False)
    }

    /// The server-side filter this query implies.
    pub fn pushdown(&self) -> &Pushdown {
        &self.pushdown
    }

    /// The optimized matcher tree, for debugging.
    pub fn describe(&self) -> String {
        let mut s = String::new();
        self.root.describe(&mut s);
        s
    }
}

impl std::fmt::Debug for Filter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Filter({:?} => {})", self.source, self.describe())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::frame::decode_json_frame;
    use crate::decode::Decoded;
    use crate::event::{EventBuilder, Kind};

    fn ev(src: &str) -> Event {
        match decode_json_frame(src, &mut EventBuilder::new()).unwrap() {
            Decoded::Event(e) => e,
            other => panic!("{other:?}"),
        }
    }

    fn commit(did: &str, op: &str, collection: &str, record: &str) -> Event {
        let rec = if record.is_empty() { String::new() } else { format!(r#","record":{record}"#) };
        ev(&format!(
            r#"{{"$type":"message","payload":{{"$type":"x#commit","did":"{did}","seq":1000,"time":"2026-08-13T06:47:43.000000Z","operation":"{op}","collection":"{collection}","rkey":"3kabc","rev":"r"{rec}}}}}"#
        ))
    }

    const ALICE: &str = "did:plc:7e6kocyzb77xkncplrkoojej";
    const BOB: &str = "did:plc:z72i7hdynmk6r22z27h6tvur";

    fn post() -> Event {
        commit(
            ALICE,
            "create",
            "app.bsky.feed.post",
            r##"{"$type":"app.bsky.feed.post","createdAt":"2026-08-13T06:47:44.000Z","langs":["en-US","ja"],"text":"Hello Wörld, Rust is GREAT\nsecond line","facets":[{"features":[{"$type":"app.bsky.richtext.facet#tag","tag":"RustLang"}]},{"features":[{"$type":"app.bsky.richtext.facet#mention","did":"did:plc:z72i7hdynmk6r22z27h6tvur"},{"$type":"app.bsky.richtext.facet#link","uri":"https://blog.Example.com/post?id=1"}]}],"embed":{"$type":"app.bsky.embed.images","images":[{"alt":"A grey cat","image":{}},{"alt":"","image":{}}]},"reply":{"parent":{"uri":"at://did:plc:z72i7hdynmk6r22z27h6tvur/app.bsky.feed.post/3parent"},"root":{"uri":"at://did:plc:rootrootrootrootrootroot/app.bsky.feed.post/3root"}},"labels":{"values":[{"val":"sexual"}]},"n":{"deep":[1,2,{"x":7.5}]}}"##,
        )
    }

    fn like() -> Event {
        commit(
            BOB,
            "create",
            "app.bsky.feed.like",
            r#"{"$type":"app.bsky.feed.like","createdAt":"2026-08-13T05:00:00.000Z","subject":{"cid":"c","uri":"at://did:plc:7e6kocyzb77xkncplrkoojej/app.bsky.feed.post/3liked"}}"#,
        )
    }

    fn plain_post() -> Event {
        commit(
            BOB,
            "create",
            "app.bsky.feed.post",
            r#"{"$type":"app.bsky.feed.post","createdAt":"2026-08-13T06:47:43.000Z","text":"just some plain words"}"#,
        )
    }

    fn delete() -> Event {
        commit(BOB, "delete", "app.bsky.graph.follow", "")
    }

    fn identity() -> Event {
        ev(&format!(
            r#"{{"$type":"message","payload":{{"$type":"x#identity","did":"{ALICE}","seq":7,"time":"2026-08-13T06:47:43.000000Z","identity":{{"handle":"alice.bsky.social"}}}}}}"#
        ))
    }

    fn account(active: bool, status: &str) -> Event {
        ev(&format!(
            r#"{{"$type":"message","payload":{{"$type":"x#account","did":"{BOB}","seq":8,"time":"2026-08-13T06:47:43.000000Z","account":{{"active":{active},"status":"{status}"}}}}}}"#
        ))
    }

    #[track_caller]
    fn yes(q: &str, e: &Event) {
        let f = Filter::parse(q).unwrap_or_else(|err| panic!("{q}: {err}"));
        assert!(f.matches(e), "expected match: {q}\n  {f:?}");
    }

    #[track_caller]
    fn no(q: &str, e: &Event) {
        let f = Filter::parse(q).unwrap_or_else(|err| panic!("{q}: {err}"));
        assert!(!f.matches(e), "expected NO match: {q}\n  {f:?}");
    }

    #[test]
    fn empty_matches_everything() {
        let f = Filter::parse("  ").unwrap();
        assert!(f.is_all());
        for e in [post(), like(), delete(), identity(), account(true, "")] {
            assert!(f.matches(&e));
        }
        assert!(Filter::all().matches(&post()));
    }

    #[test]
    fn text_search() {
        let p = post();
        yes("hello", &p);
        yes("HELLO", &p);
        yes("\"rust is great\"", &p);
        yes("text:\"second line\"", &p);
        yes("wörld", &p);
        yes("WÖRLD", &p);
        no("goodbye", &p);
        no("hello goodbye", &p);
        yes("hello OR goodbye", &p);
        yes("text:goodbye,rust", &p);
        yes("text=\"Hello Wörld, Rust is GREAT\nsecond line\"", &p);
        no("text=hello", &p);
        yes("text:hel*wörld", &p);
        yes("text:*", &p);
        no("text:*", &like());
        yes("text:\"\"", &like());
        // Regex: `~` ignores case, a /literal/ is exact unless flagged.
        yes("text~^hello", &p);
        yes("text:/GREAT$/m", &p);
        no("text:/great$/m", &p);
        yes("text:/great$/mi", &p);
        yes("/R[aeiou]st/", &p);
        no("text!~rust", &p);
        yes("word:rust", &p);
        no("word:rus", &p);
        yes("rus", &p);
    }

    #[test]
    fn kinds_ops_collections() {
        let (p, l, d, i, a) = (post(), like(), delete(), identity(), account(false, "takendown"));
        yes("kind:commit", &p);
        no("kind:commit", &i);
        yes("kind:identity,account", &i);
        yes("kind:identity,account", &a);
        yes("-kind:commit", &a);
        yes("op:create", &p);
        yes("op:delete", &d);
        no("op:delete", &p);
        no("op:create", &i);
        yes("is:delete", &d);
        yes("collection:app.bsky.feed.post", &p);
        yes("type:post", &p);
        yes("is:post", &p);
        yes("type:post,like", &l);
        no("type:post", &l);
        yes("collection:app.bsky.feed.*", &l);
        yes("collection:app.bsky.*", &d);
        no("collection:app.bsky.feed.*", &d);
        yes("collection:*.follow", &d);
        yes("type:bsky", &d);
        yes("collection~feed", &p);
        yes("collection:/graph\\.(follow|block)$/", &d);
        no("collection:app.bsky.feed.post", &i);
        yes("-type:post", &i);
        yes("is:reply", &p);
        no("is:root", &p);
        yes("is:root", &plain_post());
        no("is:root", &l);
    }

    #[test]
    fn dids_and_uris() {
        let (p, l) = (post(), like());
        yes(&format!("did:{ALICE}"), &p);
        yes("did:plc:7e6kocyzb77xkncplrkoojej", &p);
        yes(&format!("from:{ALICE},{BOB}"), &l);
        no(&format!("did:{BOB}"), &p);
        yes("did:did:plc:7e6*", &p);
        yes("did~^did:plc:z72", &l);
        yes(&format!("-did:{BOB}"), &p);
        yes(&format!("uri:at://{ALICE}/app.bsky.feed.post/3kabc"), &p);
        no(&format!("uri:at://{ALICE}/app.bsky.feed.post/other"), &p);
        yes(&format!("uri:at://{ALICE}"), &p);
        yes("uri:*/app.bsky.feed.post/3k*", &p);
        yes("rkey:3KABC", &p);
        yes("rkey=3kabc", &p);
        no("rkey=3KABC", &p);
        yes("rkey:3k*", &p);
        // References.
        yes(&format!("subject:{ALICE}"), &l);
        yes("to:3liked", &l);
        yes(&format!("reply-to:{BOB}"), &p);
        yes("root:rootroot", &p);
        no(&format!("reply-to:{ALICE}"), &p);
        yes(&format!("mention:{BOB}"), &p);
        yes("mention:plc:z72i7hdynmk6r22z27h6tvur", &p);
        no(&format!("mention:{ALICE}"), &p);
    }

    #[test]
    fn langs_tags_links_labels() {
        let p = post();
        yes("lang:en", &p);
        yes("lang:EN-us", &p);
        yes("lang:ja", &p);
        yes("lang:de,ja", &p);
        no("lang:de", &p);
        no("lang:e", &p);
        yes("#rustlang", &p);
        yes("tag:RUSTLANG", &p);
        yes("tag:#rustlang", &p);
        yes("tag:rust*", &p);
        no("tag:rust", &p);
        yes("link:example.com/post", &p);
        yes("domain:example.com", &p);
        yes("domain:blog.example.com", &p);
        no("domain:ample.com", &p);
        yes("label:sexual", &p);
        no("label:porn", &p);
        yes("alt:\"grey cat\"", &p);
    }

    #[test]
    fn flags() {
        let (p, l, plain) = (post(), like(), plain_post());
        for q in ["has:reply", "has:images", "has:media", "has:link", "has:mention", "has:tag", "has:label", "has:alt", "has:lang", "has:embed", "has:text", "has:record", "embed:images", "embed:media", "has:media,video"] {
            yes(q, &p);
        }
        for q in ["has:video", "has:quote", "embed:video", "embed:external", "embed:none", "has:gif"] {
            no(q, &p);
        }
        yes("embed:none", &plain);
        no("embed:none", &delete());
        no("has:text", &l);
        yes("has:subject", &l);
        yes("-has:media", &plain);
        yes("has:reply has:images -has:video", &p);
        assert!(Filter::parse("has:nonsense").is_err());
    }

    #[test]
    fn numbers() {
        let p = post();
        yes("images:2", &p);
        yes("images>1", &p);
        yes("images>=2", &p);
        no("images>2", &p);
        yes("images:1..3", &p);
        yes("images:..2", &p);
        yes("images:2..", &p);
        no("images:3..", &p);
        yes("langs:2", &p);
        yes("tags:1 mentions:1 links:1", &p);
        yes("len>20", &p);
        yes("len:<100", &p);
        yes("len:>=38 len:<=38", &p);
        yes("bytes>500", &p);
        yes("seq:1000", &p);
        yes("seq>=1_000", &p);
        yes("time>2026-08-13T00:00:00Z", &p);
        no("time<2026-08-13", &p);
        yes("created>2026-01-01", &p);
        no("created>2026-01-01", &delete());
        yes("skew:<5s", &p);
        yes("skew>1h", &like());
        no("skew>1h", &p);
        yes("images!=3", &p);
        assert!(Filter::parse("len:abc").is_err());
        assert!(Filter::parse("len~3").is_err());
        assert!(Filter::parse("text>3").is_err());
    }

    #[test]
    fn identity_and_account_fields() {
        let (i, up, down) = (identity(), account(true, ""), account(false, "takendown"));
        yes("handle:alice.bsky.social", &i);
        yes("handle:*.bsky.social", &i);
        yes("handle~bsky", &i);
        no("handle:bob.bsky.social", &i);
        no("handle:*", &post());
        yes("status:takendown", &down);
        yes("status:takendown,suspended", &down);
        no("status:takendown", &up);
        yes("active:true", &up);
        yes("active:false", &down);
        no("active:false", &up);
        no("active:false", &post());
        yes("is:active", &up);
        // Record predicates never match non-commits; their negation does.
        no("hello", &i);
        yes("-hello", &i);
    }

    #[test]
    fn json_paths() {
        let p = post();
        yes("record.$type=app.bsky.feed.post", &p);
        yes("record.embed.$type:IMAGES", &p);
        yes("record.embed.images.*.alt:cat", &p);
        yes("record.embed.images[0].alt=\"A grey cat\"", &p);
        no("record.embed.images[1].alt:cat", &p);
        yes("record.facets[*].features[*].tag:rustlang", &p);
        yes("record.n.deep[2].x>7", &p);
        yes("record.n.deep.*<2", &p);
        no("record.n.deep.*>2", &p);
        yes("record.n.deep.0=1", &p);
        yes("exists:record.reply", &p);
        yes("record.reply:*", &p);
        no("exists:record.reply", &plain_post());
        no("exists:record.reply", &delete());
        yes("-exists:record.reply", &delete());
        yes("json.payload.seq=1000", &p);
        yes("json.payload.rev:r", &p);
        yes("r.text~^hello", &p);
        yes("raw:\"app.bsky.embed.images\"", &p);
        yes("raw=\"\\\"seq\\\":1000\"", &p);
        assert!(Filter::parse("record.a[x]:1").is_err());
        assert!(Filter::parse("exists:nope").is_err());
    }

    #[test]
    fn boolean_structure() {
        let (p, l, d) = (post(), like(), delete());
        let q = "(type:post lang:en has:media) OR (type:like -did:did:plc:nope) OR op:delete";
        for e in [&p, &l, &d] {
            yes(q, e);
        }
        no(q, &identity());
        yes("NOT (type:like OR type:repost)", &p);
        no("NOT (type:like OR type:repost)", &l);
        yes("type:post AND (hello | goodbye) && !has:video", &p);
        yes("-(-hello)", &p);
    }

    #[test]
    fn sampling_is_deterministic_and_proportional() {
        let f = Filter::parse("sample:25%").unwrap();
        let mut hits = 0;
        let n = 20_000;
        for i in 0..n {
            let e = ev(&format!(
                r#"{{"$type":"message","payload":{{"$type":"x#identity","did":"{ALICE}","seq":{i},"time":"2026-08-13T06:47:43.000000Z","identity":{{}}}}}}"#
            ));
            if f.matches(&e) {
                hits += 1;
            }
            assert_eq!(f.matches(&e), f.matches(&e));
        }
        let frac = hits as f64 / n as f64;
        assert!((0.23..0.27).contains(&frac), "{frac}");
        yes("sample:1/1", &post());
        assert!(Filter::parse("sample:0").is_err());
        assert!(Filter::parse("sample:150%").is_err());
    }

    #[test]
    fn optimizer_merges_and_orders() {
        let d = |q: &str| Filter::parse(q).unwrap().describe();
        // Text alternatives collapse into one automaton.
        assert_eq!(d("foo OR bar OR baz"), "Text~ac(3)");
        assert_eq!(d("text:foo,bar"), "Text~ac(2)");
        // DID lists become one hash set.
        assert_eq!(d(&format!("did:{ALICE},{BOB}")), "did[2 dids]");
        // Collection alternatives merge.
        assert!(d("type:post OR type:like").starts_with("collection["));
        assert!(!d("type:post OR type:like").contains("or("));
        // Masks fold; contradictions vanish.
        assert_eq!(d("kind:commit kind:identity"), "false");
        assert_eq!(d("kind:commit OR kind:identity OR kind:account OR kind:sync"), "true");
        assert_eq!(d("kind:identity hello"), "false");
        assert_eq!(d("-(-kind:commit)"), "kind[0001]");
        assert_eq!(d("has:reply has:images"), "all[Flags(REPLY | IMAGES)]");
        assert_eq!(d("has:reply OR has:images"), "any[Flags(REPLY | IMAGES)]");
        // Cheap tests run before scans, scans before JSON walks.
        let s = d("record.x:1 /re+/ hello has:reply type:post");
        let pos = |needle: &str| s.find(needle).unwrap_or_else(|| panic!("{needle} not in {s}"));
        assert!(pos("all[") < pos("Text~ac"));
        assert!(pos("collection[") < pos("Text~ac"));
        assert!(pos("Text~ac") < pos("Text~re"));
        assert!(pos("Text~re") < pos("json"));
        assert!(Filter::parse("kind:commit kind:identity").unwrap().is_none());
    }

    #[test]
    fn pushdown_is_derived_soundly() {
        let pd = |q: &str| Filter::parse(q).unwrap().pushdown().clone();
        let colls = |q: &str| pd(q).collections;
        let some = |v: &[&str]| Some(v.iter().map(|s| s.to_string()).collect::<Vec<_>>());

        assert!(pd("").is_empty());
        assert!(pd("hello").collections.is_none());
        assert_eq!(pd("hello").kinds, Some(vec![Kind::Commit]));
        assert_eq!(colls("type:post"), some(&["app.bsky.feed.post"]));
        assert_eq!(colls("type:post hello lang:en"), some(&["app.bsky.feed.post"]));
        assert_eq!(colls("type:post,like"), some(&["app.bsky.feed.like", "app.bsky.feed.post"]));
        assert_eq!(colls("(type:post hello) OR (type:like)"), some(&["app.bsky.feed.like", "app.bsky.feed.post"]));
        assert_eq!(colls("collection:app.bsky.graph.*"), some(&["app.bsky.graph.*"]));
        // One unconstrained branch means no collection pushdown…
        assert_eq!(colls("type:post OR hello"), None);
        assert_eq!(colls("-type:post"), None);
        assert_eq!(colls("collection:*.post"), None);
        assert_eq!(colls("collection~feed"), None);
        assert_eq!(colls("collection:app.*"), None);
        // …but non-commit branches do not widen it: they bypass it server-side.
        let p = pd("type:post OR kind:identity");
        assert_eq!(p.collections, some(&["app.bsky.feed.post"]));
        assert_eq!(p.kinds, Some(vec![Kind::Commit, Kind::Identity]));
        let p = pd("kind:identity,account");
        assert_eq!((p.kinds, p.collections), (Some(vec![Kind::Identity, Kind::Account]), None));
        assert_eq!(pd("handle:*.test").kinds, Some(vec![Kind::Identity]));
        assert_eq!(pd("-kind:commit").kinds, Some(vec![Kind::Identity, Kind::Account, Kind::Sync]));
        assert_eq!(pd("-hello").kinds, None);

        assert_eq!(pd(&format!("did:{ALICE}")).dids, some(&[ALICE]));
        assert_eq!(pd(&format!("did:{ALICE},{BOB} type:post")).dids, some(&[ALICE, BOB]));
        assert_eq!(pd(&format!("did:{ALICE} OR hello")).dids, None);
        assert_eq!(pd(&format!("did:{ALICE},{BOB} did:{BOB}")).dids, some(&[BOB]));
        assert_eq!(pd(&format!("-did:{ALICE}")).dids, None);
        assert_eq!(pd("did:did:plc:7e6*").dids, None);
        assert_eq!(pd(&format!("uri:at://{ALICE}/app.bsky.feed.post/3k")).dids, some(&[ALICE]));

        // Contradictions push nothing down.
        assert!(pd("kind:commit kind:identity").is_empty());
        assert_eq!(pd("type:post").summary(), "commit · app.bsky.feed.post");
    }

    #[test]
    fn did_lists_from_files() {
        let dir = std::env::temp_dir().join(format!("vortex-filter-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dids.txt");
        std::fs::write(&path, format!("# watchlist\n{ALICE}\n\n  {BOB}  \n")).unwrap();
        let q = format!("did:@{}", path.display());
        let f = Filter::parse(&q).unwrap();
        assert!(f.matches(&post()) && f.matches(&like()));
        assert_eq!(f.pushdown().dids.as_ref().map(Vec::len), Some(2));
        assert!(Filter::parse("did:@/definitely/not/here.txt").is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn helpful_errors() {
        let e = Filter::parse("lnag:en").unwrap_err();
        assert!(e.message.contains("did you mean 'lang'"), "{}", e.message);
        assert_eq!(e.span, 0..7);
        let e = Filter::parse("hello kind:nope").unwrap_err();
        assert_eq!(e.span, 6..15);
        assert!(Filter::parse("text:/(/").unwrap_err().message.starts_with("regex:"));
        assert!(Filter::parse("type:nonsense").unwrap_err().message.contains("unknown type"));
        assert!(Filter::parse("zzzzzzzz:1").unwrap_err().message.contains("unknown field"));
    }

    /// Every pushdown must be a necessary condition of its query: an event the
    /// local filter matches may never be excluded by the derived server filter.
    #[test]
    fn pushdown_never_excludes_a_local_match() {
        let events = [post(), like(), plain_post(), delete(), identity(), account(true, ""), account(false, "suspended")];
        let queries = [
            "type:post", "type:post,like", "hello", "type:post OR kind:identity", "-type:post", "kind:account",
            "op:delete", "has:media", "-has:media", "handle:*", "active:false", "collection:app.bsky.graph.*",
            "did:did:plc:7e6kocyzb77xkncplrkoojej", "did:did:plc:7e6kocyzb77xkncplrkoojej OR type:like",
            "(type:post lang:en) OR (type:like skew>1h) OR status:suspended", "-kind:commit", "is:root", "sample:50%",
            "record.text:hello", "raw:plc", "embed:none", "-exists:record.reply", "text:\"\"", "-text:*",
        ];
        for q in queries {
            let f = Filter::parse(q).unwrap();
            let pd = f.pushdown();
            for e in &events {
                if !f.matches(e) {
                    continue;
                }
                if let Some(kinds) = &pd.kinds {
                    assert!(kinds.contains(&e.kind), "{q}: kinds excludes {:?}", e.kind);
                }
                if let Some(dids) = &pd.dids {
                    assert!(dids.contains(&e.did.to_string()), "{q}: dids excludes {}", e.did);
                }
                if let (Some(colls), Kind::Commit) = (&pd.collections, e.kind) {
                    let c = e.collection_str();
                    let ok = colls.iter().any(|p| match p.strip_suffix('*') {
                        Some(prefix) => c.starts_with(prefix),
                        None => p == c,
                    });
                    assert!(ok, "{q}: collections {colls:?} excludes {c}");
                }
            }
        }
    }
}
