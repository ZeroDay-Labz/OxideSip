//! The "OxideSip" visual identity: a single custom `iced::Theme` plus a
//! small set of reusable style closures, so every screen/window pulls from
//! one palette instead of hand-rolled colors scattered across `view.rs`.

use iced::widget::{button, container, slider};
use iced::{color, Background, Border, Color, Radians, Shadow, Theme};

pub fn oxide_palette() -> iced::theme::Palette {
    iced::theme::Palette {
        background: color!(0x14161b),
        text: color!(0xe8e6e1),
        primary: color!(0xff7a3d),
        success: color!(0x33d17a),
        warning: color!(0xffb648),
        danger: color!(0xff5c5c),
    }
}

pub fn theme() -> Theme {
    Theme::custom("OxideSip".to_string(), oxide_palette())
}

/// A shared spacing scale, so `.spacing()`/`.padding()` calls across
/// `view.rs` pick from one small vocabulary instead of arbitrary per-call-site
/// pixel values. Existing screens still use hand-picked literals in most
/// places (all of them run through `scaled()` for responsive sizing anyway,
/// so a token here is just `scaled(space::MD, scale)` at the call site) —
/// this scale is meant to be adopted incrementally, screen by screen, not as
/// a single mechanical find-replace across the whole file.
#[allow(dead_code)] // adopted incrementally screen-by-screen, see module doc above
pub mod space {
    pub const XS: f32 = 4.0;
    pub const SM: f32 = 8.0;
    pub const MD: f32 = 12.0;
    pub const LG: f32 = 16.0;
    pub const XL: f32 = 24.0;
    pub const XXL: f32 = 32.0;
}

/// A shared type scale, same intent as `space` above — covers the ~6-28px
/// range of `.size()` calls already in use across `view.rs`.
#[allow(dead_code)] // adopted incrementally screen-by-screen, see module doc above
pub mod text_size {
    pub const CAPTION: f32 = 11.0;
    pub const BODY: f32 = 13.0;
    pub const SUBHEAD: f32 = 15.0;
    pub const TITLE: f32 = 20.0;
    pub const DISPLAY: f32 = 28.0;
}

/// Slightly lighter than the app background, used for card-like panels
/// (rows, footers, forms) so they read as distinct surfaces.
const SURFACE: Color = color!(0x1c1f26);
const SURFACE_BORDER: Color = Color {
    r: 1.0,
    g: 1.0,
    b: 1.0,
    a: 0.06,
};
/// A step lighter than `SURFACE` — list rows (contacts, call history) sit on
/// top of a `card`-styled panel that's already `SURFACE`-colored, so a row
/// using that same tone would be invisible against its own container. This
/// keeps the same background/panel/row "elevation" progression instead.
const ROW_SURFACE: Color = color!(0x242a35);
const ROW_SURFACE_HOVER: Color = color!(0x2c3340);

/// A soft top-lit vertical gradient from `base`, a touch lighter, into
/// `base` itself — used on filled buttons/active fills so they read as
/// gently glossy/raised instead of a flat block of color. Deliberately
/// subtle (a 10% lightness step): enough to add depth, not enough to look
/// like a skeuomorphic bevel.
fn top_lit(base: Color) -> Background {
    let lighter = Color {
        r: (base.r + 0.10).min(1.0),
        g: (base.g + 0.10).min(1.0),
        b: (base.b + 0.10).min(1.0),
        a: base.a,
    };
    Background::Gradient(
        iced::gradient::Linear::new(Radians(std::f32::consts::FRAC_PI_2))
            .add_stop(0.0, lighter)
            .add_stop(1.0, base)
            .into(),
    )
}

pub fn card(_theme: &Theme) -> container::Style {
    container::Style {
        text_color: None,
        background: Some(Background::Color(SURFACE)),
        border: Border {
            color: SURFACE_BORDER,
            width: 1.0,
            radius: 18.0.into(),
        },
        shadow: Shadow {
            color: Color::from_rgba(0.0, 0.0, 0.0, 0.3),
            offset: iced::Vector::new(0.0, 3.0),
            blur_radius: 16.0,
        },
        ..container::Style::default()
    }
}

