//! A small read-only AT Protocol client for lookups: identity resolution,
//! profiles and posts (public AppView), repo browsing (the account's PDS),
//! PLC history, and backlinks (constellation).
//!
//! Calls block, so they run on background threads; successful GETs are cached
//! for the session (bounded), except paged listings.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const APPVIEW: &str = "https://public.api.bsky.app";
pub const PLC: &str = "https://plc.directory";
pub const CONSTELLATION: &str = "https://constellation.microcosm.blue";

const CACHE_MAX: usize = 4096;

pub type Json = Arc<Value>;
pub type Res<T> = Result<T, String>;

/// Something to look at: a repo, optionally narrowed to a collection or record.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AtTarget {
    /// A DID or a handle.
    pub repo: String,
    pub collection: Option<String>,
    pub rkey: Option<String>,
}

impl AtTarget {
    pub fn repo(repo: impl Into<String>) -> Self {
        AtTarget { repo: repo.into(), collection: None, rkey: None }
    }

    pub fn uri(&self) -> String {
        let mut s = format!("at://{}", self.repo);
        if let Some(c) = &self.collection {
            s.push('/');
            s.push_str(c);
            if let Some(r) = &self.rkey {
                s.push('/');
                s.push_str(r);
            }
        }
        s
    }

    /// The parent level (record → collection → repo).
    pub fn up(&self) -> Option<AtTarget> {
        match (&self.collection, &self.rkey) {
            (Some(_), Some(_)) => Some(AtTarget { rkey: None, ..self.clone() }),
            (Some(_), None) => Some(AtTarget::repo(self.repo.clone())),
            _ => None,
        }
    }
}

/// Parse what a person might paste: `at://…`, a DID, a handle (with or
/// without `@`), a bsky.app profile/post URL, or a pdsls.dev URL.
pub fn parse_target(input: &str) -> Option<AtTarget> {
    let s = input.trim().trim_start_matches('@');
    if s.is_empty() || s.contains(char::is_whitespace) {
        return None;
    }
    if let Some(rest) = s.strip_prefix("https://pdsls.dev/").or_else(|| s.strip_prefix("pdsls.dev/")) {
        return parse_target(rest);
    }
    for prefix in ["https://bsky.app/profile/", "bsky.app/profile/"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            let mut parts = rest.split('/').filter(|p| !p.is_empty());
            let repo = parts.next()?.to_owned();
            return Some(match (parts.next(), parts.next()) {
                (Some("post"), Some(rkey)) => {
                    AtTarget { repo, collection: Some("app.bsky.feed.post".into()), rkey: Some(rkey.into()) }
                }
                (Some("feed"), Some(rkey)) => {
                    AtTarget { repo, collection: Some("app.bsky.feed.generator".into()), rkey: Some(rkey.into()) }
                }
                (Some("lists"), Some(rkey)) => {
                    AtTarget { repo, collection: Some("app.bsky.graph.list".into()), rkey: Some(rkey.into()) }
                }
                _ => AtTarget::repo(repo),
            });
        }
    }
    let path = s.strip_prefix("at://").unwrap_or(s);
    let mut parts = path.split('/').filter(|p| !p.is_empty());
    let repo = parts.next()?;
    let is_did = repo.starts_with("did:") && repo.len() > 8;
    let is_handle = repo.contains('.') && repo.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    if !(is_did || is_handle) {
        return None;
    }
    Some(AtTarget {
        repo: repo.to_owned(),
        collection: parts.next().map(str::to_owned),
        rkey: parts.next().map(str::to_owned),
    })
}

/// Resolved identity of a repo.
#[derive(Debug, Clone)]
pub struct Identity {
    pub did: String,
    /// The first `at://` alias in the DID document (unverified).
    pub handle: Option<String>,
    pub pds: Option<String>,
    pub doc: Json,
}

