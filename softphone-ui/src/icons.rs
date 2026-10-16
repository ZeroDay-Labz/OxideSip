//! Inline SVG line icons (Lucide, ISC — see `assets/icons/LICENSE`), tinted
//! to whatever colour the call site asks for. Shipping the SVGs inside the
//! binary means every desktop renders exactly the same glyphs.

use crate::app::Message;
use iced::widget::svg;
use iced::{Color, Element, Length};

macro_rules! icon_fn {
    ($name:ident, $file:literal) => {
        pub fn $name<'a>(size: f32, color: Color) -> Element<'a, Message> {
            let handle = svg::Handle::from_memory(include_bytes!(concat!("../assets/icons/", $file)).as_slice());
            svg(handle)
                .width(Length::Fixed(size))
                .height(Length::Fixed(size))
                .style(move |_theme, _status| svg::Style { color: Some(color) })
                .into()
        }
    };
}

icon_fn!(phone, "phone.svg");
icon_fn!(phone_incoming, "phone-incoming.svg");
icon_fn!(phone_outgoing, "phone-outgoing.svg");
icon_fn!(phone_missed, "phone-missed.svg");
icon_fn!(phone_off, "phone-off.svg");
icon_fn!(phone_forwarded, "phone-forwarded.svg");
icon_fn!(phone_call, "phone-call.svg");
icon_fn!(mic, "mic.svg");
icon_fn!(mic_off, "mic-off.svg");
icon_fn!(pause, "pause.svg");
icon_fn!(play, "play.svg");
icon_fn!(user_plus, "user-plus.svg");
icon_fn!(history, "history.svg");
icon_fn!(settings, "settings.svg");
icon_fn!(search, "search.svg");
icon_fn!(plus, "plus.svg");
icon_fn!(trash, "trash-2.svg");
icon_fn!(pencil, "pencil.svg");
icon_fn!(x, "x.svg");
icon_fn!(chevron_up, "chevron-up.svg");
icon_fn!(chevron_down, "chevron-down.svg");
icon_fn!(refresh, "refresh-cw.svg");
icon_fn!(arrow_left_right, "arrow-left-right.svg");
icon_fn!(merge, "merge.svg");
icon_fn!(split, "split.svg");
icon_fn!(maximize, "maximize-2.svg");
icon_fn!(minimize, "minimize-2.svg");
icon_fn!(folder, "folder.svg");
icon_fn!(moon, "moon.svg");
icon_fn!(sun, "sun.svg");
icon_fn!(bell_off, "bell-off.svg");
icon_fn!(bell_ring, "bell-ring.svg");
icon_fn!(audio_lines, "audio-lines.svg");
icon_fn!(circle_check, "circle-check.svg");
icon_fn!(x_circle, "x-circle.svg");
icon_fn!(alert, "triangle-alert.svg");
icon_fn!(info, "info.svg");
icon_fn!(headphones, "headphones.svg");
icon_fn!(server, "server.svg");
icon_fn!(clock, "clock.svg");
icon_fn!(arrow_left, "arrow-left.svg");
icon_fn!(book_user, "book-user.svg");
icon_fn!(layout_grid, "layout-grid.svg");

/// The app logo glyph (handset + signal arcs), tinted.
pub fn logo<'a>(size: f32, color: Color) -> Element<'a, Message> {
    let handle = svg::Handle::from_memory(include_bytes!("../assets/logo.svg").as_slice());
    svg(handle)
        .width(Length::Fixed(size))
        .height(Length::Fixed(size))
        .style(move |_theme, _status| svg::Style { color: Some(color) })
        .into()
}
