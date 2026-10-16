//! The compact floating call bar: fills the small compact window exactly.

use crate::app::Message;
use crate::contacts::Contact;
use crate::theme::{self, Pill, Tokens};
use crate::ui::components::{
    self, avatar, call_avatar_label, contact_name_for, format_elapsed, muted, pill_call_button,
};
use crate::{branding, icons};
use iced::widget::{column, container, row, text};
use iced::{Alignment, Color, Element, Length};

#[allow(clippy::too_many_arguments)]
pub fn view<'a>(
    t: &Tokens,
    contacts: &'a [Contact],
    number: &'a str,
    remote_name: Option<&'a str>,
    media_ready: bool,
    is_muted: bool,
    on_hold: bool,
    elapsed: std::time::Duration,
) -> Element<'a, Message> {
    let (status, status_color) = if !media_ready {
        ("Connecting…".to_string(), t.warning)
    } else if on_hold {
        ("On hold".to_string(), t.warning)
    } else {
        (format_elapsed(elapsed), t.success)
    };
    let name = contact_name_for(contacts, number).or(remote_name).unwrap_or(number);
    let header = row![
        avatar(
            name,
            call_avatar_label(contacts, number),
            42.0,
            if media_ready && !on_hold { 1.0 } else { 0.0 }
        ),
        column![
            text(name)
                .size(theme::text_size::SUBHEAD)
                .font(branding::UI_FONT_SEMIBOLD),
            text(status).size(theme::text_size::CAPTION).color(status_color),
        ]
        .spacing(2)
        .width(Length::Fill),
        components::icon_button(
            icons::maximize(15.0, muted(t)),
            "Back to the full window",
            false,
            Some(Message::CompactToggled),
        ),
    ]
    .spacing(theme::space::MD)
    .align_y(Alignment::Center);

    let fg = |active: bool| if active { Color::WHITE } else { t.text };
    let controls = row![
        components::with_tooltip(
            components::circle_button(
                if is_muted {
                    icons::mic_off(16.0, fg(true))
                } else {
                    icons::mic(16.0, fg(false))
                },
                42.0,
                theme::circle(is_muted),
            )
            .on_press(Message::MuteToggled),
            if is_muted { "Unmute" } else { "Mute" },
        ),
        components::with_tooltip(
            components::circle_button(
                if on_hold {
                    icons::play(16.0, fg(true))
                } else {
                    icons::pause(16.0, fg(false))
                },
                42.0,
                theme::circle(on_hold),
            )
            .on_press(Message::HoldToggled),
            if on_hold { "Resume" } else { "Hold" },
        ),
        pill_call_button(
            "Hang up",
            theme::pill(Pill::Danger),
            icons::phone_off(15.0, Color::WHITE)
        )
        .on_press(Message::HangUpPressed),
    ]
    .spacing(theme::space::SM)
    .align_y(Alignment::Center);

    container(column![header, controls].spacing(theme::space::MD))
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(theme::space::LG)
        .align_y(Alignment::Center)
        .style(theme::app_background)
        .into()
}
