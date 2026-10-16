//! The OxideSip design system: colour tokens for a dark and a light mode,
//! the `iced::Theme` built from them, and the catalogue of widget styles
//! every screen draws from — so no screen invents its own colours.
//!
//! Pattern: style functions take `&Theme`, resolve the current `Mode` from
//! it (`mode_of`) and read the matching `Tokens`. That keeps both modes in
//! one place and lets `App::theme` switch at runtime (System / Dark / Light).

use iced::widget::{button, container, pick_list, rule, scrollable, slider, text_input, toggler};
use iced::{Background, Border, Color, Radians, Shadow, Theme, border, color};

/// Resolved colour mode (what is actually on screen).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Dark,
    Light,
}

/// The user's preference, persisted in settings. `System` follows the
/// desktop's colour scheme (xdg-desktop-portal), resolved by `App::theme`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeSetting {
    #[default]
    System,
    Dark,
    Light,
}

impl ThemeSetting {
    pub const ALL: [ThemeSetting; 3] = [ThemeSetting::System, ThemeSetting::Dark, ThemeSetting::Light];
}

impl std::fmt::Display for ThemeSetting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ThemeSetting::System => "Follow system",
            ThemeSetting::Dark => "Dark",
            ThemeSetting::Light => "Light",
        })
    }
}

/// Brand colours shared by both modes (slightly re-tuned for light).
pub const ACCENT: Color = color!(0xff7a3d); // copper — the "oxide"
pub const ACCENT_LIGHT: Color = color!(0xe5602a);
pub const VERDIGRIS: Color = color!(0x3ec9a7); // patina green: registered / answer
pub const VERDIGRIS_LIGHT: Color = color!(0x1f9f7f);
pub const AMBER: Color = color!(0xffb648); // ringing / warnings
pub const AMBER_LIGHT: Color = color!(0xc98a12);
pub const CORAL: Color = color!(0xff5c5c); // decline / hang up / errors
pub const CORAL_LIGHT: Color = color!(0xd64545);
pub const SKY: Color = color!(0x5ec4f5); // transfer / info
pub const SKY_LIGHT: Color = color!(0x2a8fc9);

#[derive(Debug, Clone, Copy)]
pub struct Tokens {
    pub mode: Mode,
    pub bg: Color,
    pub sidebar: Color,
    pub panel: Color,
    pub raised: Color,
    pub raised_hover: Color,
    pub line: Color,
    pub text: Color,
    pub text_dim: Color,
    pub meter_bg: Color,
    pub accent: Color,
    pub success: Color,
    pub warning: Color,
    pub danger: Color,
    pub info: Color,
}

pub fn tokens(mode: Mode) -> Tokens {
    match mode {
        Mode::Dark => Tokens {
            mode,
            bg: color!(0x111318),
            sidebar: color!(0x0d0f13),
            panel: color!(0x1a1d24),
            raised: color!(0x262a34),
            raised_hover: color!(0x343a48),
            line: Color::from_rgba8(255, 255, 255, 0.07),
            text: color!(0xecf0f4),
            text_dim: color!(0x969eac),
            meter_bg: color!(0x0a0b0e),
            accent: ACCENT,
            success: VERDIGRIS,
            warning: AMBER,
            danger: CORAL,
            info: SKY,
        },
        Mode::Light => Tokens {
            mode,
            bg: color!(0xf4f5f8),
            sidebar: color!(0xeceef3),
            panel: Color::WHITE,
            raised: color!(0xeef0f5),
            raised_hover: color!(0xe3e6ee),
            line: Color::from_rgba8(0, 0, 0, 0.08),
            text: color!(0x1b1f27),
            text_dim: color!(0x5f6878),
            meter_bg: color!(0xdfe3ea),
            accent: ACCENT_LIGHT,
            success: VERDIGRIS_LIGHT,
            warning: AMBER_LIGHT,
            danger: CORAL_LIGHT,
            info: SKY_LIGHT,
        },
    }
}

