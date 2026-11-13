//! Offline benchmark of the relay firehose (DAG-CBOR + CAR) decode path.
//!
//! Live traffic is not repeatable, so frames are first captured to a file and
//! every measurement replays that file from memory:
//!
//! ```text
//! vortex-bench-firehose capture FILE [--frames 200000] [--rewind 300000] [--host HOST]
//! vortex-bench-firehose bench FILE [--runs 7] [--skip N] [--limit N]
//! ```
//!
//! The capture format is a sequence of `u32 LE length || frame bytes`.
//! `--skip`/`--limit` replay a slice of the file; a couple of thousand frames
//! fit in cache and show CPU cost, the whole file adds the memory traffic of
//! cold frames (a CAR walk is a chain of dependent loads).

use futures_util::StreamExt;
use std::io::{Read, Write};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use vortex_core::decode::car::Car;
use vortex_core::decode::cbor::{cid_to_string, Reader};
use vortex_core::decode::firehose::decode_firehose_frame;
use vortex_core::decode::frame::decode_json_frame;
use vortex_core::decode::Decoded;
use vortex_core::event::{Event, EventBuilder};

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("vortex-bench-firehose: {msg}");
    std::process::exit(2);
}

fn best_of<T>(runs: usize, mut f: impl FnMut() -> T) -> (Duration, T) {
    let mut best = Duration::MAX;
    let mut out = None;
    for _ in 0..runs {
        let t = Instant::now();
        let r = f();
        best = best.min(t.elapsed());
        out = Some(r);
    }
    (best, out.unwrap())
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(host: &str, cursor: Option<i64>) -> Ws {
    let mut url = format!("wss://{host}/xrpc/com.atproto.sync.subscribeRepos");
    if let Some(c) = cursor {
        url.push_str(&format!("?cursor={c}"));
    }
    let mut req = url.into_client_request().unwrap_or_else(|e| die(e));
    req.headers_mut().insert("User-Agent", HeaderValue::from_static("vortex"));
    let cfg = WebSocketConfig::default()
        .read_buffer_size(1 << 20)
        .max_message_size(Some(64 << 20))
        .max_frame_size(Some(64 << 20));
    match tokio_tungstenite::connect_async_tls_with_config(req, Some(cfg), true, None).await {
        Ok((ws, _)) => ws,
        Err(e) => die(format!("connect: {e}")),
    }
}

/// `seq` of a commit/identity/account frame, read straight from the CBOR.
fn frame_seq(frame: &[u8]) -> Option<i64> {
    let mut r = Reader::new(frame);
    r.skip().ok()?;
    let n = r.map().ok()??;
    for _ in 0..n {
        if r.key().ok()? == "seq" {
            return r.int().ok()?;
        }
        r.skip().ok()?;
    }
    None
}

async fn capture(path: &str, host: &str, want: usize, rewind: i64) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    // The firehose cursor is a sequence number, so learn the live one first.
    let mut ws = connect(host, None).await;
    let live = loop {
        match ws.next().await {
            Some(Ok(Message::Binary(b))) => {
                if let Some(s) = frame_seq(&b) {
                    break s;
                }
            }
            Some(Ok(_)) => {}
            other => die(format!("no live frame: {other:?}")),
        }
    };
    drop(ws);
    eprintln!("live seq {live}; replaying from {}", live - rewind);
    let mut ws = connect(host, Some(live - rewind)).await;
    let mut file = std::io::BufWriter::new(std::fs::File::create(path).unwrap_or_else(|e| die(e)));
    let (mut n, mut bytes) = (0usize, 0usize);
    let started = Instant::now();
    while n < want {
        match ws.next().await {
            Some(Ok(Message::Binary(b))) => {
                file.write_all(&(b.len() as u32).to_le_bytes()).unwrap();
                file.write_all(&b).unwrap();
                n += 1;
                bytes += b.len();
            }
            Some(Ok(_)) => {}
            other => {
                eprintln!("stream ended: {other:?}");
                break;
            }
        }
    }
    file.flush().unwrap();
    eprintln!("captured {n} frames, {:.1} MB in {:.1?}", bytes as f64 / 1e6, started.elapsed());
}

