//! Persistent settings, stored as TOML.
//!
//! Location: `$XDG_CONFIG_HOME/vortex/config.toml`, falling back to
//! `~/.config/vortex/config.toml`. Every field has a default, so a partial
//! (or absent) file is fine. The file is re-read when it changes on disk, and
//! rewritten when a setting is changed from the UI.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::SystemTime;
use vortex_core::net::{SourceConfig, SourceKind, StartFrom};
use vortex_core::store::StoreConfig;

/// Family name of the bundled font.
pub const BUNDLED_FONT: &str = "Berkeley Mono";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    /// A built-in theme name or one defined under `[themes.<name>]`.
    pub theme: String,
    /// Font family for the whole UI.
    pub font: String,
    pub font_size: f32,
    /// Row height as a multiple of the font size.
    pub line_height: f32,
    /// Base padding in pixels.
    pub padding: f32,
    /// Corner radius in pixels.
    pub radius: f32,
    /// Square window corners (macOS draws them rounded by default).
    pub square_corners: bool,
    /// Show frames/s and frame build time in the status bar.
    pub fps_meter: bool,
    /// Show the welcome screen on launch.
    pub welcome: bool,
    pub sidebar: bool,
    pub sidebar_width: f32,
    pub detail: bool,
    pub detail_width: f32,
}

