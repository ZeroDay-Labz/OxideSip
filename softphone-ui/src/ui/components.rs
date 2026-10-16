//! Small building blocks shared by every page: avatars, labelled fields,
//! sections, tooltips, empty states, icon buttons and name resolution.

use crate::app::Message;
use crate::contacts::Contact;
use crate::theme::{self, Tokens};
use crate::{branding, number};
use iced::widget::{button, column, container, pick_list, row, text, text_input, toggler, tooltip};
use iced::{Alignment, Color, Element, Length};
use softphone_core::events::RemoteParty;

/// Dim text colour for the current tokens.
pub fn muted(t: &Tokens) -> Color {
    t.text_dim
}

pub fn format_elapsed(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

/// Wraps `content` with a hover tooltip.
pub fn with_tooltip<'a>(content: impl Into<Element<'a, Message>>, label: impl Into<String>) -> Element<'a, Message> {
    tooltip(
        content,
        container(text(label.into()).size(theme::text_size::CAPTION + 1.0)).padding([6, 10]),
        tooltip::Position::Top,
    )
    .style(theme::overlay)
    .gap(6)
    .into()
}

/// Caption above an input.
pub fn field<'a>(t: &Tokens, label: &'a str, input: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    column![
        text(label).size(theme::text_size::CAPTION).color(muted(t)),
        input.into()
    ]
    .spacing(theme::space::XS + 2.0)
    .into()
}

/// A titled panel grouping related controls.
pub fn section<'a>(title: &'a str, content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(
        column![
            text(title)
                .size(theme::text_size::SUBHEAD)
                .font(branding::UI_FONT_SEMIBOLD),
            content.into(),
        ]
        .spacing(theme::space::MD),
    )
    .style(theme::panel)
    .padding(theme::space::LG)
    .width(Length::Fill)
    .into()
}

/// A calm "nothing here yet" placeholder.
pub fn empty_state<'a>(t: &Tokens, glyph: Element<'a, Message>, title: &'a str, hint: &'a str) -> Element<'a, Message> {
    let mut col = column![glyph, text(title).size(theme::text_size::SUBHEAD)]
        .spacing(theme::space::MD)
        .align_x(Alignment::Center);
    if !hint.is_empty() {
        col = col.push(text(hint).size(theme::text_size::BODY).color(muted(t)));
    }
    container(col)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Alignment::Center)
        .align_y(Alignment::Center)
        .padding(theme::space::XL)
        .into()
}

/// Icon-only button with a tooltip. `active` fills it with the accent.
pub fn icon_button<'a>(
    icon: Element<'a, Message>,
    label: impl Into<String>,
    active: bool,
    on_press: Option<Message>,
) -> Element<'a, Message> {
    let mut b = button(container(icon).align_x(Alignment::Center).align_y(Alignment::Center))
        .style(theme::icon_button(active))
        .padding(8);
    if let Some(m) = on_press {
        b = b.on_press(m);
    }
    with_tooltip(b, label)
}

/// Round button of a fixed diameter (dialpad keys, in-call toggles).
pub fn circle_button<'a>(
    content: impl Into<Element<'a, Message>>,
    diameter: f32,
    style: impl Fn(&iced::Theme, button::Status) -> button::Style + 'a,
) -> button::Button<'a, Message> {
    button(
        container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Alignment::Center)
            .align_y(Alignment::Center),
    )
    .style(style)
    .width(Length::Fixed(diameter))
    .height(Length::Fixed(diameter))
    .padding(0)
}

/// Wide rounded pill carrying an icon and a label — the call actions.
pub fn pill_call_button<'a>(
    label: &'a str,
    style: impl Fn(&iced::Theme, button::Status) -> button::Style + 'a,
    icon: Element<'a, Message>,
) -> button::Button<'a, Message> {
    button(
        container(
            row![
                icon,
                text(label)
                    .size(theme::text_size::SUBHEAD)
                    .font(branding::UI_FONT_MEDIUM)
            ]
            .spacing(theme::space::SM)
            .align_y(Alignment::Center),
        )
        .width(Length::Fill)
        .align_x(Alignment::Center),
    )
    .style(style)
    .padding([theme::space::MD, theme::space::XL])
    .width(Length::Fill)
}

/// Standard text button.
pub fn text_button<'a>(
    label: &'a str,
    style: impl Fn(&iced::Theme, button::Status) -> button::Style + 'a,
) -> button::Button<'a, Message> {
    button(text(label).size(theme::text_size::BODY).font(branding::UI_FONT_MEDIUM))
        .style(style)
        .padding([theme::space::SM + 1.0, theme::space::LG])
}

/// A text input with the app's style applied.
pub fn input<'a>(placeholder: &'a str, value: &'a str) -> text_input::TextInput<'a, Message> {
    text_input(placeholder, value)
        .style(theme::text_input_style)
        .padding([theme::space::SM + 1.0, theme::space::MD])
        .size(theme::text_size::BODY)
}

/// A pick list with the app's style applied.
pub fn picker<'a, T, L, V>(
    options: L,
    selected: Option<V>,
    on_select: impl Fn(T) -> Message + 'a,
) -> pick_list::PickList<'a, T, L, V, Message>
where
    T: ToString + PartialEq + Clone + 'a,
    L: std::borrow::Borrow<[T]> + 'a,
    V: std::borrow::Borrow<T> + 'a,
{
    pick_list(options, selected, on_select)
        .style(theme::pick_list_style)
        .menu_style(theme::menu_style)
        .padding([theme::space::SM + 1.0, theme::space::MD])
        .text_size(theme::text_size::BODY)
}