fn load(path: &str) -> Vec<Vec<u8>> {
    let mut data = Vec::new();
    std::fs::File::open(path).unwrap_or_else(|e| die(format!("{path}: {e}"))).read_to_end(&mut data).unwrap();
    let mut frames = Vec::new();
    let mut pos = 0;
    while pos + 4 <= data.len() {
        let len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        if pos + len > data.len() {
            break;
        }
        frames.push(data[pos..pos + len].to_vec());
        pos += len;
    }
    frames
}

/// What the commit decoder needs from a frame, re-derived with the public
/// reader so stages can be timed in isolation.
struct Commit<'a> {
    blocks: &'a [u8],
    cids: Vec<&'a [u8]>,
}

fn commit_envelope(frame: &[u8]) -> Option<Commit<'_>> {
    let mut r = Reader::new(frame);
    let n = r.map().ok()??;
    let mut t = "";
    for _ in 0..n {
        match r.key().ok()? {
            "t" => t = r.str().ok()?.unwrap_or(""),
            _ => r.skip().ok()?,
        }
    }
    if t != "#commit" {
        return None;
    }
    let n = r.map().ok()??;
    let mut c = Commit { blocks: &[], cids: Vec::new() };
    for _ in 0..n {
        match r.key().ok()? {
            "blocks" => c.blocks = r.bytes().ok()?.unwrap_or(&[]),
            "ops" => {
                let count = r.array().ok()??;
                for _ in 0..count {
                    let fields = r.map().ok()??;
                    for _ in 0..fields {
                        match r.key().ok()? {
                            "cid" => {
                                if let Some(cid) = r.cid().ok()? {
                                    c.cids.push(cid);
                                }
                            }
                            _ => r.skip().ok()?,
                        }
                    }
                }
            }
            _ => r.skip().ok()?,
        }
    }
    Some(c)
}

