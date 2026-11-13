//! A pdsls-style repo browser: identity, collections, records, PLC history
//! and backlinks for any account, reached from the detail pane, the command
//! palette, or `i` on a selected event.

use super::lookup::Fetch;
use super::*;
use crate::atp::{backlink_rows, parse_target, AtTarget};
use gpui::ElementId;
use serde_json::Value;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum RepoTab {
    Collections,
    History,
    Backlinks,
}

pub(super) struct Inspector {
    pub(super) target: AtTarget,
    back: Vec<AtTarget>,
    forward: Vec<AtTarget>,
    tab: RepoTab,
    /// `listRecords` page keys for the current collection, in order.
    pages: Vec<String>,
    /// Pretty-printed record JSON, keyed by lookup key.
    record_lines: Option<(String, Arc<Vec<JsonLine>>)>,
}

impl Inspector {
    fn new(target: AtTarget) -> Self {
        Inspector { target, back: Vec::new(), forward: Vec::new(), tab: RepoTab::Collections, pages: Vec::new(), record_lines: None }
    }
}

fn key_describe(repo: &str) -> String {
    format!("describe:{repo}")
}
fn key_plc(repo: &str) -> String {
    format!("plc:{repo}")
}
fn key_links(uri: &str) -> String {
    format!("links:{uri}")
}
fn key_record(uri: &str) -> String {
    format!("record:{uri}")
}
fn key_page(uri: &str, cursor: &str) -> String {
    format!("list:{uri}|{cursor}")
}

impl Vortex {
    /// Open the inspector at `target` (pushing history if already open).
    pub(super) fn inspect(&mut self, target: AtTarget, cx: &mut Context<Self>) {
        match &mut self.inspector {
            Some(i) if i.target == target => {}
            Some(i) => {
                let prev = std::mem::replace(&mut i.target, target);
                i.back.push(prev);
                i.forward.clear();
                i.pages.clear();
                i.tab = RepoTab::Collections;
            }
            None => self.inspector = Some(Inspector::new(target)),
        }
        self.show_welcome = false;
        self.load_inspector(cx);
        cx.notify();
    }

    pub(super) fn inspect_back(&mut self, cx: &mut Context<Self>) {
        let Some(i) = &mut self.inspector else { return };
        if let Some(prev) = i.back.pop() {
            let cur = std::mem::replace(&mut i.target, prev);
            i.forward.push(cur);
            i.pages.clear();
            self.load_inspector(cx);
            cx.notify();
        }
    }

    pub(super) fn inspect_forward(&mut self, cx: &mut Context<Self>) {
        let Some(i) = &mut self.inspector else { return };
        if let Some(next) = i.forward.pop() {
            let cur = std::mem::replace(&mut i.target, next);
            i.back.push(cur);
            i.pages.clear();
            self.load_inspector(cx);
            cx.notify();
        }
    }

    pub(super) fn inspect_up(&mut self, cx: &mut Context<Self>) {
        if let Some(up) = self.inspector.as_ref().and_then(|i| i.target.up()) {
            self.inspect(up, cx);
        }
    }

    pub(super) fn close_inspector(&mut self, cx: &mut Context<Self>) {
        self.inspector = None;
        cx.notify();
    }

    #[cfg(debug_assertions)]
    pub(super) fn debug_inspect_tab(&mut self, tab: &str, cx: &mut Context<Self>) {
        match tab {
            "history" => self.set_repo_tab(RepoTab::History, cx),
            "backlinks" => self.set_repo_tab(RepoTab::Backlinks, cx),
            _ => {}
        }
    }

    fn set_repo_tab(&mut self, tab: RepoTab, cx: &mut Context<Self>) {
        if let Some(i) = &mut self.inspector {
            i.tab = tab;
        }
        self.load_inspector(cx);
        cx.notify();
    }