pub fn toggle_row<'a>(
    t: &Tokens,
    title: &'a str,
    detail: &'a str,
    value: bool,
    on_toggle: impl Fn(bool) -> Message + 'a,
) -> Element<'a, Message> {
    let mut labels = column![text(title).size(theme::text_size::BODY)]
        .spacing(2)
        .width(Length::Fill);
    if !detail.is_empty() {
        labels = labels.push(text(detail).size(theme::text_size::CAPTION).color(muted(t)));
    }
    row![
        labels,
        toggler(value).on_toggle(on_toggle).style(theme::toggler_style).size(22)
    ]
    .spacing(theme::space::MD)
    .align_y(Alignment::Center)
    .into()
}

/// Small coloured dot.
pub fn dot<'a>(color: Color, size: f32) -> Element<'a, Message> {
    container(text(""))
        .width(Length::Fixed(size))
        .height(Length::Fixed(size))
        .style(theme::led(color))
        .into()
}

/// Chip with a dot and a label, tinted by `color`.
pub fn status_chip<'a>(color: Color, label: impl Into<String>) -> Element<'a, Message> {
    container(
        row![dot(color, 7.0), text(label.into()).size(theme::text_size::CAPTION)]
            .spacing(6)
            .align_y(Alignment::Center),
    )
    .padding([3, 9])
    .style(theme::chip_tinted(color))
    .into()
}

// ----------------------------------------------------------------- identity

/// First alphanumeric of a label, uppercased; `#` if none.
pub fn initial(label: &str) -> String {
    label
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "#".to_string())
}

pub fn contact_initials(name: &str) -> String {
    let mut letters = name.split_whitespace().filter_map(|w| w.chars().next());
    match (letters.next(), letters.next()) {
        (Some(a), Some(b)) => format!("{}{}", a.to_ascii_uppercase(), b.to_ascii_uppercase()),
        (Some(a), None) => a.to_ascii_uppercase().to_string(),
        _ => "#".to_string(),
    }
}

/// Saved contact name for a dialled/received number, by normalised match.
pub fn contact_name_for<'a>(contacts: &'a [Contact], raw: &str) -> Option<&'a str> {
    contacts
        .iter()
        .find(|c| number::matches(&c.number, raw))
        .map(|c| c.name.as_str())
}

/// Contact name, else the network's display name, else nothing.
pub fn party_name<'a>(contacts: &'a [Contact], party: &'a RemoteParty) -> Option<&'a str> {
    contact_name_for(contacts, &party.user).or(party.display_name.as_deref())
}

/// Avatar label for a number: contact initials, else the last two digits.
pub fn call_avatar_label(contacts: &[Contact], raw: &str) -> String {
    if let Some(name) = contact_name_for(contacts, raw) {
        return contact_initials(name);
    }
    let digits: String = raw.chars().rev().filter(|c| c.is_ascii_digit()).take(2).collect();
    if digits.is_empty() {
        "#".to_string()
    } else {
        digits.chars().rev().collect()
    }
}

pub fn call_avatar_label_for(contacts: &[Contact], party: &RemoteParty) -> String {
    match party_name(contacts, party) {
        Some(name) => contact_initials(name),
        None => call_avatar_label(contacts, &party.user),
    }
}

/// Round avatar with initials; `glow` 0–1 lights it up for a live party.
pub fn avatar<'a>(seed: &str, label: String, size: f32, glow: f32) -> Element<'a, Message> {
    container(text(label).size((size * 0.34).round()).font(branding::UI_FONT_SEMIBOLD))
        .width(Length::Fixed(size))
        .height(Length::Fixed(size))
        .align_x(Alignment::Center)
        .align_y(Alignment::Center)
        .style(theme::avatar_state(theme::avatar_color(seed), glow))
        .into()
}

// -------------------------------------------------------------- audio devices

/// Adapter so `AudioDevice` can be used as a `pick_list` item.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceOption {
    pub id: String,
    pub description: String,
}

impl std::fmt::Display for DeviceOption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.description)
    }
}

/// Sentinel id meaning "no explicit device, use the system default".
pub const SYSTEM_DEFAULT_ID: &str = "";

pub fn device_picker<'a>(
    t: &Tokens,
    label: &'a str,
    default_label: &str,
    devices: &[softphone_media::AudioDevice],
    app_streams: &[softphone_media::AudioDevice],
    selected_id: &Option<String>,
    on_selected: impl Fn(String) -> Message + 'a,
) -> Element<'a, Message> {
    let mut options: Vec<DeviceOption> = vec![DeviceOption {
        id: SYSTEM_DEFAULT_ID.to_string(),
        description: default_label.to_string(),
    }];
    options.extend(app_streams.iter().map(|d| DeviceOption {
        id: d.id.clone(),
        description: format!("App: {}", d.description),
    }));
    options.extend(devices.iter().map(|d| DeviceOption {
        id: d.id.clone(),
        description: d.description.clone(),
    }));
    let selected = options
        .iter()
        .find(|o| o.id == selected_id.clone().unwrap_or_default())
        .cloned();
    field(
        t,
        label,
        picker(options, selected, move |opt: DeviceOption| on_selected(opt.id)).width(Length::Fill),
    )
}
