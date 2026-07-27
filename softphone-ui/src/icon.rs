//! Bootstrap Icons glyphs for buttons that used to show small-caps text
//! labels ("MIC", "HOLD", "XFER", ...). Bundled via `iced_fonts` and loaded
//! once at boot (`App::boot`'s `font::load` task) rather than relying on a
//! system symbol font being present — the same reliability goal
//! `view.rs::initial`'s ASCII-only fallback was originally written around,
//! just solved here by shipping our own font instead of avoiding symbols.
//!
//! Each re-exported function returns a normal `iced::widget::Text`, so a
//! call site chains `.size(...)`/`.color(...)` on it exactly like `text(...)`.

pub use iced_fonts::BOOTSTRAP_FONT_BYTES as FONT_BYTES;
pub use iced_fonts::bootstrap::{
    arrow_clockwise, arrow_left_right, arrows_fullscreen, box_arrow_in_down, box_arrow_up, chevron_down,
    chevron_up, clock_history, dash_lg, diagram_three_fill, foldertwo_open, gear_fill, mic_fill, mic_mute_fill,
    pause_fill, pencil_fill, person_lines_fill, person_x, play_fill, plus_lg, search, sliders,
    telephone_fill, telephone_forward_fill, telephone_inbound_fill, telephone_outbound_fill, telephone_x,
    telephone_x_fill, trash_fill, x_circle_fill,
};
