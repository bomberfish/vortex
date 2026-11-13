//! Background lookups (AppView, PLC, PDS, constellation) and the detail pane's
//! context section built from them.

use super::*;
use crate::atp::{AtTarget, Client, Json, Res};
use serde_json::Value;

/// State of one lookup, keyed by a string like `profile:did:plc:…`.
pub(super) enum Fetch {
    Pending,
    Done(Res<Json>),
}

/// What an event points at, and how to label it.
#[derive(Clone)]
pub(super) enum Ref {
    Post(String),
    Actor(String),
    Record(String),
}

impl Vortex {
    /// Run `job` on a background thread once per `key`; re-render when done.
    pub(super) fn request(
        &mut self,
        key: String,
        cx: &mut Context<Self>,
        job: impl FnOnce(&Client) -> Res<Json> + Send + 'static,
    ) {
        if matches!(self.lookups.get(&key), Some(Fetch::Pending | Fetch::Done(Ok(_)))) {
            return;
        }
        self.lookups.insert(key.clone(), Fetch::Pending);
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { job(&client) }).await;
            let _ = this.update(cx, |this, cx| {
                this.lookups.insert(key, Fetch::Done(result));
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn lookup(&self, key: &str) -> Option<&Fetch> {
        self.lookups.get(key)
    }

    pub(super) fn lookup_ok(&self, key: &str) -> Option<&Json> {
        match self.lookups.get(key) {
            Some(Fetch::Done(Ok(v))) => Some(v),
            _ => None,
        }
    }

    pub(super) fn request_profile(&mut self, actor: &str, cx: &mut Context<Self>) {
        let a = actor.to_owned();
        self.request(format!("profile:{actor}"), cx, move |c| c.profile(&a));
    }

    pub(super) fn request_post(&mut self, uri: &str, cx: &mut Context<Self>) {
        let u = uri.to_owned();
        self.request(format!("post:{uri}"), cx, move |c| c.post(&u));
    }

    pub(super) fn request_identity(&mut self, repo: &str, cx: &mut Context<Self>) {
        let r = repo.to_owned();
        self.request(format!("ident:{repo}"), cx, move |c| {
            let id = c.identity(&r)?;
            let aka: Vec<&str> = id.doc["alsoKnownAs"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            let keys: Vec<&str> = id.doc["verificationMethod"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|k| k["publicKeyMultibase"].as_str())
                .collect();
            Ok(Arc::new(serde_json::json!({ "did": id.did, "handle": id.handle, "pds": id.pds, "aka": aka, "keys": keys })))
        });
    }

    /// Fetch context for the selection once it has settled (arrow-key
    /// scrolling should not fire a request per row).
    pub(super) fn schedule_context(&mut self, cx: &mut Context<Self>) {
        self.context_gen += 1;
        let generation = self.context_gen;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(150)).await;
            let _ = this.update(cx, |this, cx| {
                if this.context_gen == generation {
                    this.fetch_context(cx);
                }
            });
        })
        .detach();
    }

    fn fetch_context(&mut self, cx: &mut Context<Self>) {
        let Some(d) = &self.detail else { return };
        let e = &d.event;
        let did = e.did.to_string();
        let own_post = (e.collection_str() == "app.bsky.feed.post" && e.op != vortex_core::event::Op::Delete)
            .then(|| e.at_uri())
            .flatten();
        let refs = event_refs(e);
        self.request_profile(&did, cx);
        self.request_identity(&did, cx);
        if let Some(uri) = own_post {
            self.request_post(&uri, cx);
        }
        for (_, r) in refs {
            match r {
                Ref::Post(uri) => self.request_post(&uri, cx),
                Ref::Actor(d) => self.request_profile(&d, cx),
                Ref::Record(_) => {}
            }
        }
    }