fn bench(path: &str, runs: usize, skip: usize, limit: usize, only_total: bool) {
    let mut frames = load(path);
    frames.drain(..skip.min(frames.len()));
    frames.truncate(limit);
    frames.shrink_to_fit();
    let n_frames = frames.len();
    assert!(n_frames > 0, "empty capture");
    let wire_bytes: usize = frames.iter().map(Vec::len).sum();

    // End to end, exactly as the pipeline calls it.
    let mut b = EventBuilder::new();
    let mut json = String::with_capacity(16 * 1024);
    let (d_total, (decoded, errors)) = best_of(runs, || {
        let mut out: Vec<Decoded> = Vec::with_capacity(n_frames * 2);
        let mut errors = 0usize;
        for f in &frames {
            if decode_firehose_frame(f, &mut b, &mut json, &mut out).is_err() {
                errors += 1;
            }
        }
        (out, errors)
    });
    if only_total {
        println!("{:.0} ns/frame", d_total.as_nanos() as f64 / n_frames as f64);
        return;
    }
    let events: Vec<Event> = decoded
        .into_iter()
        .filter_map(|d| match d {
            Decoded::Event(e) => Some(e),
            Decoded::Notice(_) => None,
        })
        .collect();
    let n = events.len();
    let json_bytes: usize = events.iter().map(|e| e.raw().len()).sum();
    let rec_bytes: usize = events.iter().filter_map(|e| e.record_json()).map(str::len).sum();
    println!(
        "corpus: {n_frames} frames ({:.1} MB, {} B/frame) -> {n} events ({:.1} MB JSON, {} B/event, {} B record), {errors} frame errors",
        wire_bytes as f64 / 1e6,
        wire_bytes / n_frames,
        json_bytes as f64 / 1e6,
        json_bytes / n.max(1),
        rec_bytes / n.max(1),
    );
    let per = |d: Duration| d.as_nanos() as f64 / n as f64;
    let line = |name: &str, d: Duration| {
        println!(
            "{name:<34} {:>7.0} ns/event  {:>6.2} M events/s  {:>5.1}%",
            per(d),
            n as f64 / d.as_secs_f64() / 1e6,
            100.0 * d.as_secs_f64() / d_total.as_secs_f64()
        );
    };
    line("firehose decode, end to end", d_total);
    println!(
        "{:<34} {:>7.0} MB/s wire, {:.0} MB/s JSON out",
        "",
        wire_bytes as f64 / 1e6 / d_total.as_secs_f64(),
        json_bytes as f64 / 1e6 / d_total.as_secs_f64()
    );

    println!("\nstages in isolation (they overlap slightly; percentages are of end to end):");

    // 1. Envelope: header + payload map + ops, skipping everything else.
    let (d, commits) = best_of(runs, || frames.iter().filter_map(|f| commit_envelope(f)).collect::<Vec<_>>());
    line("cbor envelope walk", d);

    // 2. CAR index.
    let (d, cars) = best_of(runs, || commits.iter().map(|c| Car::parse(c.blocks).ok()).collect::<Vec<_>>());
    line("car index", d);

    // 3. Block lookup by CID.
    let (d, blocks) = best_of(runs, || {
        let mut blocks: Vec<&[u8]> = Vec::with_capacity(n);
        for (c, car) in commits.iter().zip(&cars) {
            let Some(car) = car else { continue };
            for cid in &c.cids {
                if let Some(b) = car.get(cid) {
                    blocks.push(b);
                }
            }
        }
        blocks
    });
    line("car cid lookup", d);
    let block_bytes: usize = blocks.iter().map(|b| b.len()).sum();

    // 4. CID -> base32 string.
    let (d, _) = best_of(runs, || {
        let mut total = 0usize;
        for c in &commits {
            for cid in &c.cids {
                json.clear();
                cid_to_string(cid, &mut json);
                total += json.len();
            }
        }
        total
    });
    line("op cid -> base32", d);

    // 5. Record CBOR -> JSON.
    let (d, out_bytes) = best_of(runs, || {
        let mut total = 0usize;
        for blk in &blocks {
            json.clear();
            let _ = Reader::new(blk).to_json(&mut json);
            total += json.len();
        }
        total
    });
    line("record cbor -> json transcode", d);
    println!(
        "{:<34} {} records, {} B cbor avg -> {} B json avg, {:.0} MB/s in",
        "",
        blocks.len(),
        block_bytes / blocks.len().max(1),
        out_bytes / blocks.len().max(1),
        block_bytes as f64 / 1e6 / d.as_secs_f64()
    );

    // 5b. The same walk without producing output (reader only).
    let (d, _) = best_of(runs, || {
        for blk in &blocks {
            let _ = Reader::new(blk).skip();
        }
    });
    line("record cbor skip (no output)", d);

    // 6. JSON extraction + event assembly over the transcoded text.
    let raws: Vec<&str> = events.iter().map(|e| e.raw()).collect();
    let (d, _) = best_of(runs, || {
        let mut out: Vec<Event> = Vec::with_capacity(n);
        for r in &raws {
            if let Ok(Decoded::Event(e)) = decode_json_frame(r, &mut b) {
                out.push(e);
            }
        }
        out
    });
    line("json extract + event build", d);
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_default();
    let path = args.next().unwrap_or_else(|| die("usage: vortex-bench-firehose capture|bench FILE [options]"));
    let mut want = 200_000usize;
    let mut rewind = 300_000i64;
    let mut runs = 7usize;
    let mut limit = usize::MAX;
    let mut skip = 0usize;
    let mut host = "relay1.us-east.bsky.network".to_owned();
    while let Some(a) = args.next() {
        let mut value = |name: &str| args.next().unwrap_or_else(|| die(format!("{name} needs a value")));
        match a.as_str() {
            "--frames" => want = value("--frames").parse().unwrap_or_else(|_| die("bad --frames")),
            "--rewind" => rewind = value("--rewind").parse().unwrap_or_else(|_| die("bad --rewind")),
            "--runs" => runs = value("--runs").parse().unwrap_or_else(|_| die("bad --runs")),
            "--limit" => limit = value("--limit").parse().unwrap_or_else(|_| die("bad --limit")),
            "--skip" => skip = value("--skip").parse().unwrap_or_else(|_| die("bad --skip")),
            "--host" => host = value("--host"),
            other => die(format!("unknown argument {other}")),
        }
    }
    match mode.as_str() {
        "capture" => {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(capture(&path, &host, want, rewind));
        }
        "bench" => bench(&path, runs, skip, limit, false),
        // Only the end-to-end loop, for attaching a sampling profiler.
        "profile" => bench(&path, runs, skip, limit, true),
        _ => die("usage: vortex-bench-firehose capture|bench FILE [options]"),
    }
}