/// A translucent tint of `c`.
pub const fn tint(c: Color, a: f32) -> Color {
    Color { a, ..c }
}

pub fn theme(mode: Mode) -> Theme {
    let t = tokens(mode);
    Theme::custom(
        match mode {
            Mode::Dark => "OxideSip Dark".to_string(),
            Mode::Light => "OxideSip Light".to_string(),
        },
        iced::theme::Palette {
            background: t.bg,
            text: t.text,
            primary: t.accent,
            success: t.success,
            warning: t.warning,
            danger: t.danger,
        },
    )
}

pub fn mode_of(theme: &Theme) -> Mode {
    let b = theme.palette().background;
    if 0.2126 * b.r + 0.7152 * b.g + 0.0722 * b.b > 0.5 {
        Mode::Light
    } else {
        Mode::Dark
    }
}

/// Tokens for the theme currently being rendered.
pub fn t(theme: &Theme) -> Tokens {
    tokens(mode_of(theme))
}

/// Spacing scale (px). Every `.spacing()`/`.padding()` picks from here.
pub mod space {
    pub const XS: f32 = 4.0;
    pub const SM: f32 = 8.0;
    pub const MD: f32 = 12.0;
    pub const LG: f32 = 16.0;
    pub const XL: f32 = 24.0;
}

/// Type scale (px). Nothing renders below `CAPTION`.
pub mod text_size {
    pub const CAPTION: f32 = 11.0;
    pub const BODY: f32 = 13.0;
    pub const SUBHEAD: f32 = 15.0;
    pub const TITLE: f32 = 20.0;
    pub const DISPLAY: f32 = 28.0;
}

/// Corner radii.
pub mod radius {
    pub const MD: f32 = 10.0;
    pub const LG: f32 = 14.0;
    pub const PILL: f32 = 999.0;
}

pub fn rounded(r: f32) -> Border {
    Border {
        radius: border::radius(r),
        ..Border::default()
    }
}