/// The whole main window's backdrop, behind the tab bar/panel/footer —
/// without this, `main_view`'s outermost container has no explicit style at
/// all, so the window just shows the theme's flat `palette.background`
/// solid color. A very subtle top-lit gradient (barely darker than
/// `background` at the very top, easing to it) instead, so the app doesn't
/// read as one flat matte block behind the cards floating on it.
pub fn app_background(theme: &Theme) -> container::Style {
    let palette = theme.palette();
    let top = Color {
        r: (palette.background.r + 0.02).min(1.0),
        g: (palette.background.g + 0.02).min(1.0),
        b: (palette.background.b + 0.03).min(1.0),
        a: palette.background.a,
    };
    container::Style {
        text_color: None,
        background: Some(Background::Gradient(
            iced::gradient::Linear::new(Radians(std::f32::consts::FRAC_PI_2))
                .add_stop(0.0, top)
                .add_stop(1.0, palette.background)
                .into(),
        )),
        ..container::Style::default()
    }
}

/// Small inline "chip" background — same surface/border tone as `card` but
/// a touch less round and with no drop shadow, since this is meant for
/// small inline footer badges (the call-status label, the registration
/// LED) rather than an elevated panel.
pub fn chip(_theme: &Theme) -> container::Style {
    container::Style {
        text_color: None,
        background: Some(Background::Color(SURFACE)),
        border: Border {
            color: SURFACE_BORDER,
            width: 1.0,
            radius: 14.0.into(),
        },
        ..container::Style::default()
    }
}

/// Faint inset "track" container behind the Dialer/Contacts/History tab
/// pills — gives them a modern segmented-control look (the active tab's
/// filled pill standing out against a subtly recessed background) instead
/// of three buttons floating directly on the page background.
pub fn tab_track(_theme: &Theme) -> container::Style {
    container::Style {
        text_color: None,
        background: Some(Background::Color(Color {
            a: 0.05,
            ..Color::WHITE
        })),
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: 16.0.into(),
        },
        ..container::Style::default()
    }
}

/// Fully round container used for the registration status LED.
pub fn led(fill: Color) -> impl Fn(&Theme) -> container::Style {
    move |_theme| container::Style {
        text_color: None,
        background: Some(Background::Color(fill)),
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: 5.0.into(),
        },
        shadow: Shadow {
            color: fill.scale_alpha(0.6),
            offset: iced::Vector::new(0.0, 0.0),
            blur_radius: 6.0,
        },
        ..container::Style::default()
    }
}

/// Deterministic avatar background color for a contact/history entry, hashed
/// from its display name/number so the same person always gets the same
/// color without needing to persist one.
pub fn avatar_color(seed: &str) -> Color {
    let hash = seed.bytes().fold(5381u32, |h, b| {
        h.wrapping_mul(33).wrapping_add(b as u32)
    });
    let hue = (hash % 360) as f32;
    hsl_to_rgb(hue, 0.55, 0.5)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> Color {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match h as u32 {
        0..=59 => (c, x, 0.0),
        60..=119 => (x, c, 0.0),
        120..=179 => (0.0, c, x),
        180..=239 => (0.0, x, c),
        240..=299 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    Color::from_rgb(r + m, g + m, b + m)
}

/// A perfectly round container, e.g. for a contact/history avatar initial.
pub fn avatar(fill: Color) -> impl Fn(&Theme) -> container::Style {
    move |_theme| container::Style {
        text_color: Some(Color::WHITE),
        background: Some(Background::Color(fill)),
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: 999.0.into(),
        },
        ..container::Style::default()
    }
}

/// Same as `avatar`, plus a soft colored glow scaled by `live_amount`
/// (0.0-1.0) — used for the caller/callee avatar on the incoming-call and
/// in-call screens while there's actually live audio (ringing, or connected
/// and not on hold), so the one moment that matters most in the whole app
/// reads as more alive than a flat circle. Takes a continuous amount rather
/// than a bool so a caller can drive it from an `iced::Animation<bool>`
/// (see `CallUiState::Active::avatar_glow`) and get a real cross-fade
/// instead of the glow snapping on/off. A single function (not two) so
/// every call site shares one concrete return type — `if`/`else` can't pick
/// between two `impl Trait` functions even when their signatures match,
/// since each one is still a distinct anonymous type.
pub fn avatar_state(fill: Color, live_amount: f32) -> impl Fn(&Theme) -> container::Style {
    let live_amount = live_amount.clamp(0.0, 1.0);
    move |_theme| container::Style {
        text_color: Some(Color::WHITE),
        background: Some(Background::Color(fill)),
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: 999.0.into(),
        },
        shadow: Shadow {
            color: fill.scale_alpha(0.55 * live_amount),
            offset: iced::Vector::new(0.0, 0.0),
            blur_radius: 22.0 * live_amount,
        },
        ..container::Style::default()
    }
}

