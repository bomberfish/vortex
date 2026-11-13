//! The engine: one handle that owns the threads and wires the stages together.
//!
//! ```text
//!  net thread (tokio, current-thread)      pipeline thread (+ rayon for bursts)
//!  ┌──────────────────────────────┐ frames ┌───────────────────────────────────┐
//!  │ websocket · reconnect · resume├───────▶│ zstd · decode · dedup · filter     │
//!  └──────────────▲───────────────┘ bounded └────────────────┬──────────────────┘
//!                 │ commands                                 │ batches
//!            ┌────┴─────┐                               ┌────▼────┐
//!            │  Engine  │◀──────────── UI ─────────────▶│  Store  │
//!            └──────────┘     (never blocks either)     └─────────┘
//! ```

use crate::decode::zstd_dict::Dictionaries;
use crate::filter::{Filter, Pushdown};
use crate::net::{self, Cmd, ConnStatus, Ctx, SourceConfig, StatusCell};
use crate::pipeline;
use crate::stats::{Log, Stats};
use crate::store::{RefilterStats, Store, StoreConfig};
use parking_lot::Mutex;
use std::sync::Arc;
use tokio::sync::mpsc;

struct State {
    pushdown_enabled: bool,
    /// Pushdown implied by the current filter (sent only while enabled).
    filter_pushdown: Pushdown,
    source: Option<SourceConfig>,
}

pub struct Engine {
    pub store: Arc<Store>,
    pub stats: Arc<Stats>,
    pub log: Arc<Log>,
    status: Arc<StatusCell>,
    cmds: mpsc::UnboundedSender<Cmd>,
    state: Mutex<State>,
}

impl Engine {
    pub fn new(store_cfg: StoreConfig) -> Arc<Engine> {
        let store = Arc::new(Store::new(store_cfg));
        let stats = Arc::new(Stats::default());
        let log = Arc::new(Log::default());
        let dicts = Arc::new(Dictionaries::default());
        let status = Arc::new(StatusCell::default());

        let (tx, _pipeline) = pipeline::spawn(store.clone(), stats.clone(), log.clone(), dicts.clone());
        let (cmds, cmd_rx) = mpsc::unbounded_channel();
        let ctx = Ctx { tx, stats: stats.clone(), log: log.clone(), dicts, status: status.clone() };
        std::thread::Builder::new()
            .name("vortex-net".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build network runtime");
                // When this returns the frame sender drops and the pipeline
                // thread drains and exits on its own.
                rt.block_on(net::run(cmd_rx, ctx));
            })
            .expect("spawn network thread");

        Arc::new(Engine {
            store,
            stats,
            log,
            status,
            cmds,
            state: Mutex::new(State { pushdown_enabled: true, filter_pushdown: Pushdown::default(), source: None }),
        })
    }

    /// Start (or restart) streaming from `source`.
    pub fn connect(&self, source: SourceConfig) {
        self.state.lock().source = Some(source.clone());
        let _ = self.cmds.send(Cmd::Connect(source));
    }

    pub fn disconnect(&self) {
        self.state.lock().source = None;
        let _ = self.cmds.send(Cmd::Disconnect);
    }

    pub fn source(&self) -> Option<SourceConfig> {
        self.state.lock().source.clone()
    }

    pub fn is_connected(&self) -> bool {
        self.state.lock().source.is_some()
    }

    /// Install a filter: re-filters buffered history (in parallel) and updates
    /// the server-side filter. Blocks for the duration of the re-filter, so
    /// call it off the UI thread.
    pub fn set_filter(&self, filter: Filter) -> RefilterStats {
        let pushdown = filter.pushdown().clone();
        let stats = self.store.set_filter(filter);
        let mut st = self.state.lock();
        st.filter_pushdown = pushdown;
        self.send_pushdown(&st);
        stats
    }

    /// Enable or disable server-side filtering. Disabled keeps the full
    /// stream flowing so history is complete for any later filter, at the
    /// cost of bandwidth.
    pub fn set_pushdown_enabled(&self, enabled: bool) {
        let mut st = self.state.lock();
        if st.pushdown_enabled != enabled {
            st.pushdown_enabled = enabled;
            self.send_pushdown(&st);
        }
    }

    pub fn pushdown_enabled(&self) -> bool {
        self.state.lock().pushdown_enabled
    }

    fn send_pushdown(&self, st: &State) {
        let p = if st.pushdown_enabled { st.filter_pushdown.clone() } else { Pushdown::default() };
        let _ = self.cmds.send(Cmd::Pushdown(p));
    }

    pub fn status(&self) -> ConnStatus {
        self.status.get()
    }

    /// Bumps whenever the connection status changes.
    pub fn status_version(&self) -> u64 {
        self.status.version()
    }

    /// Drop all buffered events.
    pub fn clear(&self) {
        self.store.clear();
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.cmds.send(Cmd::Shutdown);
    }
}