    /// Author, the event's targets, and post counts, as far as fetched.
    pub(super) fn render_context(&self, e: &Event, t: &Theme, cx: &mut Context<Self>) -> Div {
        let did = e.did.to_string();
        let mut out = div().flex().flex_col().pt(t.pad).gap(px(1.));
        let label = |s: &'static str, t: &Theme| div().flex_none().w(t.ch * 8.).text_color(t.fg_faint).child(s);

        // Author.
        let mut author = div().flex().flex_col().min_w_0().flex_1();
        match self.lookup(&format!("profile:{did}")) {
            Some(Fetch::Done(Ok(p))) => {
                author = author.child(profile_line(p, t)).child(div().text_color(t.fg_faint).truncate().child(profile_stats(p)));
            }
            Some(Fetch::Done(Err(err))) => author = author.child(div().text_color(t.orange).truncate().child(err.clone())),
            _ => author = author.child(div().text_color(t.fg_faint).child("…")),
        }
        if let Some(id) = self.lookup_ok(&format!("ident:{did}")) {
            if let Some(pds) = id.get("pds").and_then(Value::as_str) {
                author = author.child(div().text_color(t.fg_faint).truncate().child(format!("pds {}", pds.trim_start_matches("https://"))));
            }
        }
        let target = AtTarget::repo(did.clone());
        out = out.child(
            div()
                .id("ctx-author")
                .flex()
                .gap(t.ch)
                .cursor_pointer()
                .hover({
                    let h = t.bg_hover;
                    move |s| s.bg(h)
                })
                .tooltip(tip("author", None, Some("click to inspect this repo")))
                .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| this.inspect(target.clone(), cx)))
                .child(label("by", t))
                .child(author),
        );

        // The event's own post counts.
        if let Some(uri) = e.at_uri().filter(|_| e.collection_str() == "app.bsky.feed.post") {
            if let Some(f) = self.lookup(&format!("post:{uri}")) {
                let text = match f {
                    Fetch::Done(Ok(p)) => post_counts(p),
                    Fetch::Done(Err(err)) => err.clone(),
                    Fetch::Pending => "…".into(),
                };
                out = out.child(div().flex().gap(t.ch).child(label("counts", t)).child(div().text_color(t.fg_dim).truncate().child(text)));
            }
        }

        // What it points at.
        for (i, (name, r)) in event_refs(e).into_iter().enumerate() {
            let (key, inspect_uri) = match &r {
                Ref::Post(u) => (format!("post:{u}"), u.clone()),
                Ref::Actor(d) => (format!("profile:{d}"), d.clone()),
                Ref::Record(u) => (String::new(), u.clone()),
            };
            let mut body = div().flex().flex_col().min_w_0().flex_1();
            match (&r, self.lookup(&key)) {
                (Ref::Post(_), Some(Fetch::Done(Ok(p)))) => {
                    let author = &p["author"];
                    body = body
                        .child(
                            div()
                                .flex()
                                .gap(t.ch)
                                .min_w_0()
                                .child(div().flex_none().text_color(t.blue).child(format!("@{}", author["handle"].as_str().unwrap_or("?"))))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_color(t.fg_dim)
                                        .child(author["displayName"].as_str().unwrap_or("").to_owned()),
                                ),
                        )
                        .child(
                            div()
                                .text_color(t.fg)
                                .max_h(t.row * 4.)
                                .overflow_hidden()
                                .child(p["record"]["text"].as_str().unwrap_or("").replace('\n', " ")),
                        )
                        .child(div().text_color(t.fg_faint).truncate().child(post_counts(p)));
                }
                (Ref::Actor(_), Some(Fetch::Done(Ok(p)))) => {
                    body = body.child(profile_line(p, t)).child(div().text_color(t.fg_faint).truncate().child(profile_stats(p)));
                }
                (_, Some(Fetch::Done(Err(err)))) => {
                    body = body
                        .child(div().text_color(t.fg_dim).truncate().child(inspect_uri.clone()))
                        .child(div().text_color(t.orange).truncate().child(err.clone()));
                }
                (Ref::Record(u), _) => body = body.child(div().text_color(t.fg_dim).truncate().child(u.clone())),
                _ => body = body.child(div().text_color(t.fg_faint).child("…")),
            }
            let target = crate::atp::parse_target(&inspect_uri);
            out = out.child(
                div()
                    .id(("ctx-ref", i))
                    .flex()
                    .gap(t.ch)
                    .cursor_pointer()
                    .hover({
                        let h = t.bg_hover;
                        move |s| s.bg(h)
                    })
                    .tooltip(tip(name, None, Some("click to inspect")))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            if let Some(t) = target.clone() {
                                this.inspect(t, cx);
                            }
                        }),
                    )
                    .child(label(name, t))
                    .child(body),
            );
        }
        out
    }
}

/// The records and accounts an event refers to, labelled for display.
pub(super) fn event_refs(e: &Event) -> Vec<(&'static str, Ref)> {
    let mut out = Vec::new();
    let subject = e.field(Field::Subject);
    if !subject.is_empty() {
        let name = match e.collection_str() {
            "app.bsky.feed.like" => "liked",
            "app.bsky.feed.repost" => "reposted",
            "app.bsky.graph.follow" => "follows",
            "app.bsky.graph.block" => "blocks",
            "app.bsky.graph.listitem" => "member",
            "app.bsky.graph.verification" => "verifies",
            _ => "subject",
        };
        let r = if subject.starts_with("did:") {
            Ref::Actor(subject.to_owned())
        } else if subject.contains("/app.bsky.feed.post/") {
            Ref::Post(subject.to_owned())
        } else {
            Ref::Record(subject.to_owned())
        };
        out.push((name, r));
    }
    let parent = e.field(Field::Parent);
    if !parent.is_empty() {
        out.push(("reply to", Ref::Post(parent.to_owned())));
    }
    let quote = e.field(Field::Quote);
    if !quote.is_empty() {
        out.push(("quotes", if quote.contains("/app.bsky.feed.post/") { Ref::Post(quote.to_owned()) } else { Ref::Record(quote.to_owned()) }));
    }
    out
}

fn profile_line(p: &Value, t: &Theme) -> Div {
    div()
        .flex()
        .gap(t.ch)
        .min_w_0()
        .child(div().flex_none().text_color(t.blue).child(format!("@{}", p["handle"].as_str().unwrap_or("?"))))
        .child(div().flex_1().min_w_0().truncate().text_color(t.fg).child(p["displayName"].as_str().unwrap_or("").to_owned()))
}

fn profile_stats(p: &Value) -> String {
    let n = |k: &str| fmt_count(p[k].as_u64().unwrap_or(0));
    let mut s = format!("{} followers · {} following · {} posts", n("followersCount"), n("followsCount"), n("postsCount"));
    if let Some(created) = p["createdAt"].as_str() {
        s.push_str(&format!(" · since {}", &created[..created.len().min(10)]));
    }
    s
}

fn post_counts(p: &Value) -> String {
    let n = |k: &str| fmt_count(p[k].as_u64().unwrap_or(0));
    format!("♥ {}  ⟲ {}  ↩ {}  ❝ {}", n("likeCount"), n("repostCount"), n("replyCount"), n("quoteCount"))
}