/// Linear interpolation between two colours, channel-wise.
pub fn lerp_color(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    Color {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
}

/// A soft top-lit gradient from `base` (a touch lighter at the top) into
/// `base` — filled buttons read as gently raised instead of flat.
fn top_lit(base: Color) -> Background {
    let lighter = Color {
        r: (base.r + 0.08).min(1.0),
        g: (base.g + 0.08).min(1.0),
        b: (base.b + 0.08).min(1.0),
        a: base.a,
    };
    Background::Gradient(
        iced::gradient::Linear::new(Radians(std::f32::consts::FRAC_PI_2))
            .add_stop(0.0, lighter)
            .add_stop(1.0, base)
            .into(),
    )
}

// ------------------------------------------------------------------ containers

/// The window backdrop: a very subtle top-lit gradient over `bg`.
pub fn app_background(theme: &Theme) -> container::Style {
    let t = t(theme);
    let top = Color {
        r: (t.bg.r + 0.02).min(1.0),
        g: (t.bg.g + 0.02).min(1.0),
        b: (t.bg.b + 0.03).min(1.0),
        a: 1.0,
    };
    container::Style {
        text_color: Some(t.text),
        background: Some(Background::Gradient(
            iced::gradient::Linear::new(Radians(std::f32::consts::FRAC_PI_2))
                .add_stop(0.0, top)
                .add_stop(1.0, t.bg)
                .into(),
        )),
        ..container::Style::default()
    }
}

pub fn sidebar(theme: &Theme) -> container::Style {
    let t = t(theme);
    container::Style {
        background: Some(t.sidebar.into()),
        text_color: Some(t.text),
        ..Default::default()
    }
}

/// A content panel: `panel` fill, hairline border, 14 px corners.
pub fn panel(theme: &Theme) -> container::Style {
    let t = t(theme);
    container::Style {
        background: Some(t.panel.into()),
        border: Border {
            radius: border::radius(radius::LG),
            width: 1.0,
            color: t.line,
        },
        ..Default::default()
    }
}

/// `panel` with a coloured edge — the one that is live/selected/ringing.
pub fn panel_accent(accent: Color) -> impl Fn(&Theme) -> container::Style {
    move |theme| {
        let t = t(theme);
        container::Style {
            background: Some(t.panel.into()),
            border: Border {
                radius: border::radius(radius::LG),
                width: 1.0,
                color: tint(accent, 0.45),
            },
            shadow: Shadow {
                color: tint(accent, 0.18),
                offset: iced::Vector::new(0.0, 0.0),
                blur_radius: 18.0,
            },
            ..Default::default()
        }
    }
}

/// A step above `panel`: rows, inputs' housing, grouped controls.
pub fn raised(theme: &Theme) -> container::Style {
    let t = t(theme);
    container::Style {
        background: Some(t.raised.into()),
        border: rounded(radius::MD),
        ..Default::default()
    }
}

/// Small rounded label (status readouts, account tags).
pub fn chip(theme: &Theme) -> container::Style {
    let t = t(theme);
    container::Style {
        background: Some(t.raised.into()),
        border: rounded(radius::PILL),
        text_color: Some(t.text_dim),
        ..Default::default()
    }
}

/// Chip tinted with a state colour (registered / ringing / failed).
pub fn chip_tinted(accent: Color) -> impl Fn(&Theme) -> container::Style {
    move |theme| {
        let t = t(theme);
        container::Style {
            background: Some(tint(accent, if t.mode == Mode::Dark { 0.16 } else { 0.14 }).into()),
            border: rounded(radius::PILL),
            text_color: Some(if t.mode == Mode::Dark {
                accent
            } else {
                lerp_color(accent, t.text, 0.3)
            }),
            ..Default::default()
        }
    }
}

/// Housing for the segmented tab/filter pills.
pub fn tab_track(theme: &Theme) -> container::Style {
    let t = t(theme);
    container::Style {
        background: Some(t.raised.into()),
        border: rounded(radius::LG),
        ..Default::default()
    }
}

/// Fully round dot used for status LEDs, with a glow in its own colour.
pub fn led(fill: Color) -> impl Fn(&Theme) -> container::Style {
    move |_theme| container::Style {
        background: Some(fill.into()),
        border: rounded(radius::PILL),
        shadow: Shadow {
            color: fill.scale_alpha(0.6),
            offset: iced::Vector::new(0.0, 0.0),
            blur_radius: 6.0,
        },
        ..Default::default()
    }
}

/// Floating overlay card (modals, toasts, menus).
pub fn overlay(theme: &Theme) -> container::Style {
    let t = t(theme);
    container::Style {
        background: Some(t.panel.into()),
        text_color: Some(t.text),
        border: Border {
            radius: border::radius(radius::LG),
            width: 1.0,
            color: t.line,
        },
        shadow: Shadow {
            color: Color::from_rgba(0.0, 0.0, 0.0, if t.mode == Mode::Dark { 0.5 } else { 0.18 }),
            offset: iced::Vector::new(0.0, 8.0),
            blur_radius: 28.0,
        },
        ..Default::default()
    }
}

/// Dim scrim behind a modal.
pub fn scrim(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Color::from_rgba(0.0, 0.0, 0.0, 0.45).into()),
        ..Default::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Warning,
    Error,
}

/// A toast: overlay card with a coloured left accent (border colour).
pub fn toast(kind: ToastKind) -> impl Fn(&Theme) -> container::Style {
    move |theme| {
        let t = t(theme);
        let accent = match kind {
            ToastKind::Info => t.info,
            ToastKind::Success => t.success,
            ToastKind::Warning => t.warning,
            ToastKind::Error => t.danger,
        };
        let mut style = overlay(theme);
        style.border.color = tint(accent, 0.6);
        style
    }
}

pub fn toast_accent(kind: ToastKind, theme: &Theme) -> Color {
    let t = t(theme);
    match kind {
        ToastKind::Info => t.info,
        ToastKind::Success => t.success,
        ToastKind::Warning => t.warning,
        ToastKind::Error => t.danger,
    }
}