#[derive(Clone, Copy)]
pub enum Pill {
    Primary,
    Success,
    Danger,
    Neutral,
    /// 0.0 = unselected, 1.0 = selected — a continuous amount (rather than a
    /// bare `bool`) so the tab bar can cross-fade the fill in/out via
    /// `iced::Animation<bool>::interpolate` instead of it snapping instantly.
    Tab(f32),
}

/// Linear interpolation between two colors, channel-wise.
fn lerp_color(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    Color {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
}

/// Rounded, filled button style used for the tab bar and the primary call
/// actions (answer/hang up/hold/transfer), replacing iced's default
/// rectangular button look everywhere in the app.
pub fn pill(kind: Pill) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let palette = theme.palette();
        let (base, text, filled) = match kind {
            Pill::Primary => (palette.primary, Color::WHITE, true),
            Pill::Success => (palette.success, Color::BLACK, true),
            Pill::Danger => (palette.danger, Color::WHITE, true),
            Pill::Neutral => (
                Color {
                    a: 0.08,
                    ..Color::WHITE
                },
                palette.text,
                false,
            ),
            Pill::Tab(t) => (
                palette.primary.scale_alpha(t.clamp(0.0, 1.0)),
                lerp_color(palette.text, Color::WHITE, t),
                false,
            ),
        };
        let alpha_scale = match status {
            button::Status::Active => 1.0,
            button::Status::Hovered => 0.85,
            button::Status::Pressed => 0.7,
            button::Status::Disabled => 0.35,
        };
        let background = if filled {
            top_lit(base.scale_alpha(alpha_scale))
        } else {
            Background::Color(base.scale_alpha(alpha_scale))
        };
        // Filled pills (primary/success/danger CTAs, the active tab) get a
        // soft shadow tinted with their own fill color so they visually lift
        // off the page instead of sitting flush with the background — the
        // rest stay flat, matching their less prominent role.
        let shadow = if filled && matches!(status, button::Status::Active | button::Status::Hovered) {
            Shadow {
                color: base.scale_alpha(0.35),
                offset: iced::Vector::new(0.0, 2.0),
                blur_radius: 10.0,
            }
        } else {
            Shadow::default()
        };
        button::Style {
            background: Some(background),
            text_color: text,
            border: Border {
                color: Color::TRANSPARENT,
                width: 0.0,
                radius: 14.0.into(),
            },
            shadow,
            ..button::Style::default()
        }
    }
}

/// Card-like button style for list rows (contacts, call history) — each
/// entry gets its own subtly bordered, slightly-raised surface instead of
/// `Pill::Neutral`'s flatter near-transparent look, so a list of many
/// entries reads as distinct separated items rather than a single blurred
/// block of text.
pub fn list_row(_theme: &Theme, status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered => ROW_SURFACE_HOVER,
        _ => ROW_SURFACE,
    };
    button::Style {
        background: Some(Background::Color(background)),
        text_color: oxide_palette().text,
        border: Border {
            color: SURFACE_BORDER,
            width: 1.0,
            radius: 15.0.into(),
        },
        shadow: Shadow::default(),
        ..button::Style::default()
    }
}

/// Perfectly round buttons — the dialpad keys and the mute/hold icon
/// toggles. `active` swaps in the primary color (e.g. a toggle that's
/// currently engaged); otherwise a neutral glass-like fill.
pub fn circle(active: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let palette = theme.palette();
        let base = if active {
            palette.primary
        } else {
            Color {
                a: 0.07,
                ..Color::WHITE
            }
        };
        let background = match status {
            button::Status::Active => base,
            button::Status::Hovered => Color {
                a: (base.a + 0.06).min(1.0),
                ..base
            },
            button::Status::Pressed => base.scale_alpha(0.75),
            button::Status::Disabled => base.scale_alpha(0.35),
        };
        button::Style {
            background: Some(Background::Color(background)),
            text_color: if active { Color::WHITE } else { palette.text },
            border: Border {
                color: Color::TRANSPARENT,
                width: 0.0,
                radius: 999.0.into(),
            },
            shadow: Shadow::default(),
            ..button::Style::default()
        }
    }
}

