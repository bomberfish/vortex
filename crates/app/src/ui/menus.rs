//! The macOS menu bar (also the app menu on other platforms that have one).
//!
//! Without a main menu, macOS can leave the previously active app's menus in
//! the menu bar while vortex is frontmost. Shortcuts shown in the menus come
//! from the keymap, so they always match the real bindings.

use super::app::*;
use super::input;
use gpui::{actions, App, Menu, MenuItem, OsAction, SystemMenuType};

actions!(vortex_app, [Hide, HideOthers, ShowAll]);

pub fn install(cx: &mut App) {
    cx.on_action(|_: &Hide, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    // Quit must work even with no window focused.
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.bind_keys([
        gpui::KeyBinding::new("cmd-h", Hide, None),
        gpui::KeyBinding::new("alt-cmd-h", HideOthers, None),
        gpui::KeyBinding::new("cmd-m", Minimize, None),
        gpui::KeyBinding::new("ctrl-cmd-f", ToggleFullscreen, None),
    ]);

    cx.set_menus([
        Menu::new("vortex").items([
            MenuItem::action("About vortex", ShowWelcome),
            MenuItem::separator(),
            MenuItem::action("Settings…", ToggleSettings),
            MenuItem::action("Open config.toml", OpenConfig),
            MenuItem::separator(),
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Hide vortex", Hide),
            MenuItem::action("Hide Others", HideOthers),
            MenuItem::action("Show All", ShowAll),
            MenuItem::separator(),
            MenuItem::action("Quit vortex", Quit),
        ]),
        Menu::new("Edit").items([
            MenuItem::os_action("Cut", input::Cut, OsAction::Cut),
            MenuItem::os_action("Copy", input::Copy, OsAction::Copy),
            MenuItem::os_action("Paste", input::Paste, OsAction::Paste),
            MenuItem::os_action("Select All", input::SelectAll, OsAction::SelectAll),
            MenuItem::separator(),
            MenuItem::action("Filter…", FocusFilter),
            MenuItem::action("Copy Event JSON", CopyJson),
            MenuItem::action("Copy AT-URI", CopyUri),
        ]),
        Menu::new("View").items([
            MenuItem::action("Command Palette…", OpenPalette),
            MenuItem::action("Welcome", ShowWelcome),
            MenuItem::separator(),
            MenuItem::action("Toggle Sidebar", ToggleSidebar),
            MenuItem::action("Toggle Detail Pane", ToggleDetail),
            MenuItem::separator(),
            MenuItem::action("Next Theme", NextTheme),
            MenuItem::action("Previous Theme", PrevTheme),
            MenuItem::action("Bigger Text", FontBigger),
            MenuItem::action("Smaller Text", FontSmaller),
            MenuItem::separator(),
            MenuItem::action("Enter Full Screen", ToggleFullscreen),
        ]),
        Menu::new("Stream").items([
            MenuItem::action("Connect / Disconnect", ToggleConnect),
            MenuItem::action("Tail", ToggleFollow),
            MenuItem::action("Clear Buffer", ClearBuffer),
            MenuItem::separator(),
            MenuItem::submenu(Menu::new("Source").items([
                MenuItem::action("Jetstream", SourceJetstream),
                MenuItem::action("Jetstream v1 (legacy)", SourceJetstreamV1),
                MenuItem::action("Relay Firehose", SourceFirehose),
                MenuItem::action("Spacedust (Links)", SourceSpacedust),
                MenuItem::action("PLC Directory", SourcePlc),
            ])),
            MenuItem::submenu(Menu::new("Start From").items([
                MenuItem::action("Live", ReplayLive),
                MenuItem::action("Last Minute", Replay1m),
                MenuItem::action("Last 5 Minutes", Replay5m),
                MenuItem::action("Last 30 Minutes", Replay30m),
                MenuItem::action("Last 2 Hours", Replay2h),
            ])),
        ]),
        Menu::new("Go").items([
            MenuItem::action("Inspect Selected", InspectSelected),
            MenuItem::action("Back", InspectBack),
            MenuItem::action("Forward", InspectForward),
            MenuItem::action("Up", InspectUp),
            MenuItem::separator(),
            MenuItem::action("Scroll to Selected Row", RevealSelected),
            MenuItem::action("Open in Browser", OpenInBrowser),
            MenuItem::action("Open on pdsls.dev", OpenInPdsls),
        ]),
        Menu::new("Window").items([MenuItem::action("Minimize", Minimize), MenuItem::action("Zoom", Zoom)]),
        Menu::new("Help").items([MenuItem::action("Filter Syntax & Keys", ToggleHelp)]),
    ]);
}