// --------------------------------------------------------------------- avatars

/// Deterministic avatar colour for a person, hashed from their name/number.
pub fn avatar_color(seed: &str) -> Color {
    let hash = seed
        .bytes()
        .fold(5381u32, |h, b| h.wrapping_mul(33).wrapping_add(b as u32));
    let hue = (hash % 360) as f32;
    hsl_to_rgb(hue, 0.5, 0.48)
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

/// `avatar` plus a glow scaled by `live_amount` (0–1) for a ringing or
/// connected party — drive it from an `iced::Animation<bool>` for a fade.
pub fn avatar_state(fill: Color, live_amount: f32) -> impl Fn(&Theme) -> container::Style {
    let live_amount = live_amount.clamp(0.0, 1.0);
    move |_theme| container::Style {
        text_color: Some(Color::WHITE),
        background: Some(top_lit(fill)),
        border: rounded(radius::PILL),
        shadow: Shadow {
            color: fill.scale_alpha(0.55 * live_amount),
            offset: iced::Vector::new(0.0, 0.0),
            blur_radius: 22.0 * live_amount,
        },
        ..container::Style::default()
    }
}

// --------------------------------------------------------------------- buttons

fn btn(bg: Color, hover: Color, text: Color, r: f32) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let background = match status {
            button::Status::Hovered | button::Status::Pressed => hover,
            button::Status::Disabled => Color { a: bg.a * 0.5, ..bg },
            _ => bg,
        };
        button::Style {
            background: Some(background.into()),
            text_color: if matches!(status, button::Status::Disabled) {
                Color { a: 0.5, ..text }
            } else {
                text
            },
            border: rounded(r),
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

/// Filled accent button (the page's primary action).
pub fn button_primary(theme: &Theme, status: button::Status) -> button::Style {
    let t = t(theme);
    btn(
        t.accent,
        lerp_color(t.accent, Color::WHITE, 0.12),
        Color::WHITE,
        radius::MD,
    )(theme, status)
}

/// Raised neutral button.
pub fn button_soft(theme: &Theme, status: button::Status) -> button::Style {
    let t = t(theme);
    btn(t.raised, t.raised_hover, t.text, radius::MD)(theme, status)
}

/// Borderless button that only shows a fill on hover.
pub fn button_ghost(theme: &Theme, status: button::Status) -> button::Style {
    let t = t(theme);
    btn(Color::TRANSPARENT, t.raised, t.text, radius::MD)(theme, status)
}

/// Danger-tinted button (delete, clear).
pub fn button_danger(theme: &Theme, status: button::Status) -> button::Style {
    let t = t(theme);
    btn(tint(t.danger, 0.16), tint(t.danger, 0.3), t.danger, radius::MD)(theme, status)
}

/// Square-ish icon-only button; `active` fills it with the accent.
pub fn icon_button(active: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let t = t(theme);
        if active {
            btn(
                t.accent,
                lerp_color(t.accent, Color::WHITE, 0.12),
                Color::WHITE,
                radius::MD,
            )(theme, status)
        } else {
            btn(Color::TRANSPARENT, t.raised, t.text_dim, radius::MD)(theme, status)
        }
    }
}

/// Sidebar navigation entry; `active` is a continuous 0–1 amount so the
/// highlight can cross-fade between pages.
pub fn nav(active: f32) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let t = t(theme);
        let active = active.clamp(0.0, 1.0);
        let base = tint(t.accent, 0.2 * active);
        let hover = lerp_color(t.raised, tint(t.accent, 0.26), active);
        let text = lerp_color(t.text_dim, t.text, active);
        let background = match status {
            button::Status::Hovered | button::Status::Pressed => hover,
            _ => base,
        };
        button::Style {
            background: Some(background.into()),
            text_color: text,
            border: rounded(radius::MD),
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

#[derive(Clone, Copy)]
pub enum Pill {
    Success,
    Danger,
    /// 0.0 = unselected, 1.0 = selected (continuous for cross-fades).
    Tab(f32),
}

/// Rounded, filled pill — the call actions (Call / Answer / Decline / Hang
/// up) and the segmented tabs.
pub fn pill(kind: Pill) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let t = t(theme);
        let (base, text, filled) = match kind {
            Pill::Success => (
                t.success,
                if t.mode == Mode::Dark {
                    color!(0x06201a)
                } else {
                    Color::WHITE
                },
                true,
            ),
            Pill::Danger => (t.danger, Color::WHITE, true),
            Pill::Tab(a) => (
                lerp_color(Color::TRANSPARENT, t.accent, a),
                lerp_color(t.text_dim, Color::WHITE, a),
                false,
            ),
        };
        let background = match status {
            button::Status::Active => base,
            button::Status::Hovered => lerp_color(base, Color::WHITE, 0.08),
            button::Status::Pressed => lerp_color(base, Color::BLACK, 0.1),
            button::Status::Disabled => base.scale_alpha(0.4),
        };
        let background = if filled {
            top_lit(background)
        } else {
            Background::Color(background)
        };
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
            text_color: if matches!(status, button::Status::Disabled) {
                Color { a: 0.5, ..text }
            } else {
                text
            },
            border: rounded(radius::LG),
            shadow,
            snap: true,
        }
    }
}

