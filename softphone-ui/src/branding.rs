//! Application identity shared by every window: the desktop app id, the
//! embedded icon, and the `window::Settings` defaults that carry them.
//!
//! On Wayland the task-manager / alt-tab icon is *not* taken from
//! `window::Settings::icon` (winit 0.30 has no `xdg_toplevel_icon` support);
//! the compositor maps the toplevel's `app_id` to the `.desktop` file of the
//! same name and uses that entry's `Icon=`. So `APP_ID` must equal the
//! basename of `packaging/io.github.zerodaylabz.OxideSip.desktop`, and the
//! embedded PNG only matters on X11/XWayland.

use iced::window;

pub const APP_NAME: &str = "OxideSip";
/// Reverse-DNS application id — also the `.desktop` basename, the Flatpak id,
/// and the icon name under `hicolor/`.
pub const APP_ID: &str = "io.github.zerodaylabz.OxideSip";

const ICON_PNG: &[u8] = include_bytes!("../../packaging/icons/128.png");

/// The embedded app icon decoded for winit. `None` only if the compiled-in
/// PNG somehow fails to decode (never expected; the build embeds a known file).
pub fn app_icon() -> Option<window::Icon> {
    let img = image::load_from_memory_with_format(ICON_PNG, image::ImageFormat::Png)
        .ok()?
        .into_rgba8();
    let (w, h) = img.dimensions();
    window::icon::from_rgba(img.into_raw(), w, h).ok()
}

/// Baseline settings for any window this app opens: identity (icon + app id)
/// plus size. Callers override position/resizable/level/etc. as needed.
pub fn window_settings(size: iced::Size, min_size: Option<iced::Size>) -> window::Settings {
    window::Settings {
        size,
        min_size,
        position: window::Position::Centered,
        icon: app_icon(),
        platform_specific: window::settings::PlatformSpecific {
            application_id: APP_ID.to_string(),
            ..Default::default()
        },
        ..window::Settings::default()
    }
}
