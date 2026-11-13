//! Connection manager: one async task that owns the socket.
//!
//! It only does I/O — frames are forwarded untouched to the pipeline — and
//! keeps the stream alive: reconnect with exponential backoff and jitter,
//! resume from the last processed cursor, fail over between hosts, and
//! re-subscribe (gaplessly, via the cursor) when the server-side filter
//! changes.

use super::http;
use super::{build_request, ConnState, SourceConfig, SourceKind, StartFrom, StatusCell};
use crate::decode::zstd_dict::Dictionaries;
use crate::filter::Pushdown;
use crate::json::Scanner;
use crate::pipeline::{FrameMsg, Msg};
use crate::stats::{Log, Stats};
use crate::time::now_us;
use bytes::Bytes;
use futures_util::StreamExt;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{connect_async_tls_with_config, MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Jetstream pings every 30 s; silence well past that means a dead link.
const IDLE_TIMEOUT: Duration = Duration::from_secs(75);
/// Quiet period before a changed server filter triggers a re-subscribe, so
/// typing a query does not cause a reconnect per keystroke.
const PUSHDOWN_DEBOUNCE: Duration = Duration::from_millis(600);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// A connection that lasted this long resets the failure streak.
const STABLE_AFTER: Duration = Duration::from_secs(10);

pub(crate) enum Cmd {
    Connect(SourceConfig),
    Disconnect,
    Pushdown(Pushdown),
    Shutdown,
}

pub(crate) struct Ctx {
    pub tx: mpsc::Sender<Msg>,
    pub stats: Arc<Stats>,
    pub log: Arc<Log>,
    pub dicts: Arc<Dictionaries>,
    pub status: Arc<StatusCell>,
}

enum ConnectError {
    /// The server refused the request with a structured XRPC error.
    Rejected { name: String, message: String },
    Other(String),
}

enum PumpEnd {
    Cmd(Cmd),
    Lost(String),
}

struct Manager {
    ctx: Ctx,
    pushdown: Pushdown,
    host_idx: usize,
    /// Host of the most recent live connection (its cursors are comparable).
    last_live_host: Option<String>,
}

pub(crate) async fn run(mut cmds: mpsc::UnboundedReceiver<Cmd>, ctx: Ctx) {
    let mut m = Manager { ctx, pushdown: Pushdown::default(), host_idx: 0, last_live_host: None };
    let mut source: Option<SourceConfig> = None;
    let mut fresh = true;
    loop {
        let cmd = match &source {
            None => match cmds.recv().await {
                Some(c) => c,
                None => return,
            },
            Some(cfg) => m.session(cfg, fresh, &mut cmds).await,
        };
        match cmd {
            Cmd::Connect(cfg) => {
                m.host_idx = 0;
                m.last_live_host = None;
                m.ctx.stats.reset_position();
                source = Some(cfg);
                fresh = true;
            }
            Cmd::Disconnect => {
                source = None;
                m.ctx.status.update(|s| {
                    s.state = ConnState::Idle;
                    s.error = None;
                    s.server_filter.clear();
                });
                m.ctx.log.info("disconnected");
            }
            Cmd::Pushdown(p) => {
                m.pushdown = p;
                fresh = false;
            }
            Cmd::Shutdown => return,
        }
    }
}

fn backoff(failures: u32) -> Duration {
    let base = Duration::from_millis(250).saturating_mul(1u32 << failures.min(8));
    let capped = base.min(MAX_BACKOFF);
    capped.mul_f64(0.5 + fastrand::f64() * 0.5)
}

async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d.into()).await,
        None => std::future::pending().await,
    }
}

/// The part of `p` the server would actually be asked to apply.
fn effective(kind: SourceKind, p: &Pushdown, enabled: bool) -> Pushdown {
    if !enabled {
        return Pushdown::default();
    }
    build_request(kind, "h", None, p, None).applied
}