fn circle_base(theme: &Theme) -> Color {
    let t = t(theme);
    if t.mode == Mode::Dark {
        Color::from_rgba8(255, 255, 255, 0.07)
    } else {
        Color::from_rgba8(0, 0, 0, 0.06)
    }
}

/// Round button — dialpad keys and the in-call toggles. `active` = accent.
pub fn circle(active: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let t = t(theme);
        let base = if active { t.accent } else { circle_base(theme) };
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
            background: Some(background.into()),
            text_color: if active { Color::WHITE } else { t.text },
            border: rounded(radius::PILL),
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

/// Round/rounded button coloured by a call-state colour (the line buttons).
/// `selected` is a border independent of `fill`, so an idle selected line
/// is still distinguishable.
pub fn circle_state(fill: Option<Color>, selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let t = t(theme);
        let indicator = fill.unwrap_or(t.accent);
        let base = match (fill, selected) {
            (Some(c), true) => c.scale_alpha(0.9),
            (Some(c), false) => tint(c, 0.18),
            (None, true) => tint(t.accent, 0.14),
            (None, false) => circle_base(theme),
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
        let prominent = fill.is_some() && selected;
        button::Style {
            background: Some(if prominent {
                top_lit(background)
            } else {
                Background::Color(background)
            }),
            text_color: if prominent { Color::WHITE } else { t.text },
            border: Border {
                color: if selected {
                    indicator
                } else if fill.is_some() {
                    indicator.scale_alpha(0.5)
                } else {
                    Color::TRANSPARENT
                },
                width: if selected || fill.is_some() { 1.5 } else { 0.0 },
                radius: border::radius(radius::LG),
            },
            shadow: if prominent {
                Shadow {
                    color: indicator.scale_alpha(0.4),
                    offset: iced::Vector::new(0.0, 2.0),
                    blur_radius: 12.0,
                }
            } else {
                Shadow::default()
            },
            snap: true,
        }
    }
}

/// Dialpad key blended toward the accent by `flash` (1.0 = just pressed).
pub fn circle_flash(flash: f32) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let t = t(theme);
        let base = lerp_color(circle_base(theme), t.accent, flash.clamp(0.0, 1.0));
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
            background: Some(background.into()),
            text_color: lerp_color(t.text, Color::WHITE, flash.clamp(0.0, 1.0)),
            border: rounded(radius::PILL),
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

// --------------------------------------------------------------------- widgets

pub fn text_input_style(theme: &Theme, status: text_input::Status) -> text_input::Style {
    let t = t(theme);
    let focused = matches!(status, text_input::Status::Focused { .. });
    text_input::Style {
        background: t.meter_bg.into(),
        border: Border {
            radius: border::radius(radius::MD),
            width: 1.0,
            color: if focused { t.accent } else { t.line },
        },
        icon: t.text_dim,
        placeholder: t.text_dim,
        value: t.text,
        selection: tint(t.accent, 0.35),
    }
}

pub fn pick_list_style(theme: &Theme, status: pick_list::Status) -> pick_list::Style {
    let t = t(theme);
    let hovered = matches!(status, pick_list::Status::Hovered | pick_list::Status::Opened { .. });
    pick_list::Style {
        text_color: t.text,
        placeholder_color: t.text_dim,
        handle_color: t.text_dim,
        background: (if hovered { t.raised_hover } else { t.raised }).into(),
        border: Border {
            radius: border::radius(radius::MD),
            width: 1.0,
            color: t.line,
        },
    }
}

pub fn menu_style(theme: &Theme) -> iced::overlay::menu::Style {
    let t = t(theme);
    iced::overlay::menu::Style {
        background: t.panel.into(),
        border: Border {
            radius: border::radius(radius::MD),
            width: 1.0,
            color: t.line,
        },
        text_color: t.text,
        selected_text_color: Color::WHITE,
        selected_background: t.accent.into(),
        shadow: Shadow {
            color: Color::from_rgba(0.0, 0.0, 0.0, 0.35),
            offset: iced::Vector::new(0.0, 6.0),
            blur_radius: 18.0,
        },
    }
}

pub fn toggler_style(theme: &Theme, status: toggler::Status) -> toggler::Style {
    let t = t(theme);
    let on = matches!(
        status,
        toggler::Status::Active { is_toggled: true } | toggler::Status::Hovered { is_toggled: true }
    );
    let hovered = matches!(status, toggler::Status::Hovered { .. });
    let off_bg = if hovered { t.raised_hover } else { t.raised };
    toggler::Style {
        background: (if on { t.accent } else { off_bg }).into(),
        background_border_width: if on { 0.0 } else { 1.0 },
        background_border_color: t.line,
        foreground: Color::WHITE.into(),
        foreground_border_width: 0.0,
        foreground_border_color: Color::TRANSPARENT,
        text_color: None,
        border_radius: None,
        padding_ratio: 0.2,
    }
}

/// Thin embedded scrollbar.
pub fn scrollable_style(theme: &Theme, status: scrollable::Status) -> scrollable::Style {
    let t = t(theme);
    let active = !matches!(status, scrollable::Status::Active { .. });
    let scroller = scrollable::Scroller {
        background: (if active { t.text_dim } else { tint(t.text_dim, 0.5) }).into(),
        border: rounded(radius::PILL),
    };
    let rail = scrollable::Rail {
        background: None,
        border: rounded(radius::PILL),
        scroller,
    };
    scrollable::Style {
        container: container::Style::default(),
        vertical_rail: rail,
        horizontal_rail: rail,
        gap: None,
        auto_scroll: scrollable::AutoScroll {
            background: t.panel.into(),
            border: rounded(radius::PILL),
            shadow: Shadow::default(),
            icon: t.text_dim,
        },
    }
}

pub fn rule_style(theme: &Theme) -> rule::Style {
    let t = t(theme);
    rule::Style {
        color: t.line,
        radius: border::radius(0.0),
        fill_mode: rule::FillMode::Full,
        snap: true,
    }
}

/// A transparent-rail slider stacked on top of a level meter so the meter
/// shows signal and the thin handle is what you drag to set gain.
pub fn meter_slider(theme: &Theme, _status: slider::Status) -> slider::Style {
    let t = t(theme);
    slider::Style {
        rail: slider::Rail {
            backgrounds: (Color::TRANSPARENT.into(), Color::TRANSPARENT.into()),
            width: 22.0,
            border: Border::default(),
        },
        handle: slider::Handle {
            shape: slider::HandleShape::Rectangle {
                width: 5,
                border_radius: 3.0.into(),
            },
            background: Color::WHITE.into(),
            border_width: 2.0,
            border_color: t.accent,
        },
    }
}
