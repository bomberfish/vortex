//! vortex — a live viewer for AT Protocol event streams.

mod atp;
mod config;
mod corners;
mod theme;
mod ui;

use config::{config_path, ConfigFile};
use gpui::{point, px, size, App, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions};
use std::borrow::Cow;
use theme::{resolve_palette, Theme};
use ui::app::Vortex;
use vortex_core::engine::Engine;

/// The default UI font, embedded so the app looks the same everywhere.
static BUNDLED_FONT_BYTES: &[u8] = include_bytes!("../../../assets/fonts/Berkeley.ttf");

fn main() {
    let cfg = ConfigFile::load(config_path());
    let engine = Engine::new(cfg.config.buffer.to_store());

    gpui_platform::application().run(move |cx: &mut App| {
        if let Err(e) = cx.text_system().add_fonts(vec![Cow::Borrowed(BUNDLED_FONT_BYTES)]) {
            eprintln!("vortex: could not load bundled font: {e}");
        }
        // A provisional theme so the first frame is already styled; the view
        // rebuilds it once fonts are validated.
        let (palette, _) = resolve_palette(&cfg.config);
        cx.set_global(Theme::build(&cfg.config, palette, config::BUNDLED_FONT.into()));

        ui::input::bind_keys(cx);
        ui::app::bind_keys(cx);

        let bounds = Bounds::centered(None, size(px(1440.), px(900.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            // The top bar doubles as the title bar: one less row of chrome.
            titlebar: Some(TitlebarOptions {
                title: Some("vortex".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(9.), px(7.))),
            }),
            window_min_size: Some(size(px(640.), px(360.))),
            ..Default::default()
        };
        cx.open_window(options, move |window, cx| cx.new(|cx| Vortex::new(engine, cfg, window, cx)))
            .expect("open main window");
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}
