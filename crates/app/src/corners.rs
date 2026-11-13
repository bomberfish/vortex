//! Square window corners on macOS.
//!
//! AppKit has no public switch for this, so this uses the private
//! `-[NSWindow _setCornerRadius:]` (macOS 26). A radius of 0 means "system
//! default", so square corners use a radius too small to see. If the selector
//! is missing, nothing happens and the window simply stays rounded.

use gpui::Window;

#[cfg(target_os = "macos")]
mod imp {
    use gpui::Window;
    use objc2::runtime::AnyObject;
    use objc2::{msg_send, sel};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    /// Smallest radius AppKit takes literally (0 restores the default).
    const SQUARE_RADIUS: f64 = 0.01;

    fn ns_window(window: &Window) -> Option<*mut AnyObject> {
        let RawWindowHandle::AppKit(h) = HasWindowHandle::window_handle(window).ok()?.as_raw() else { return None };
        let view = h.ns_view.as_ptr() as *mut AnyObject;
        let win: *mut AnyObject = unsafe { msg_send![view, window] };
        (!win.is_null()).then_some(win)
    }

    pub fn set_square(window: &Window, square: bool) {
        let Some(win) = ns_window(window) else { return };
        unsafe {
            let supported: bool = msg_send![win, respondsToSelector: sel!(_setCornerRadius:)];
            if !supported {
                return;
            }
            let radius = if square { SQUARE_RADIUS } else { 0.0 };
            let _: () = msg_send![win, _setCornerRadius: radius];
            let _: () = msg_send![win, invalidateShadow];
        }
    }
}

/// Square (`true`) or system-rounded (`false`) window corners. No-op off macOS.
pub fn set_square(window: &Window, square: bool) {
    #[cfg(target_os = "macos")]
    imp::set_square(window, square);
    #[cfg(not(target_os = "macos"))]
    let _ = (window, square);
}