/// Round button colored by an arbitrary state color (not just the theme's
/// primary), used for the Line 1-5 sidebar buttons. `fill` reflects the call
/// state (`None` = idle, `Some(color)` = ringing/dialing/active/ending);
/// `selected` is drawn as a border *independent* of `fill` so the currently
/// selected line is always visually distinguishable — including an idle
/// selected line, which otherwise looked identical to every other idle line
/// (the original bug: clicking a line button while nothing was ringing gave
/// no visible feedback at all, reading as "doesn't work").
pub fn circle_state(fill: Option<Color>, selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let palette = theme.palette();
        let indicator = fill.unwrap_or(palette.primary);
        let base_alpha = match (fill.is_some(), selected) {
            (true, true) => 0.9,
            (true, false) => 0.18,
            (false, true) => 0.14,
            (false, false) => 0.07,
        };
        let base_color = if fill.is_some() { indicator } else { Color::WHITE };
        let base = Color {
            a: base_alpha,
            ..base_color
        };
        let background = match status {
            button::Status::Active => base,
            button::Status::Hovered => Color {
                a: (base.a + 0.06).min(1.0),
                ..base
            },
            button::Status::Pressed => base.scale_alpha(0.75),
            button::Status::Disabled => base.scale_alpha(0.35),
        };
        let text_color = if fill.is_some() && selected {
            Color::WHITE
        } else {
            palette.text
        };
        let border_color = if selected {
            indicator
        } else if fill.is_some() {
            indicator.scale_alpha(0.5)
        } else {
            Color::TRANSPARENT
        };
        // A live/ringing/active line (fill.is_some() && selected, the
        // "0.9 alpha" case above) gets the same glossy top-lit fill and a
        // soft colored shadow `pill`'s filled CTAs use, so the one line
        // that's actually live reads with real presence instead of just a
        // slightly different flat tint.
        let prominent = fill.is_some() && selected;
        let background_fill = if prominent {
            top_lit(background)
        } else {
            Background::Color(background)
        };
        let shadow = if prominent {
            Shadow {
                color: indicator.scale_alpha(0.4),
                offset: iced::Vector::new(0.0, 2.0),
                blur_radius: 12.0,
            }
        } else {
            Shadow::default()
        };
        button::Style {
            background: Some(background_fill),
            text_color,
            border: Border {
                color: border_color,
                width: if selected || fill.is_some() { 1.5 } else { 0.0 },
                radius: 14.0.into(),
            },
            shadow,
            ..button::Style::default()
        }
    }
}

/// Same shape/role as `circle(false)` (dialpad keys), but blended toward the
/// primary color by `flash` (0.0 = normal neutral fill, 1.0 = just pressed)
/// — `view.rs`'s `dialpad` decays this from 1.0 back to 0.0 over a couple
/// hundred ms after each press, so a tap gets its own brief visible
/// confirmation instead of relying only on the pointer-held-down `Pressed`
/// state, which a fast tap-and-release barely shows.
pub fn circle_flash(flash: f32) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let palette = theme.palette();
        let neutral = Color {
            a: 0.07,
            ..Color::WHITE
        };
        let base = lerp_color(neutral, palette.primary, flash.clamp(0.0, 1.0));
        let background = match status {
            button::Status::Active => base,
            button::Status::Hovered => Color {
                a: (base.a + 0.06).min(1.0),
                ..base
            },
            button::Status::Pressed => base.scale_alpha(0.75),
            button::Status::Disabled => base.scale_alpha(0.35),
        };
        button::Style {
            background: Some(Background::Color(background)),
            text_color: lerp_color(palette.text, Color::WHITE, flash.clamp(0.0, 1.0)),
            border: Border {
                color: Color::TRANSPARENT,
                width: 0.0,
                radius: 999.0.into(),
            },
            shadow: Shadow::default(),
            ..button::Style::default()
        }
    }
}

/// A transparent-rail slider, meant to be stacked directly on top of a level
/// meter fill (see `view.rs`'s `meter_slider`) so the two fuse into one
/// control: the meter shows live signal level, the thin handle bar is what
/// you actually drag to set gain. A plain slider would paint its own opaque
/// rail over the meter and hide it.
pub fn meter_slider(theme: &Theme, _status: slider::Status) -> slider::Style {
    let palette = theme.palette();
    slider::Style {
        rail: slider::Rail {
            backgrounds: (
                Background::Color(Color::TRANSPARENT),
                Background::Color(Color::TRANSPARENT),
            ),
            width: 22.0,
            border: Border::default(),
        },
        handle: slider::Handle {
            shape: slider::HandleShape::Rectangle {
                width: 5,
                border_radius: 3.0.into(),
            },
            background: Background::Color(Color::WHITE),
            border_width: 2.0,
            border_color: palette.primary,
        },
    }
}

