//! Micro-benchmark on real traffic: replay a few minutes of Jetstream into
//! memory, then time each pipeline stage in isolation.
//!
//! ```text
//! vortex-bench [--rewind 5m] [--events 200000]
//! ```

use std::time::{Duration, Instant};
use vortex_core::decode::frame::decode_json_frame;
use vortex_core::decode::zstd_dict::{DictDecompressor, BUNDLED_V2_DICT};
use vortex_core::decode::Decoded;
use vortex_core::engine::Engine;
use vortex_core::event::{Event, EventBuilder};
use vortex_core::filter::Filter;
use vortex_core::net::{SourceConfig, SourceKind, StartFrom};
use vortex_core::store::StoreConfig;
use vortex_core::time::parse_duration_us;

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

fn main() {
    let mut args = std::env::args().skip(1);
    let mut rewind = Duration::from_secs(300);
    let mut want = 200_000usize;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--rewind" => {
                let us = args.next().and_then(|v| parse_duration_us(&v)).expect("--rewind needs a duration");
                rewind = Duration::from_micros(us as u64);
            }
            "--events" => want = args.next().and_then(|v| v.parse().ok()).expect("--events needs a number"),
            other => panic!("unknown argument {other}"),
        }
    }

    let engine = Engine::new(StoreConfig { ring_events: want, ring_bytes: usize::MAX, pinned_events: 0 });
    let mut source = SourceConfig::new(SourceKind::JetstreamV2);
    source.start = StartFrom::Rewind(rewind);
    engine.connect(source);
    eprintln!("capturing up to {want} events (rewind {rewind:?})…");
    let deadline = Instant::now() + Duration::from_secs(40);
    while engine.store.view().ring_len() < want && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    engine.disconnect();
    std::thread::sleep(Duration::from_millis(300));

    let raws: Vec<String> = {
        let v = engine.store.view();
        (0..v.len()).filter_map(|i| v.row(i)).map(|e| e.raw().to_owned()).collect()
    };
    let n = raws.len();
    assert!(n > 0, "captured nothing");
    let bytes: usize = raws.iter().map(String::len).sum();
    println!("corpus: {n} events, {:.1} MB JSON, {} B/event avg", bytes as f64 / 1e6, bytes / n);
    let per = |d: Duration| d.as_nanos() as f64 / n as f64;
    let rate = |d: Duration| n as f64 / d.as_secs_f64() / 1e6;

    // zstd (as the server would send it)
    let mut enc = zstd::bulk::Compressor::with_dictionary(1, BUNDLED_V2_DICT).unwrap();
    let packed: Vec<Vec<u8>> = raws.iter().map(|r| enc.compress(r.as_bytes()).unwrap()).collect();
    let packed_bytes: usize = packed.iter().map(Vec::len).sum();
    let mut dec = DictDecompressor::new(BUNDLED_V2_DICT).unwrap();
    let mut buf = Vec::new();
    let (d, _) = best_of(3, || {
        for p in &packed {
            dec.decompress(p, &mut buf).unwrap();
        }
    });
    println!(
        "zstd inflate        {:>7.0} ns/event  {:>6.2} M events/s   ({:.1}x ratio)",
        per(d),
        rate(d),
        bytes as f64 / packed_bytes as f64
    );

    // decode
    let mut b = EventBuilder::new();
    let (d, events) = best_of(3, || {
        let mut out: Vec<Event> = Vec::with_capacity(n);
        for r in &raws {
            if let Ok(Decoded::Event(e)) = decode_json_frame(r, &mut b) {
                out.push(e);
            }
        }
        out
    });
    println!("decode + extract    {:>7.0} ns/event  {:>6.2} M events/s   ({:.0} MB/s)", per(d), rate(d), bytes as f64 / 1e6 / d.as_secs_f64());

    // filters, single-threaded
    println!("\nfilter evaluation, one thread:");
    let queries = [
        "kind:commit",
        "type:post",
        "type:post lang:en has:media -is:reply",
        "did:did:plc:z72i7hdynmk6r22z27h6tvur",
        "bluesky",
        "bluesky OR twitter OR mastodon OR threads OR elon OR trump OR biden OR news",
        "type:post bluesky",
        "text~\\bcats?\\b",
        "type:post text:/https?:\\/\\/\\S+\\.(png|jpe?g|gif)/",
        "domain:youtube.com,youtu.be",
        "#art OR #photography",
        "type:post len>200 langs:1 images>=2",
        "record.embed.$type:video",
        "record.facets[*].features[*].$type:link",
        "raw:bafyrei",
        "(type:post lang:ja has:media) OR (type:like subject:did:plc:z72i7hdynmk6r22z27h6tvur) OR is:delete",
    ];
    for q in queries {
        let f = Filter::parse(q).unwrap();
        let (d, hits) = best_of(3, || events.iter().filter(|e| f.matches(e)).count());
        println!("  {:>6.1} ns/event  {:>7.2} M/s  {:>6.2}% match   {q}", per(d), rate(d), hits as f64 * 100.0 / n as f64);
    }

    // re-filter of the full buffer (parallel, as the UI does on every edit)
    println!("\nstore re-filter of {n} buffered events (parallel):");
    for q in ["type:post lang:en has:media", "bluesky OR twitter OR mastodon", "record.embed.$type:video", ""] {
        let (d, st) = best_of(3, || engine.store.set_filter(Filter::parse(q).unwrap()));
        println!("  {:>7.2} ms  {:>7} matched   {}", d.as_secs_f64() * 1e3, st.matched, if q.is_empty() { "(all)" } else { q });
    }
}