pub struct Client {
    agent: ureq::Agent,
    cache: Mutex<HashMap<String, Json>>,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    pub fn new() -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(12)))
            .user_agent("vortex (atproto stream viewer)")
            .http_status_as_error(false)
            .build()
            .into();
        Client { agent, cache: Mutex::new(HashMap::new()) }
    }

    fn get(&self, base: &str, path: &str, query: &[(&str, &str)], cache: bool) -> Res<Json> {
        let mut key = format!("{base}{path}");
        for (k, v) in query {
            key.push_str(&format!("|{k}={v}"));
        }
        if cache {
            if let Some(v) = self.cache.lock().unwrap().get(&key) {
                return Ok(v.clone());
            }
        }
        let mut req = self.agent.get(format!("{base}{path}"));
        for (k, v) in query {
            req = req.query(*k, *v);
        }
        let mut res = req.call().map_err(|e| short_err(&e.to_string()))?;
        let status = res.status().as_u16();
        let body: Value =
            res.body_mut().read_json().map_err(|e| format!("HTTP {status}: {}", short_err(&e.to_string())))?;
        if status >= 400 {
            let msg = body.get("message").and_then(Value::as_str).or_else(|| body.get("error").and_then(Value::as_str));
            return Err(match msg {
                Some(m) => format!("{status}: {m}"),
                None => format!("HTTP {status}"),
            });
        }
        let body = Arc::new(body);
        if cache {
            let mut c = self.cache.lock().unwrap();
            if c.len() >= CACHE_MAX {
                c.clear();
            }
            c.insert(key, body.clone());
        }
        Ok(body)
    }

    /// A handle or DID to a DID.
    pub fn resolve(&self, repo: &str) -> Res<String> {
        if repo.starts_with("did:") {
            return Ok(repo.to_owned());
        }
        let v = self.get(APPVIEW, "/xrpc/com.atproto.identity.resolveHandle", &[("handle", repo)], true)?;
        v.get("did").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| "no DID for handle".into())
    }

    pub fn did_doc(&self, did: &str) -> Res<Json> {
        if let Some(host) = did.strip_prefix("did:web:") {
            let host = host.replace("%3A", ":");
            return self.get(&format!("https://{host}"), "/.well-known/did.json", &[], true);
        }
        if did.starts_with("did:plc:") {
            return self.get(PLC, &format!("/{did}"), &[], true);
        }
        Err(format!("unsupported DID method: {did}"))
    }

    pub fn identity(&self, repo: &str) -> Res<Identity> {
        let did = self.resolve(repo)?;
        let doc = self.did_doc(&did)?;
        let handle = doc
            .get("alsoKnownAs")
            .and_then(Value::as_array)
            .and_then(|a| a.iter().filter_map(Value::as_str).find_map(|s| s.strip_prefix("at://")))
            .map(str::to_owned);
        let pds = doc.get("service").and_then(Value::as_array).and_then(|services| {
            services.iter().find_map(|s| {
                let id = s.get("id").and_then(Value::as_str)?;
                (id.ends_with("#atproto_pds")).then(|| s.get("serviceEndpoint").and_then(Value::as_str)).flatten()
            })
        });
        Ok(Identity { did, handle, pds: pds.map(|p| p.trim_end_matches('/').to_owned()), doc })
    }

    pub fn profile(&self, actor: &str) -> Res<Json> {
        self.get(APPVIEW, "/xrpc/app.bsky.actor.getProfile", &[("actor", actor)], true)
    }

    /// One post with author and counts, via the AppView.
    pub fn post(&self, uri: &str) -> Res<Json> {
        let v = self.get(APPVIEW, "/xrpc/app.bsky.feed.getPosts", &[("uris", uri)], true)?;
        match v.get("posts").and_then(Value::as_array).and_then(|p| p.first()) {
            Some(p) => Ok(Arc::new(p.clone())),
            None => Err("not found (deleted, or not indexed yet)".into()),
        }
    }

    pub fn describe_repo(&self, pds: &str, did: &str) -> Res<Json> {
        self.get(pds, "/xrpc/com.atproto.repo.describeRepo", &[("repo", did)], true)
    }

    pub fn list_records(&self, pds: &str, did: &str, collection: &str, cursor: Option<&str>) -> Res<Json> {
        let mut q = vec![("repo", did), ("collection", collection), ("limit", "100")];
        if let Some(c) = cursor {
            q.push(("cursor", c));
        }
        self.get(pds, "/xrpc/com.atproto.repo.listRecords", &q, false)
    }

    pub fn get_record(&self, pds: &str, did: &str, collection: &str, rkey: &str) -> Res<Json> {
        self.get(
            pds,
            "/xrpc/com.atproto.repo.getRecord",
            &[("repo", did), ("collection", collection), ("rkey", rkey)],
            true,
        )
    }

    pub fn plc_log(&self, did: &str) -> Res<Json> {
        self.get(PLC, &format!("/{did}/log/audit"), &[], true)
    }

    /// Backlink counts to a DID or AT-URI, by collection and path.
    pub fn backlinks(&self, target: &str) -> Res<Json> {
        self.get(CONSTELLATION, "/links/all", &[("target", target)], true)
    }
}

