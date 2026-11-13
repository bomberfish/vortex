//! Headless stream tail: the engine without the UI.
//!
//! ```text
//! vortex-tail [options] [filter query…]
//!   --source jetstream|jetstream-v1|firehose   (default jetstream)
//!   --host HOST            override host (repeatable)
//!   --rewind 5m            start this far in the past
//!   --cursor N             start from an explicit cursor
//!   --no-compress          disable zstd
//!   --no-pushdown          do not filter server-side
//!   --secs N               stop after N seconds (default: run until killed)
//!   --print N              print the first N matching events (default 10)
//!   --json                 print raw JSON instead of a summary line
//! ```

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use vortex_core::engine::Engine;
use vortex_core::event::{Event, Field};
use vortex_core::filter::Filter;
use vortex_core::net::{SourceConfig, SourceKind, StartFrom};
use vortex_core::stats::RateSampler;
use vortex_core::store::StoreConfig;
use vortex_core::time::{civil_from_us, now_us, parse_duration_us};

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("vortex-tail: {msg}");
    std::process::exit(2);
}

fn summary(e: &Event) -> String {
    let t = civil_from_us(e.time_us);
    let what = if e.collection.is_some() {
        format!("{} {}", e.op.as_str(), e.collection_str())
    } else {
        format!("{} {}", e.kind.as_str(), e.field(Field::Aux))
    };
    let text = e.text().replace('\n', " ");
    let text: String = text.chars().take(100).collect();
    format!("{:02}:{:02}:{:02}.{:03} {} {:<34} {}", t.hour, t.minute, t.second, t.micros / 1000, e.did, what, text)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut kind = SourceKind::JetstreamV2;
    let mut hosts = Vec::new();
    let mut start = StartFrom::Live;
    let mut compress = true;
    let mut pushdown = true;
    let mut secs = None;
    let mut print = 10usize;
    let mut json = false;
    let mut query = Vec::new();
    while let Some(a) = args.next() {
        let mut value = |name: &str| args.next().unwrap_or_else(|| die(format!("{name} needs a value")));
        match a.as_str() {
            "--source" => {
                let v = value("--source");
                kind = SourceKind::parse(&v).unwrap_or_else(|| die(format!("unknown source '{v}'")));
            }
            "--host" => hosts.push(value("--host")),
            "--rewind" => {
                let v = value("--rewind");
                let us = parse_duration_us(&v).unwrap_or_else(|| die(format!("bad duration '{v}'")));
                start = StartFrom::Rewind(Duration::from_micros(us as u64));
            }
            "--cursor" => start = StartFrom::Cursor(value("--cursor").parse().unwrap_or_else(|_| die("bad cursor"))),
            "--no-compress" => compress = false,
            "--no-pushdown" => pushdown = false,
            "--secs" => secs = Some(value("--secs").parse::<u64>().unwrap_or_else(|_| die("bad --secs"))),
            "--print" => print = value("--print").parse().unwrap_or_else(|_| die("bad --print")),
            "--json" => json = true,
            "-h" | "--help" => {
                println!("usage: vortex-tail [--source jetstream|jetstream-v1|firehose] [--host H] [--rewind 5m] [--cursor N] [--no-compress] [--no-pushdown] [--secs N] [--print N] [--json] [filter…]");
                return;
            }
            _ => query.push(a),
        }
    }
    let query = query.join(" ");
    let filter = Filter::parse(&query).unwrap_or_else(|e| die(format!("filter: {e}")));
    eprintln!("filter   : {}", if query.is_empty() { "(all)" } else { &query });
    eprintln!("compiled : {}", filter.describe());
    eprintln!("pushdown : {}", if filter.pushdown().is_empty() { "(none)".into() } else { filter.pushdown().summary() });

    let engine = Engine::new(StoreConfig::default());
    engine.set_pushdown_enabled(pushdown);
    engine.set_filter(filter);
    let mut source = SourceConfig::new(kind);
    if !hosts.is_empty() {
        source.hosts = hosts;
    }
    source.compress = compress && kind.supports_compression();
    source.start = start;
    engine.connect(source);

    let started = Instant::now();
    let mut sampler = RateSampler::new(60);
    let mut printed = 0usize;
    let mut next_row = 0usize;
    let mut log_seen = 0u64;
    let mut last_report = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(100));
        if engine.log.version() != log_seen {
            let entries = engine.log.snapshot();
            let new = (engine.log.version() - log_seen) as usize;
            for e in entries.iter().skip(entries.len().saturating_sub(new)) {
                eprintln!("[log] {}", e.text);
            }
            log_seen = engine.log.version();
        }
        {
            let v = engine.store.view();
            while printed < print && next_row < v.len() {
                if let Some(e) = v.row(next_row) {
                    if json {
                        println!("{}", e.raw());
                    } else {
                        println!("{}", summary(e));
                    }
                    printed += 1;
                }
                next_row += 1;
            }
        }
        if last_report.elapsed() >= Duration::from_secs(1) {
            last_report = Instant::now();
            let snap = engine.stats.snapshot();
            let r = sampler.sample(snap);
            let v = engine.store.view();
            let lag_ms = if snap.last_time_us > 0 { (now_us() - snap.last_time_us) / 1000 } else { 0 };
            eprintln!(
                "{:>5.0}s  {:>7.0} ev/s  {:>6.0} match/s  {:>7.1} KB/s wire  {:>7.1} KB/s json  load {:>4.1}%  q {:>5}  lag {:>6}ms  buf {} ({:.0} MB)  view {}  err {} dup {}",
                started.elapsed().as_secs_f64(),
                r.events_per_s,
                r.matched_per_s,
                r.wire_bytes_per_s / 1024.0,
                r.decoded_bytes_per_s / 1024.0,
                r.pipeline_load * 100.0,
                snap.queue_depth,
                lag_ms,
                v.ring_len(),
                v.bytes() as f64 / (1 << 20) as f64,
                v.len(),
                snap.decode_errors,
                snap.duplicates,
            );
        }
        if secs.is_some_and(|s| started.elapsed() >= Duration::from_secs(s)) {
            break;
        }
    }
    let snap = engine.stats.snapshot();
    let status = engine.status();
    eprintln!(
        "done: {} frames, {} events, {} matched, {} errors, {} connects, state {:?}, server filter [{}]",
        snap.frames,
        snap.events,
        snap.matched,
        snap.decode_errors,
        engine.stats.connects.load(Ordering::Relaxed),
        status.state,
        status.server_filter
    );
}