    /// Request whatever the current inspector page needs.
    fn load_inspector(&mut self, cx: &mut Context<Self>) {
        let Some(i) = &self.inspector else { return };
        let t = i.target.clone();
        let tab = i.tab;
        let need_first_page = i.pages.is_empty();
        let repo = t.repo.clone();
        self.request_identity(&repo, cx);
        match (&t.collection, &t.rkey) {
            (None, _) => {
                self.request_profile(&repo, cx);
                let r = repo.clone();
                self.request(key_describe(&repo), cx, move |c| {
                    let id = c.identity(&r)?;
                    c.describe_repo(id.pds.as_deref().ok_or("no PDS in DID document")?, &id.did)
                });
                match tab {
                    RepoTab::Collections => {}
                    RepoTab::History => {
                        let r = repo.clone();
                        self.request(key_plc(&repo), cx, move |c| {
                            let did = c.resolve(&r)?;
                            if !did.starts_with("did:plc:") {
                                return Err("PLC history exists only for did:plc".into());
                            }
                            c.plc_log(&did)
                        });
                    }
                    RepoTab::Backlinks => {
                        let r = repo.clone();
                        self.request(key_links(&t.uri()), cx, move |c| c.backlinks(&c.resolve(&r)?));
                    }
                }
            }
            (Some(coll), None) => {
                if need_first_page {
                    let key = key_page(&t.uri(), "");
                    if let Some(i) = &mut self.inspector {
                        i.pages.push(key.clone());
                    }
                    self.request_page(key, repo, coll.clone(), None, cx);
                }
            }
            (Some(coll), Some(rkey)) => {
                let (r, cl, rk) = (repo.clone(), coll.clone(), rkey.clone());
                self.request(key_record(&t.uri()), cx, move |c| {
                    let id = c.identity(&r)?;
                    c.get_record(id.pds.as_deref().ok_or("no PDS in DID document")?, &id.did, &cl, &rk)
                });
                let (r, cl, rk) = (repo.clone(), coll.clone(), rkey.clone());
                self.request(key_links(&t.uri()), cx, move |c| c.backlinks(&format!("at://{}/{cl}/{rk}", c.resolve(&r)?)));
                if coll == "app.bsky.feed.post" {
                    let (r, rk) = (repo, rkey.clone());
                    self.request(format!("postview:{}", t.uri()), cx, move |c| c.post(&format!("at://{}/app.bsky.feed.post/{rk}", c.resolve(&r)?)));
                }
            }
        }
    }

    fn request_page(&mut self, key: String, repo: String, coll: String, cursor: Option<String>, cx: &mut Context<Self>) {
        self.request(key, cx, move |c| {
            let id = c.identity(&repo)?;
            c.list_records(id.pds.as_deref().ok_or("no PDS in DID document")?, &id.did, &coll, cursor.as_deref())
        });
    }

    fn load_more(&mut self, cx: &mut Context<Self>) {
        let Some(i) = &self.inspector else { return };
        let Some(last) = i.pages.last() else { return };
        let Some(cursor) = self.lookup_ok(last).and_then(|v| v["cursor"].as_str()).map(str::to_owned) else { return };
        let t = i.target.clone();
        let key = key_page(&t.uri(), &cursor);
        if let Some(i) = &mut self.inspector {
            i.pages.push(key.clone());
        }
        if let Some(coll) = t.collection {
            self.request_page(key, t.repo, coll, Some(cursor), cx);
        }
        cx.notify();
    }

    /// Pre-render step (needs `&mut self`): pretty-print a loaded record once.
    pub(super) fn prepare_inspector(&mut self) {
        let Some(i) = &self.inspector else { return };
        if i.target.rkey.is_none() {
            return;
        }
        let key = key_record(&i.target.uri());
        if i.record_lines.as_ref().is_some_and(|(k, _)| *k == key) {
            return;
        }
        let Some(v) = self.lookup_ok(&key) else { return };
        let raw = serde_json::to_string(&v["value"]).unwrap_or_default();
        let lines = Arc::new(json_lines(&raw, 5000, 2000));
        if let Some(i) = &mut self.inspector {
            i.record_lines = Some((key, lines));
        }
    }

    /// The handle for a repo if resolved, else the repo string itself.
    fn repo_label(&self, repo: &str) -> String {
        self.lookup_ok(&format!("ident:{repo}"))
            .and_then(|v| v["handle"].as_str().map(str::to_owned))
            .unwrap_or_else(|| repo.to_owned())
    }

    fn repo_did(&self, repo: &str) -> Option<String> {
        if repo.starts_with("did:") {
            return Some(repo.to_owned());
        }
        self.lookup_ok(&format!("ident:{repo}")).and_then(|v| v["did"].as_str().map(str::to_owned))
    }