fn parse_xrpc_error(body: &[u8]) -> Option<(String, String)> {
    let text = std::str::from_utf8(body).ok()?;
    let mut s = Scanner::new(text);
    if !s.object().ok()? {
        return None;
    }
    let (mut name, mut message) = (String::new(), String::new());
    while let Some(k) = s.key().ok()? {
        match k {
            "error" => {
                if let Some(v) = s.str().ok()? {
                    v.push_to(&mut name);
                }
            }
            "message" => {
                if let Some(v) = s.str().ok()? {
                    v.push_to(&mut message);
                }
            }
            _ => s.skip().ok()?,
        }
    }
    (!name.is_empty()).then_some((name, message))
}

async fn connect(url: &str, kind: SourceKind) -> Result<Ws, ConnectError> {
    http::ensure_crypto();
    let other = |e: String| ConnectError::Other(e);
    let mut req = url.into_client_request().map_err(|e| other(e.to_string()))?;
    req.headers_mut().insert("User-Agent", HeaderValue::from_static("vortex"));
    if kind == SourceKind::JetstreamV2 {
        req.headers_mut().insert("Sec-WebSocket-Protocol", HeaderValue::from_static("xrpc.v1.json"));
    }
    let cfg = WebSocketConfig::default()
        .read_buffer_size(1 << 20)
        .max_message_size(Some(64 << 20))
        .max_frame_size(Some(64 << 20));
    match timeout(CONNECT_TIMEOUT, connect_async_tls_with_config(req, Some(cfg), true, None)).await {
        Err(_) => Err(other("connect timed out".into())),
        Ok(Ok((ws, _))) => Ok(ws),
        Ok(Err(WsError::Http(resp))) => {
            let status = resp.status();
            match resp.body().as_deref().and_then(parse_xrpc_error) {
                Some((name, message)) => Err(ConnectError::Rejected { name, message }),
                None => Err(other(format!("HTTP {status}"))),
            }
        }
        Ok(Err(e)) => Err(other(e.to_string())),
    }
}

impl Manager {
    fn start_cursor(&self, cfg: &SourceConfig) -> Option<i64> {
        match cfg.start {
            StartFrom::Live => None,
            StartFrom::Cursor(c) => Some(c),
            StartFrom::Rewind(d) => {
                if !cfg.kind.supports_rewind() {
                    self.ctx.log.error(format!("{} cannot rewind by time; starting live", cfg.kind.label()));
                    None
                } else {
                    // Jetstream reads cursors >= 1e15 as unix microseconds.
                    Some(now_us() - d.as_micros() as i64)
                }
            }
        }
    }

    /// Where to resume after a drop: `(cursor, comparable_with_previous)`.
    fn resume_cursor(&self, kind: SourceKind, same_host: bool) -> Option<(i64, bool)> {
        let l = Ordering::Relaxed;
        let seq = self.ctx.stats.last_seq.load(l);
        let time = self.ctx.stats.last_time_us.load(l);
        let witnessed = self.ctx.stats.last_witnessed_us.load(l);
        match kind {
            // seq is exact but per-instance; the witnessed timestamp is the
            // portable position when failing over to another host.
            SourceKind::JetstreamV2 if same_host && seq > 0 => Some((seq, true)),
            SourceKind::JetstreamV2 if witnessed > 0 => Some((witnessed, false)),
            SourceKind::JetstreamV2 if time > 0 => Some((time, false)),
            SourceKind::JetstreamV1 if time > 0 => Some((time, true)),
            SourceKind::Firehose if same_host && seq > 0 => Some((seq, true)),
            SourceKind::Plc if seq > 0 => Some((seq, true)),
            _ => None,
        }
    }