impl Default for UiConfig {
    fn default() -> Self {
        UiConfig {
            theme: crate::theme::DEFAULT_THEME.into(),
            font: BUNDLED_FONT.into(),
            font_size: 12.0,
            line_height: 1.4,
            padding: 4.0,
            radius: 2.0,
            square_corners: false,
            fps_meter: true,
            welcome: true,
            sidebar: true,
            sidebar_width: 220.0,
            detail: true,
            detail_width: 440.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SourceSettings {
    /// `jetstream`, `jetstream-v1`, `firehose`, `spacedust` or `plc`.
    pub kind: String,
    /// Hosts to use instead of the defaults for `kind` (tried in order).
    /// Cleared when the source changes; `url` is kept per source.
    pub hosts: Vec<String>,
    /// zstd compression (Jetstream).
    pub compress: bool,
    /// Let the server pre-filter by kind/collection/DID (Jetstream).
    pub pushdown: bool,
    /// Connect on launch.
    pub autoconnect: bool,
    /// PLC: hide identity events from PDSes that create accounts in bulk.
    pub hide_plc_spam: bool,
    pub plc_spam_hosts: Vec<String>,
    /// Per-source endpoint, keyed by source name: a host, `wss://host`, or
    /// a full endpoint URL. Overrides `hosts`; unset means the defaults.
    pub url: BTreeMap<String, String>,
}

impl Default for SourceSettings {
    fn default() -> Self {
        SourceSettings {
            kind: SourceKind::JetstreamV2.label().into(),
            hosts: Vec::new(),
            compress: true,
            pushdown: true,
            autoconnect: true,
            hide_plc_spam: true,
            plc_spam_hosts: vec!["pds.trump.com".into()],
            url: BTreeMap::new(),
        }
    }
}

impl SourceSettings {
    pub fn kind(&self) -> SourceKind {
        SourceKind::parse(&self.kind).unwrap_or(SourceKind::JetstreamV2)
    }

    /// The user's endpoint for `kind`, if set.
    pub fn url_for(&self, kind: SourceKind) -> Option<&str> {
        self.url.get(kind.label()).map(|u| u.trim()).filter(|u| !u.is_empty())
    }

    /// PDS hosts whose identity events are hidden, given the current source.
    pub fn hidden_pds(&self) -> Vec<String> {
        if self.kind() == SourceKind::Plc && self.hide_plc_spam {
            self.plc_spam_hosts.clone()
        } else {
            Vec::new()
        }
    }

    pub fn to_source(&self, start: StartFrom) -> SourceConfig {
        let kind = self.kind();
        let mut s = SourceConfig::new(kind);
        if let Some(url) = self.url_for(kind) {
            s.hosts = vec![url.to_owned()];
        } else if !self.hosts.is_empty() {
            s.hosts = self.hosts.clone();
        }
        s.compress = self.compress && kind.supports_compression();
        s.start = start;
        s
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BufferSettings {
    /// Events kept for instant re-filtering.
    pub events: usize,
    /// Memory cap for those events, in MiB.
    pub megabytes: usize,
    /// Matching events kept after they age out of the buffer.
    pub pinned: usize,
}

impl Default for BufferSettings {
    fn default() -> Self {
        let d = StoreConfig::default();
        BufferSettings { events: d.ring_events, megabytes: d.ring_bytes >> 20, pinned: d.pinned_events }
    }
}

impl BufferSettings {
    pub fn to_store(&self) -> StoreConfig {
        StoreConfig {
            ring_events: self.events.clamp(1_000, 20_000_000),
            ring_bytes: self.megabytes.clamp(16, 64 * 1024) << 20,
            pinned_events: self.pinned.min(20_000_000),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SavedFilter {
    pub name: String,
    pub query: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FilterSettings {
    /// The query restored on launch.
    pub query: String,
    pub saved: Vec<SavedFilter>,
    /// Collections always listed at the top of the sidebar, in this order.
    pub pinned: Vec<String>,
}

/// The record types most people filter on.
pub const DEFAULT_PINNED: &[&str] = &[
    "app.bsky.feed.post",
    "app.bsky.feed.like",
    "app.bsky.feed.repost",
    "app.bsky.graph.follow",
    "app.bsky.graph.block",
    "app.bsky.actor.profile",
    "app.bsky.graph.list",
    "app.bsky.graph.listitem",
    "app.bsky.graph.starterpack",
    "app.bsky.feed.generator",
    "app.bsky.feed.threadgate",
    "app.bsky.feed.postgate",
];

impl Default for FilterSettings {
    fn default() -> Self {
        FilterSettings {
            query: String::new(),
            saved: Vec::new(),
            pinned: DEFAULT_PINNED.iter().map(|s| (*s).to_owned()).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaSettings {
    /// Load images, avatars and video previews.
    pub load: bool,
    /// Show media under adult, graphic or warning labels without a click.
    pub reveal_labeled: bool,
}

impl Default for MediaSettings {
    fn default() -> Self {
        MediaSettings { load: true, reveal_labeled: false }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub ui: UiConfig,
    pub source: SourceSettings,
    pub buffer: BufferSettings,
    pub filter: FilterSettings,
    pub media: MediaSettings,
    /// Per-color overrides on top of the theme, e.g. `accent = "#ff8800"`.
    pub colors: BTreeMap<String, String>,
    /// User themes: `[themes.name]` with color keys and an optional `base`.
    pub themes: BTreeMap<String, BTreeMap<String, String>>,
}

const HEADER: &str = "\
# vortex configuration — edits are picked up while the app is running.
#
# [ui]      theme, font, font_size, line_height, padding, radius, panel sizes
# [colors]  override any theme color:  accent = \"#ff8800\"
#           slots: bg bg_alt bg_hover bg_sel border fg fg_dim fg_faint accent
#                  red green yellow blue purple aqua orange
# [themes.mine]  define a theme:  base = \"nord\"  bg = \"#101010\"  …
#           then set  ui.theme = \"mine\"

";

pub fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("vortex").join("config.toml")
}

impl Config {
    pub fn parse(text: &str) -> Result<Config, String> {
        toml::from_str(text).map_err(|e| e.message().to_owned())
    }

    pub fn to_toml(&self) -> String {
        let body = toml::to_string_pretty(self).unwrap_or_default();
        format!("{HEADER}{body}")
    }
}

/// The config plus what is needed to keep it in sync with the file.
pub struct ConfigFile {
    pub path: PathBuf,
    pub config: Config,
    /// Parse error of the file on disk, if any. While set, the app runs on
    /// the last good config and never overwrites the file.
    pub error: Option<String>,
    mtime: Option<SystemTime>,
}

fn mtime(path: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl ConfigFile {
    /// Load from disk; a missing file yields defaults (and is created).
    pub fn load(path: PathBuf) -> ConfigFile {
        let mut file = ConfigFile { path, config: Config::default(), error: None, mtime: None };
        match std::fs::read_to_string(&file.path) {
            Ok(text) => {
                file.mtime = mtime(&file.path);
                match Config::parse(&text) {
                    Ok(c) => file.config = c,
                    Err(e) => file.error = Some(e),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => file.save(),
            Err(e) => file.error = Some(e.to_string()),
        }
        file
    }

    /// Re-read if the file changed on disk. Returns true if `config` changed.
    pub fn reload_if_changed(&mut self) -> bool {
        let now = mtime(&self.path);
        if now == self.mtime {
            return false;
        }
        self.mtime = now;
        let Ok(text) = std::fs::read_to_string(&self.path) else { return false };
        match Config::parse(&text) {
            Ok(c) => {
                let changed = c != self.config || self.error.is_some();
                self.config = c;
                self.error = None;
                changed
            }
            Err(e) => {
                let changed = self.error.as_deref() != Some(e.as_str());
                self.error = Some(e);
                changed
            }
        }
    }

    /// Write the config. Skipped while the on-disk file has a parse error so
    /// a half-edited file is never clobbered.
    pub fn save(&mut self) {
        if self.error.is_some() {
            return;
        }
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Write-then-rename so a crash never leaves a truncated config.
        let tmp = self.path.with_extension("toml.tmp");
        if std::fs::write(&tmp, self.config.to_toml()).is_ok() && std::fs::rename(&tmp, &self.path).is_ok() {
            self.mtime = mtime(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip() {
        let c = Config::default();
        let text = c.to_toml();
        assert!(text.starts_with("# vortex configuration"));
        assert_eq!(Config::parse(&text).unwrap(), c);
        assert_eq!(c.ui.font, BUNDLED_FONT);
        assert_eq!(c.ui.theme, "kanagawa");
        assert_eq!(c.filter.pinned.len(), DEFAULT_PINNED.len());
    }

    #[test]
    fn partial_files_fill_in_defaults() {
        let c = Config::parse("[ui]\ntheme = \"nord\"\nfont_size = 14\n\n[colors]\naccent = \"#ff8800\"\n").unwrap();
        assert_eq!(c.ui.theme, "nord");
        assert_eq!(c.ui.font_size, 14.0);
        assert_eq!(c.ui.padding, UiConfig::default().padding);
        assert_eq!(c.colors["accent"], "#ff8800");
        assert_eq!(c.source, SourceSettings::default());
        assert_eq!(Config::parse("").unwrap(), Config::default());
        assert!(Config::parse("[ui\ntheme=").is_err());
    }

    #[test]
    fn saved_filters_and_themes_round_trip() {
        let mut c = Config::default();
        c.filter.query = "type:post \"quoted\" lang:ja".into();
        c.filter.saved.push(SavedFilter { name: "media".into(), query: "has:media -is:reply".into() });
        c.themes.insert("mine".into(), [("base".to_string(), "nord".to_string())].into());
        c.source.hosts = vec!["ws://localhost:6008".into()];
        assert_eq!(Config::parse(&c.to_toml()).unwrap(), c);
    }

    #[test]
    fn file_lifecycle() {
        let dir = std::env::temp_dir().join(format!("vortex-config-test-{}", std::process::id()));
        let path = dir.join("nested").join("config.toml");
        let _ = std::fs::remove_dir_all(&dir);

        // Missing file: defaults are written out.
        let mut f = ConfigFile::load(path.clone());
        assert!(path.exists() && f.error.is_none());
        assert!(!f.reload_if_changed());

        // External edit is picked up.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, "[ui]\ntheme = \"dracula\"\n").unwrap();
        f.mtime = None;
        assert!(f.reload_if_changed());
        assert_eq!(f.config.ui.theme, "dracula");

        // A broken edit keeps the last good config and blocks saving.
        std::fs::write(&path, "[ui\nbroken").unwrap();
        f.mtime = None;
        assert!(f.reload_if_changed());
        assert!(f.error.is_some());
        assert_eq!(f.config.ui.theme, "dracula");
        f.config.ui.font_size = 20.0;
        f.save();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[ui\nbroken");

        // Fixing the file clears the error.
        std::fs::write(&path, "[ui]\ntheme = \"nord\"\n").unwrap();
        f.mtime = None;
        assert!(f.reload_if_changed());
        assert!(f.error.is_none());
        f.config.ui.font_size = 15.0;
        f.save();
        assert_eq!(ConfigFile::load(path.clone()).config.ui.font_size, 15.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_convert_to_core_types() {
        let mut s = SourceSettings::default();
        assert_eq!(s.kind(), SourceKind::JetstreamV2);
        s.kind = "firehose".into();
        s.compress = true;
        let src = s.to_source(StartFrom::Live);
        assert_eq!(src.kind, SourceKind::Firehose);
        assert!(!src.compress);
        s.kind = "garbage".into();
        assert_eq!(s.kind(), SourceKind::JetstreamV2);
        s.hosts = vec!["example.com".into()];
        assert_eq!(s.to_source(StartFrom::Live).hosts, ["example.com"]);
        let b = BufferSettings { events: 5, megabytes: 1, pinned: 7 };
        let st = b.to_store();
        assert_eq!((st.ring_events, st.ring_bytes, st.pinned_events), (1_000, 16 << 20, 7));
    }

    /// The README's config reference must stay true: its defaults block is
    /// exactly `Config::default()`, and its examples block is a valid config.
    #[test]
    fn readme_config_matches_defaults() {
        let readme = include_str!("../../../README.md");
        let blocks: Vec<&str> =
            readme.split("```toml\n").skip(1).map(|rest| rest.split("```").next().unwrap()).collect();
        assert_eq!(blocks.len(), 2, "expected a defaults block and an examples block");
        assert_eq!(Config::parse(blocks[0]).unwrap(), Config::default());
        let examples = Config::parse(blocks[1]).unwrap();
        assert_eq!(examples.filter.saved.len(), 1);
        assert!(examples.themes.contains_key("midnight"));
    }
}
