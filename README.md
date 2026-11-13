# vortex

A fast, compact desktop viewer for AT Protocol event streams — Bluesky
Jetstream and the relay firehose — with a real query language for filtering.
Rust + [GPUI](https://gpui.rs).

```
cargo run --release -p vortex          # the app
cargo run --release --bin vortex-tail -- 'type:post lang:ja has:media'   # headless
cargo run --release --bin vortex-bench                                   # stage timings on live data
```

## Sources

| source | endpoint | notes |
| --- | --- | --- |
| `jetstream` (default) | `network.bsky.jetstream.subscribeEvents` | JSON, dictionary-zstd, server-side filtering, replay by cursor |
| `jetstream-v1` | legacy `/subscribe` | same, older wire format |
| `firehose` | `com.atproto.sync.subscribeRepos` | DAG-CBOR + CAR, decoded locally; unfiltered, live only |

All three decode into the same event model, so every filter works on every
source. Connections reconnect with jittered backoff, resume from the last
processed cursor, and fail over between hosts.

## Filtering

Type in the filter bar (`/`). Terms are ANDed; `OR`, `-term`/`NOT`, and
parentheses work as expected. Errors are underlined as you type.

```
type:post lang:en has:media -is:reply
"exact phrase" OR #rustlang OR domain:github.com
type:like subject:did:plc:z72i7hdynmk6r22z27h6tvur
kind:account status:takendown,suspended
type:post text~\bcats?\b len>200 images>=2
record.embed.$type:video        record.facets[*].features[*].tag:art
did:@~/watchlist.txt            sample:5%
```

| | |
| --- | --- |
| `word` `"a phrase"` | text contains (case-insensitive) |
| `text:` `name:` `alt:` | post text / display name / image alt |
| `type:` `collection:` | `post like repost follow block profile …` or an NSID; globs ok |
| `did:` `from:` | author DID; `did:@/path/file` loads a list |
| `kind:` `op:` | `commit identity account sync` / `create update delete` |
| `lang:` `tag:` `#tag` | language (`en` matches `en-US`) / hashtag |
| `mention:` `link:` `domain:` | mentioned DID / link URL / link host (subdomains match) |
| `has:` `is:` `embed:` | `reply quote media image video link mention tag label alt gif root …` |
| `reply-to:` `root:` `quote:` `subject:` | referenced AT-URI contains (a DID works) |
| `handle:` `status:` `active:` | identity / account event fields |
| `len images tags mentions links langs bytes` | counts: `len>200` `images:2` `tags:1..3` |
| `time created skew` | `time>2026-01-01T00:00Z` `skew>1h` (createdAt vs. event time) |
| `record.path` `json.path` | any JSON field, with `[n]`, `[*]`, `*` |
| `exists:` `raw:` `uri:` `rkey:` `label:` `word:` | path exists / raw frame contains / … |
| `sample:` | keep a deterministic fraction: `sample:10%` |

Operators: `:` match · `=` exact · `~` regex · `!=` `!~` negated · `>` `>=`
`<` `<=` numeric · `a,b` any-of · `/re/flags` regex literal · `*` glob.

Clicking a kind or collection in the sidebar toggles it in the query
(alt-click excludes). `+` saves the current query.

**Changing the filter re-filters everything already buffered**, not just new
events. With `srv` on, the part of the query the server can evaluate (kinds,
collections, DIDs) is pushed down to Jetstream, so a narrow query costs almost
no bandwidth; turn it off to keep the full stream buffered for later queries.

## Keys

| | |
| --- | --- |
| `/` `⌘F` | focus filter (`enter` apply, `esc` back) |
| `j` `k` `↑` `↓` `g` `G` `pgup` `pgdn` | move selection |
| `space` | tail on/off |
| `c` `u` `o` | copy JSON / copy AT-URI / open in browser |
| `⌘K` | clear buffer |
| `⌘B` `⌘I` | sidebar / detail pane |
| `⌘T` `⌘=` `⌘-` | next theme / font size |
| `⌘,` `?` | settings / filter help |
| `⌘↵` | connect / disconnect |

## Configuration

`~/.config/vortex/config.toml` (or `$XDG_CONFIG_HOME/vortex/config.toml`).
Edits are picked up while the app runs; changes made in the UI are written
back. A file with a syntax error is reported and never overwritten.

```toml
[ui]
theme = "kanagawa"         # gruvbox-dark gruvbox-light catppuccin-mocha tokyo-night
                           # nord dracula solarized-dark one-dark bloomberg
font = "Berkeley Mono"     # any installed family
font_size = 12.0
line_height = 1.4
padding = 4.0
radius = 2.0

[colors]                   # override any slot of the active theme
accent = "#ff8800"

[themes.midnight]          # or define your own (then ui.theme = "midnight")
base = "nord"
bg = "#0b0e14"

[source]
kind = "jetstream"         # jetstream | jetstream-v1 | firehose
hosts = []                 # empty = public defaults; "ws://localhost:6008" works
compress = true
pushdown = true

[buffer]
events = 300000            # kept for instant re-filtering
megabytes = 512
pinned = 100000            # matches kept after they age out of the buffer
```

Color slots: `bg bg_alt bg_hover bg_sel border fg fg_dim fg_faint accent red
green yellow blue purple aqua orange`.

## Architecture

```
 net thread (tokio, current-thread)       pipeline thread (+ rayon on bursts)
 ┌───────────────────────────────┐ frames ┌─────────────────────────────────────┐
 │ websocket · reconnect · resume ├───────▶│ zstd · decode · dedup · filter       │
 └───────────────▲───────────────┘ bounded └──────────────────┬──────────────────┘
                 │ commands                                   │ batches
            ┌────┴─────┐                                 ┌────▼────┐
            │  Engine  │◀────────────── UI ─────────────▶│  Store  │
            └──────────┘     (never blocks on either)    └─────────┘
```

- **`crates/core`** has no UI dependency and is fully testable headless.
- **Network** only moves bytes. Frames cross a bounded channel, so a slow
  consumer back-pressures the socket instead of growing memory.
- **Pipeline** drains the channel in adaptive batches: a few frames at
  live-tail rates (one thread hop of latency), thousands during replay, where
  decode fans out over rayon with order preserved. The store lock is taken
  once per batch and the filter is evaluated outside it.
- **Events** are a fixed 168-byte header plus one allocation holding the raw
  JSON and every extracted field. `did:plc` identifiers are packed to 15
  bytes; collections are interned pointers. Filters read pre-extracted data —
  nothing is re-parsed unless a JSON-path predicate asks.
- **Decoding** uses a purpose-built zero-allocation JSON pull scanner. The
  firehose path transcodes DAG-CBOR/CAR into the Jetstream JSON shape so there
  is exactly one extraction path.
- **Filters** compile to a small tree that is optimized before use: OR'd
  text literals merge into one Aho-Corasick automaton, DID lists into a hash
  set, kind/flag tests into single mask checks, and children are ordered
  cheapest-first.
- **Store** keeps every recent event (so a new filter can be applied to
  history, in parallel) plus matches that have aged out of that window.
- **UI** polls version counters at 30 Hz and renders only when something
  changed; rendering takes the store lock with a 2 ms budget and reuses the
  previous frame's rows if ingest or a re-filter holds it.

### Numbers

`vortex-bench` on live Jetstream traffic (Apple Silicon, one thread unless
noted):

| stage | cost |
| --- | --- |
| zstd inflate | ~0.7 µs/event |
| decode + extract | ~0.7 µs/event (≈1.4 M events/s, ≈1 GB/s of JSON) |
| typical filters | 5–35 ns/event |
| JSON-path filter | ~200 ns/event |
| re-filter 140k buffered events (parallel) | 0.4–6 ms |
| 10 min replay (214k events) end to end | ~2.5 s, network-bound |

The live network is a few hundred events per second; the headroom is for
replay and for re-filtering large buffers interactively.

## Notes

- The bundled default font (`assets/fonts/Berkeley.ttf`) is Berkeley Mono, a
  commercially licensed typeface. Keep the repository private, or replace the
  file and `BUNDLED_FONT` in `crates/app/src/config.rs`, before publishing.
- The text input is adapted from GPUI's `examples/input.rs` (Apache-2.0).
- Jetstream zstd dictionaries in `assets/dict/` come from
  [bluesky-social/jetstream](https://github.com/bluesky-social/jetstream)
  (MIT/Apache-2.0). If a server retires the bundled dictionary, the current
  one is fetched automatically.