    /// Run connections for `cfg` until a command needs the session restarted.
    async fn session(&mut self, cfg: &SourceConfig, fresh: bool, cmds: &mut mpsc::UnboundedReceiver<Cmd>) -> Cmd {
        let kind = cfg.kind;
        let mut first = true;
        let mut failures = 0u32;
        let mut use_pushdown = kind.supports_pushdown();
        let mut compress = cfg.compress && kind.supports_compression();
        let mut skip_resume = false;
        let mut dict_fetches = 0;
        let mut pending: Option<(Pushdown, Instant)> = None;

        loop {
            let Some(host) = cfg.hosts.get(self.host_idx % cfg.hosts.len().max(1)).cloned() else {
                self.ctx.status.update(|s| {
                    s.state = ConnState::Waiting;
                    s.error = Some("no hosts configured".into());
                });
                return cmds.recv().await.unwrap_or(Cmd::Shutdown);
            };
            let same_host = self.last_live_host.as_deref() == Some(host.as_str());
            let (cursor, fresh_space) = if first && fresh {
                (self.start_cursor(cfg), true)
            } else if skip_resume {
                (None, true)
            } else {
                match self.resume_cursor(kind, same_host) {
                    Some((c, comparable)) => (Some(c), !comparable),
                    None => (self.start_cursor(cfg), true),
                }
            };

            let none = Pushdown::default();
            let pushdown = if use_pushdown { &self.pushdown } else { &none };
            let dict_id = compress.then(|| if kind == SourceKind::JetstreamV2 { self.ctx.dicts.v2_id() } else { 1 });
            let req = build_request(kind, &host, cursor, pushdown, dict_id);
            if use_pushdown && req.applied.dids.is_none() && pushdown.dids.is_some() {
                self.ctx.log.info("DID list too long for the server filter; filtering DIDs locally");
            }
            self.ctx.status.update(|s| {
                s.state = ConnState::Connecting;
                s.host = host.clone();
                s.attempt = failures;
            });

            let error = match connect(&req.url, kind).await {
                Ok(ws) => {
                    let how = match cursor {
                        Some(c) => format!(" from cursor {c}"),
                        None => " live".into(),
                    };
                    self.ctx.log.info(format!("connected to {host}{how}"));
                    self.ctx.stats.connects.fetch_add(1, Ordering::Relaxed);
                    self.ctx.status.update(|s| {
                        s.state = ConnState::Live;
                        s.error = None;
                        s.server_filter = req.applied.summary();
                    });
                    self.last_live_host = Some(host.clone());
                    if self.ctx.tx.send(Msg::Connected { fresh_cursor_space: fresh_space }).await.is_err() {
                        return Cmd::Shutdown;
                    }
                    first = false;
                    skip_resume = false;
                    let started = Instant::now();
                    match self.pump(ws, kind, &req.applied, use_pushdown, cmds, &mut pending).await {
                        PumpEnd::Cmd(c) => return c,
                        PumpEnd::Lost(reason) => {
                            if started.elapsed() >= STABLE_AFTER {
                                failures = 0;
                            }
                            reason
                        }
                    }
                }
                Err(ConnectError::Rejected { name, message }) => match name.as_str() {
                    // Our dictionary was retired: fetch the current one and retry.
                    "UnknownZstdDictionary" if dict_fetches < 2 => {
                        dict_fetches += 1;
                        match http::get(&host, "/xrpc/network.bsky.jetstream.getZstdDictionary").await {
                            Ok((200, bytes)) => match self.ctx.dicts.set_v2(bytes) {
                                Ok(id) => self.ctx.log.info(format!("fetched zstd dictionary {id}")),
                                Err(e) => {
                                    self.ctx.log.error(format!("bad dictionary from {host}: {e}; continuing uncompressed"));
                                    compress = false;
                                }
                            },
                            other => {
                                let why = match other {
                                    Ok((status, _)) => format!("HTTP {status}"),
                                    Err(e) => e.to_string(),
                                };
                                self.ctx.log.error(format!("dictionary fetch failed ({why}); continuing uncompressed"));
                                compress = false;
                            }
                        }
                        continue;
                    }
                    "UnknownZstdDictionary" => {
                        compress = false;
                        continue;
                    }
                    "CursorTooOld" if !skip_resume => {
                        self.ctx.log.error(format!("cursor too old ({message}); starting live"));
                        skip_resume = true;
                        continue;
                    }
                    _ if use_pushdown && !req.applied.is_empty() => {
                        self.ctx.log.error(format!("server rejected the filter ({message}); filtering locally"));
                        use_pushdown = false;
                        continue;
                    }
                    _ => format!("{name}: {message}"),
                },
                Err(ConnectError::Other(e)) => e,
            };

            failures += 1;
            if failures % 2 == 0 && cfg.hosts.len() > 1 {
                self.host_idx += 1;
            }
            let delay = backoff(failures);
            self.ctx.log.error(format!("{host}: {error}; retrying in {:.1}s", delay.as_secs_f64()));
            self.ctx.status.update(|s| {
                s.state = ConnState::Waiting;
                s.error = Some(error.clone());
                s.attempt = failures;
            });
            tokio::select! {
                cmd = cmds.recv() => return cmd.unwrap_or(Cmd::Shutdown),
                _ = tokio::time::sleep(delay) => {}
            }
        }
    }