    pub(super) fn render_inspector(&self, t: &Theme, cx: &mut Context<Self>) -> Div {
        let Some(i) = &self.inspector else { return div() };
        let target = i.target.clone();
        let hover = t.bg_hover;

        // Navigation bar: history, breadcrumbs, actions.
        let nav_chip = |label: &'static str, enabled: bool, t: &Theme| {
            div()
                .flex_none()
                .px(t.pad)
                .h(t.row)
                .flex()
                .items_center()
                .rounded(t.radius)
                .text_color(if enabled { t.fg_dim } else { t.fg_faint })
                .when(enabled, |d| d.cursor_pointer().hover(move |s| s.bg(hover)))
                .child(label)
        };
        let mut crumbs = div().flex().flex_1().min_w_0().items_center().overflow_hidden().child(div().text_color(t.fg_faint).child("at://"));
        let repo_t = AtTarget::repo(target.repo.clone());
        crumbs = crumbs.child(
            div()
                .id("crumb-repo")
                .flex_none()
                .cursor_pointer()
                .text_color(t.blue)
                .hover(move |s| s.bg(hover))
                .child(self.repo_label(&target.repo))
                .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| this.inspect(repo_t.clone(), cx))),
        );
        if let Some(coll) = &target.collection {
            let coll_t = AtTarget { rkey: None, ..target.clone() };
            crumbs = crumbs.child(div().text_color(t.fg_faint).child("/")).child(
                div()
                    .id("crumb-coll")
                    .flex_none()
                    .cursor_pointer()
                    .text_color(t.hue(coll))
                    .hover(move |s| s.bg(hover))
                    .child(coll.clone())
                    .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| this.inspect(coll_t.clone(), cx))),
            );
        }
        if let Some(rkey) = &target.rkey {
            crumbs = crumbs.child(div().text_color(t.fg_faint).child("/")).child(div().flex_none().text_color(t.fg).child(rkey.clone()));
        }

        let uri = target.uri();
        let did = self.repo_did(&target.repo);
        let pdsls = format!("https://pdsls.dev/{}", did.as_ref().map(|d| uri.replacen(&target.repo, d, 1)).unwrap_or(uri.clone()));
        let filter_did = did.clone();
        let nav = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(t.pad)
            .px(t.pad)
            .h(t.row + t.pad)
            .bg(t.bg_alt)
            .border_b_1()
            .border_color(t.border)
            .child(
                nav_chip("←", !i.back.is_empty(), t)
                    .id("i-back")
                    .tooltip(tip("back", Some("⌘["), None))
                    .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| this.inspect_back(cx))),
            )
            .child(
                nav_chip("→", !i.forward.is_empty(), t)
                    .id("i-fwd")
                    .tooltip(tip("forward", Some("⌘]"), None))
                    .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| this.inspect_forward(cx))),
            )
            .child(
                nav_chip("↑", target.up().is_some(), t)
                    .id("i-up")
                    .tooltip(tip("up one level", Some("⌘↑"), None))
                    .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| this.inspect_up(cx))),
            )
            .child(crumbs)
            .when_some(filter_did, |d, did| {
                d.child(
                    self.chip("filter", false, t)
                        .id("i-filter")
                        .tooltip(tip("filter the stream to this account", None, None))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                this.set_query(&format!("did:{did}"), cx);
                                this.close_inspector(cx);
                            }),
                        ),
                )
            })
            .child(
                self.chip("pdsls", false, t)
                    .id("i-pdsls")
                    .tooltip(tip("open on pdsls.dev", None, None))
                    .on_mouse_down(MouseButton::Left, cx.listener(move |_, _, _, cx| cx.open_url(&pdsls))),
            )
            .child(
                self.chip("copy", false, t)
                    .id("i-copy")
                    .tooltip(tip("copy the AT-URI", None, None))
                    .on_mouse_down(MouseButton::Left, {
                        let uri = uri.clone();
                        cx.listener(move |_, _, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(uri.clone())))
                    }),
            )
            .child(
                self.chip("✕", false, t)
                    .id("i-close")
                    .tooltip(tip("back to the stream", Some("esc"), None))
                    .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| this.close_inspector(cx))),
            );

        let body = match (&target.collection, &target.rkey) {
            (None, _) => self.render_repo_page(i, t, cx),
            (Some(_), None) => self.render_collection_page(i, t, cx),
            (Some(_), Some(_)) => self.render_record_page(i, t, cx),
        };
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(nav)
            .child(div().id("inspect-body").flex().flex_col().flex_1().min_h_0().overflow_y_scroll().p(t.pad).gap(px(1.)).child(body))
    }

    fn status_line(&self, key: &str, what: &str, t: &Theme) -> Option<Div> {
        match self.lookup(key) {
            Some(Fetch::Done(Ok(_))) => None,
            Some(Fetch::Done(Err(e))) => Some(div().text_color(t.orange).child(format!("{what}: {e}"))),
            _ => Some(div().text_color(t.fg_faint).child(format!("loading {what}…"))),
        }
    }

    fn render_repo_page(&self, i: &Inspector, t: &Theme, cx: &mut Context<Self>) -> Div {
        let repo = &i.target.repo;
        let kv = |k: &'static str, v: String, color: Hsla, t: &Theme| {
            div().flex().gap(t.ch).child(div().flex_none().w(t.ch * 10.).text_color(t.fg_faint).child(k)).child(div().min_w_0().truncate().text_color(color).child(v))
        };
        let mut out = div().flex().flex_col().gap(px(1.));
        let ident_key = format!("ident:{repo}");
        if let Some(id) = self.lookup_ok(&ident_key) {
            out = out
                .child(kv("handle", id["handle"].as_str().map(|h| format!("@{h}")).unwrap_or("–".into()), t.blue, t))
                .child(kv("did", id["did"].as_str().unwrap_or("").to_owned(), t.fg, t))
                .child(kv("pds", id["pds"].as_str().unwrap_or("–").to_owned(), t.fg_dim, t))
                .child(kv(
                    "aka",
                    id["aka"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>().join("  "),
                    t.fg_dim,
                    t,
                ))
                .child(kv(
                    "key",
                    id["keys"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>().join("  "),
                    t.fg_faint,
                    t,
                ));
        } else if let Some(s) = self.status_line(&ident_key, "identity", t) {
            out = out.child(s);
        }
        if let Some(p) = self.lookup_ok(&format!("profile:{repo}")) {
            let n = |k: &str| fmt_count(p[k].as_u64().unwrap_or(0));
            out = out
                .child(kv("name", p["displayName"].as_str().unwrap_or("").to_owned(), t.fg, t))
                .child(kv(
                    "profile",
                    format!("{} followers · {} following · {} posts", n("followersCount"), n("followsCount"), n("postsCount")),
                    t.fg_dim,
                    t,
                ))
                .when_some(p["createdAt"].as_str(), |d, c| d.child(kv("created", c.to_owned(), t.fg_dim, t)))
                .when_some(p["description"].as_str().filter(|s| !s.is_empty()), |d, s| {
                    d.child(div().pl(t.ch * 11.).text_color(t.fg_dim).child(s.replace('\n', " ")))
                });
        }

        let describe = self.lookup_ok(&key_describe(repo));
        let collections: Vec<String> = describe
            .and_then(|v| v["collections"].as_array())
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
            .unwrap_or_default();
        let tab_chip = |label: String, tab: RepoTab, about: &'static str, t: &Theme, cx: &mut Context<Self>| {
            self.chip(label, i.tab == tab, t)
                .id(about)
                .tooltip(tip(about, None, None))
                .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| this.set_repo_tab(tab, cx)))
        };
        out = out.child(
            div()
                .flex()
                .gap(t.pad)
                .pt(t.pad)
                .pb(t.pad * 0.5)
                .child(tab_chip(format!("collections {}", collections.len()), RepoTab::Collections, "record types in this repo", t, cx))
                .child(tab_chip("history".into(), RepoTab::History, "PLC operations: handle, PDS and key changes", t, cx))
                .child(tab_chip("backlinks".into(), RepoTab::Backlinks, "records elsewhere that reference this account (constellation)", t, cx)),
        );

        match i.tab {
            RepoTab::Collections => {
                if let Some(s) = self.status_line(&key_describe(repo), "collections", t) {
                    out = out.child(s);
                }
                for (n, coll) in collections.into_iter().enumerate() {
                    let target = AtTarget { repo: repo.clone(), collection: Some(coll.clone()), rkey: None };
                    out = out.child(self.link_row(("coll", n), div().text_color(t.hue(&coll)).child(coll.clone()), target, t, cx));
                }
            }
            RepoTab::History => {
                if let Some(s) = self.status_line(&key_plc(repo), "history", t) {
                    out = out.child(s);
                }
                if let Some(log) = self.lookup_ok(&key_plc(repo)).and_then(|v| v.as_array().cloned()) {
                    let mut prev: Option<(String, String)> = None;
                    for entry in log.iter() {
                        let op = &entry["operation"];
                        let when = entry["createdAt"].as_str().unwrap_or("").replace('T', " ");
                        let handle = op["alsoKnownAs"][0].as_str().or(op["handle"].as_str()).unwrap_or("").trim_start_matches("at://").to_owned();
                        let pds = op["services"]["atproto_pds"]["endpoint"].as_str().or(op["service"].as_str()).unwrap_or("").to_owned();
                        let mut what = Vec::new();
                        match &prev {
                            None => what.push(format!("created as @{handle} on {pds}")),
                            Some((h, p)) => {
                                if *h != handle {
                                    what.push(format!("handle @{h} → @{handle}"));
                                }
                                if *p != pds {
                                    what.push(format!("pds {p} → {pds}"));
                                }
                                if what.is_empty() {
                                    what.push("keys or services updated".into());
                                }
                            }
                        }
                        if op["type"].as_str() == Some("plc_tombstone") {
                            what = vec!["tombstoned".into()];
                        }
                        let nullified = entry["nullified"].as_bool().unwrap_or(false);
                        out = out.child(
                            div()
                                .flex()
                                .gap(t.ch)
                                .child(div().flex_none().w(t.ch * 24.).text_color(t.fg_faint).child(when[..when.len().min(23)].to_owned()))
                                .child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .text_color(if nullified { t.fg_faint } else { t.fg })
                                        .child(format!("{}{}", what.join(" · "), if nullified { "  (nullified)" } else { "" })),
                                ),
                        );
                        prev = Some((handle, pds));
                    }
                }
            }
            RepoTab::Backlinks => {
                let key = key_links(&i.target.uri());
                if let Some(s) = self.status_line(&key, "backlinks", t) {
                    out = out.child(s);
                }
                out = out.children(self.backlink_rows(&key, t));
            }
        }
        out
    }

    fn backlink_rows(&self, key: &str, t: &Theme) -> Vec<Div> {
        let Some(v) = self.lookup_ok(key) else { return Vec::new() };
        let rows = backlink_rows(v);
        if rows.is_empty() {
            return vec![div().text_color(t.fg_faint).child("no backlinks")];
        }
        rows.into_iter()
            .take(200)
            .map(|(coll, path, n)| {
                div()
                    .flex()
                    .gap(t.ch)
                    .child(div().flex_none().w(t.ch * 8.).flex().justify_end().text_color(t.accent).child(fmt_count(n)))
                    .child(div().flex_none().text_color(t.hue(&coll)).child(coll.clone()))
                    .child(div().min_w_0().truncate().text_color(t.fg_faint).child(path))
            })
            .collect()
    }

    fn render_collection_page(&self, i: &Inspector, t: &Theme, cx: &mut Context<Self>) -> Div {
        let mut out = div().flex().flex_col().gap(px(1.));
        let mut total = 0usize;
        let mut n = 0usize;
        for key in &i.pages {
            if let Some(s) = self.status_line(key, "records", t) {
                out = out.child(s);
                continue;
            }
            let Some(page) = self.lookup_ok(key) else { continue };
            for rec in page["records"].as_array().into_iter().flatten() {
                total += 1;
                let uri = rec["uri"].as_str().unwrap_or("");
                let Some(target) = parse_target(uri) else { continue };
                let rkey = target.rkey.clone().unwrap_or_default();
                let value = &rec["value"];
                let when = value["createdAt"].as_str().unwrap_or("").replace('T', " ");
                let preview = record_preview(value);
                let row = div()
                    .flex()
                    .gap(t.ch)
                    .min_w_0()
                    .child(div().flex_none().w(t.ch * 15.).text_color(t.fg_dim).child(rkey))
                    .child(div().flex_none().w(t.ch * 17.).text_color(t.fg_faint).child(when[..when.len().min(16)].to_owned()))
                    .child(div().flex_1().min_w_0().truncate().text_color(t.fg).child(preview));
                out = out.child(self.link_row(("rec", n), row, target, t, cx));
                n += 1;
            }
        }
        let more = i.pages.last().and_then(|k| self.lookup_ok(k)).and_then(|v| v["cursor"].as_str()).is_some();
        out = out.child(
            div()
                .flex()
                .gap(t.ch)
                .pt(t.pad)
                .text_color(t.fg_faint)
                .child(format!("{total} records"))
                .when(more, |d| {
                    d.child(
                        self.chip("load 100 more", false, t)
                            .id("i-more")
                            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| this.load_more(cx))),
                    )
                }),
        );
        out
    }

    fn render_record_page(&self, i: &Inspector, t: &Theme, cx: &mut Context<Self>) -> Div {
        let uri = i.target.uri();
        let mut out = div().flex().flex_col().gap(px(1.));
        let key = key_record(&uri);
        if let Some(s) = self.status_line(&key, "record", t) {
            out = out.child(s);
        }
        if let Some(v) = self.lookup_ok(&key) {
            out = out.child(div().text_color(t.fg_faint).truncate().child(format!("cid {}", v["cid"].as_str().unwrap_or("–"))));
        }
        if let Some(p) = self.lookup_ok(&format!("postview:{uri}")) {
            let n = |k: &str| fmt_count(p[k].as_u64().unwrap_or(0));
            out = out.child(div().text_color(t.fg_dim).child(format!(
                "♥ {}  ⟲ {}  ↩ {}  ❝ {}",
                n("likeCount"),
                n("repostCount"),
                n("replyCount"),
                n("quoteCount")
            )));
        }
        // JSON, with references clickable.
        if let Some((_, lines)) = &i.record_lines {
            out = out.child(div().h(t.pad));
            for (n, line) in lines.iter().enumerate() {
                let highlights = line.spans.iter().map(|(r, tok)| (r.clone(), tok.style(t)));
                let text = StyledText::new(line.text.clone()).with_highlights(highlights);
                let row = div().whitespace_nowrap().child(text);
                out = out.child(match link_in_line(&line.text) {
                    Some(target) => self.link_row(("json", n), row, target, t, cx),
                    None => row,
                });
            }
        }
        let links = key_links(&uri);
        out = out.child(div().pt(t.pad).text_color(t.fg_faint).child("backlinks"));
        if let Some(s) = self.status_line(&links, "backlinks", t) {
            out = out.child(s);
        }
        out.children(self.backlink_rows(&links, t))
    }

    /// A clickable row that navigates the inspector.
    fn link_row(&self, id: impl Into<ElementId>, content: Div, target: AtTarget, t: &Theme, cx: &mut Context<Self>) -> Div {
        let hover = t.bg_hover;
        div().child(
            content
                .id(id)
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| this.inspect(target.clone(), cx))),
        )
    }
}