fn short_err(e: &str) -> String {
    let e = e.lines().next().unwrap_or(e);
    if e.len() > 120 {
        format!("{}…", &e[..e.floor_char_boundary(120)])
    } else {
        e.to_owned()
    }
}

/// Backlink counts flattened to `(collection, path, records)`, largest first.
pub fn backlink_rows(v: &Value) -> Vec<(String, String, u64)> {
    let mut out = Vec::new();
    if let Some(links) = v.get("links").and_then(Value::as_object) {
        for (coll, paths) in links {
            if let Some(paths) = paths.as_object() {
                for (path, n) in paths {
                    let records = n.get("records").and_then(Value::as_u64).unwrap_or(0);
                    out.push((coll.clone(), path.clone(), records));
                }
            }
        }
    }
    out.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_what_people_paste() {
        let t = |s: &str| parse_target(s).map(|t| t.uri());
        assert_eq!(t("did:plc:z72i7hdynmk6r22z27h6tvur").as_deref(), Some("at://did:plc:z72i7hdynmk6r22z27h6tvur"));
        assert_eq!(t("@bsky.app").as_deref(), Some("at://bsky.app"));
        assert_eq!(
            t("at://did:plc:abc12345/app.bsky.feed.post/3k").as_deref(),
            Some("at://did:plc:abc12345/app.bsky.feed.post/3k")
        );
        assert_eq!(
            t("https://bsky.app/profile/bsky.app/post/3l6oveex3ii2l").as_deref(),
            Some("at://bsky.app/app.bsky.feed.post/3l6oveex3ii2l")
        );
        assert_eq!(
            t("https://pdsls.dev/at://did:plc:abc12345/app.bsky.feed.like").as_deref(),
            Some("at://did:plc:abc12345/app.bsky.feed.like")
        );
        assert_eq!(t("hello"), None);
        assert_eq!(t("two words"), None);
        let rec = parse_target("at://did:plc:abc12345/app.bsky.feed.post/3k").unwrap();
        assert_eq!(rec.up().unwrap().uri(), "at://did:plc:abc12345/app.bsky.feed.post");
        assert_eq!(rec.up().unwrap().up().unwrap().uri(), "at://did:plc:abc12345");
    }

    /// Live services; run with `cargo test -p vortex -- --ignored live_lookups`.
    #[test]
    #[ignore]
    fn live_lookups() {
        let c = Client::new();
        let id = c.identity("bsky.app").unwrap();
        assert_eq!(id.did, "did:plc:z72i7hdynmk6r22z27h6tvur");
        assert_eq!(id.handle.as_deref(), Some("bsky.app"));
        let pds = id.pds.clone().unwrap();
        let repo = c.describe_repo(&pds, &id.did).unwrap();
        assert!(repo["collections"].as_array().unwrap().iter().any(|v| v == "app.bsky.feed.post"));
        let page = c.list_records(&pds, &id.did, "app.bsky.feed.post", None).unwrap();
        let first = page["records"][0]["uri"].as_str().unwrap().to_owned();
        let post = c.post(&first).unwrap();
        assert!(post["likeCount"].is_u64());
        let rkey = first.rsplit('/').next().unwrap();
        assert!(c.get_record(&pds, &id.did, "app.bsky.feed.post", rkey).unwrap()["value"].is_object());
        assert!(c.profile(&id.did).unwrap()["followersCount"].as_u64().unwrap() > 1000);
        assert!(!c.plc_log(&id.did).unwrap().as_array().unwrap().is_empty());
        assert!(!backlink_rows(&c.backlinks(&id.did).unwrap()).is_empty());
        assert!(c.post("at://did:plc:z72i7hdynmk6r22z27h6tvur/app.bsky.feed.post/nope").is_err());
    }

    #[test]
    fn backlinks_flatten() {
        let v: Value = serde_json::from_str(
            r#"{"links":{"app.bsky.feed.like":{".subject.uri":{"records":5}},"app.bsky.feed.repost":{".subject.uri":{"records":9}}}}"#,
        )
        .unwrap();
        let rows = backlink_rows(&v);
        assert_eq!(rows[0], ("app.bsky.feed.repost".into(), ".subject.uri".into(), 9));
    }
}