    /// Forward frames until the connection drops or a command interrupts.
    async fn pump(
        &mut self,
        mut ws: Ws,
        kind: SourceKind,
        applied: &Pushdown,
        use_pushdown: bool,
        cmds: &mut mpsc::UnboundedReceiver<Cmd>,
        pending: &mut Option<(Pushdown, Instant)>,
    ) -> PumpEnd {
        let wire = kind.wire();
        loop {
            let deadline = pending.as_ref().map(|(_, at)| *at);
            tokio::select! {
                biased;
                cmd = cmds.recv() => match cmd {
                    None => return PumpEnd::Cmd(Cmd::Shutdown),
                    Some(Cmd::Pushdown(p)) => *pending = Some((p, Instant::now() + PUSHDOWN_DEBOUNCE)),
                    Some(other) => {
                        let _ = timeout(Duration::from_secs(1), ws.close(None)).await;
                        return PumpEnd::Cmd(other);
                    }
                },
                _ = sleep_until_opt(deadline), if deadline.is_some() => {
                    let Some((p, _)) = pending.take() else { continue };
                    if effective(kind, &p, use_pushdown) != *applied {
                        // Re-subscribe with the new filter; the cursor makes it gapless.
                        let _ = timeout(Duration::from_secs(1), ws.close(None)).await;
                        return PumpEnd::Cmd(Cmd::Pushdown(p));
                    }
                    self.pushdown = p;
                }
                msg = timeout(IDLE_TIMEOUT, ws.next()) => {
                    let (payload, is_text): (Bytes, bool) = match msg {
                        Err(_) => return PumpEnd::Lost("connection idle too long".into()),
                        Ok(None) => return PumpEnd::Lost("connection closed".into()),
                        Ok(Some(Err(e))) => return PumpEnd::Lost(e.to_string()),
                        Ok(Some(Ok(Message::Binary(b)))) => (b, false),
                        Ok(Some(Ok(Message::Text(t)))) => (t.into(), true),
                        Ok(Some(Ok(Message::Close(frame)))) => {
                            let reason = frame.map(|f| format!("{} {}", f.code, f.reason)).unwrap_or_default();
                            return PumpEnd::Lost(format!("server closed the connection {reason}").trim().to_owned());
                        }
                        Ok(Some(Ok(_))) => continue,
                    };
                    if self.ctx.tx.send(Msg::Frame(FrameMsg { wire, is_text, payload })).await.is_err() {
                        return PumpEnd::Cmd(Cmd::Shutdown);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        for f in 0..20 {
            let d = backoff(f);
            assert!(d <= MAX_BACKOFF, "{f}: {d:?}");
            assert!(d >= Duration::from_millis(125));
        }
        assert!(backoff(10) >= MAX_BACKOFF / 2);
    }

    #[test]
    fn parses_xrpc_errors() {
        let (name, msg) = parse_xrpc_error(br#"{"error":"UnknownZstdDictionary","message":"current dictionary id is 5"}"#).unwrap();
        assert_eq!((name.as_str(), msg.as_str()), ("UnknownZstdDictionary", "current dictionary id is 5"));
        assert!(parse_xrpc_error(b"<html>nope</html>").is_none());
        assert!(parse_xrpc_error(br#"{"message":"no name"}"#).is_none());
        assert!(parse_xrpc_error(&[0xff, 0xfe]).is_none());
    }

    #[test]
    fn effective_pushdown_respects_capabilities() {
        let p = Pushdown { kinds: None, collections: Some(vec!["app.bsky.feed.post".into()]), dids: None };
        assert_eq!(effective(SourceKind::JetstreamV2, &p, true), p);
        assert!(effective(SourceKind::JetstreamV2, &p, false).is_empty());
        assert!(effective(SourceKind::Firehose, &p, true).is_empty());
    }
}