/// A short human-readable summary of a record value.
fn record_preview(v: &Value) -> String {
    for k in ["text", "displayName", "name", "description"] {
        if let Some(s) = v[k].as_str().filter(|s| !s.is_empty()) {
            return s.replace('\n', " ");
        }
    }
    if let Some(s) = v["subject"]["uri"].as_str().or(v["subject"].as_str()) {
        return format!("→ {s}");
    }
    v["$type"].as_str().unwrap_or("").to_owned()
}

/// The first `at://` URI or DID quoted in a JSON line.
fn link_in_line(line: &str) -> Option<AtTarget> {
    for needle in ["\"at://", "\"did:"] {
        if let Some(start) = line.find(needle) {
            let rest = &line[start + 1..];
            let end = rest.find('"')?;
            return parse_target(&rest[..end]);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_in_json_lines() {
        assert_eq!(
            link_in_line(r#"    "uri": "at://did:plc:abc12345/app.bsky.feed.post/3k","#).map(|t| t.uri()).as_deref(),
            Some("at://did:plc:abc12345/app.bsky.feed.post/3k")
        );
        assert_eq!(link_in_line(r#"  "subject": "did:plc:abc12345""#).map(|t| t.uri()).as_deref(), Some("at://did:plc:abc12345"));
        assert!(link_in_line(r#"  "text": "hello""#).is_none());
        let v: Value = serde_json::from_str(r#"{"subject":{"uri":"at://x/y/z"}}"#).unwrap();
        assert_eq!(record_preview(&v), "→ at://x/y/z");
    }
}
