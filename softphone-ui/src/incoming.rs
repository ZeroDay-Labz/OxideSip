//! The incoming-call window: one always-on-top window listing every line
//! that is currently ringing (newest first), each with its own Answer /
//! Decline. A single window rather than one per call because Wayland
//! ignores requested positions, so several popups would land wherever the
//! compositor put them and fight each other for activation.

use crate::app::{AccountSession, App, CallUiState, Message, RegistrationStatus};
use crate::theme::{self, Pill};
use crate::ui::components::{self, avatar, call_avatar_label_for, format_elapsed, muted, party_name, pill_call_button};
use crate::{branding, icons};
use iced::widget::{button, column, container, row, rule, text};
use iced::{Alignment, Color, Element, Length, window};
use softphone_core::events::{CallId, RemoteParty};
use std::time::Instant;

/// One ringing line, as the window renders it.
pub struct Ringing<'a> {
    pub account: usize,
    pub line_idx: usize,
    pub id: &'a CallId,
    pub caller: &'a RemoteParty,
    pub ringing_since: Instant,
}

/// Every `Incoming` line across all accounts, newest first.
pub fn ringing_calls(app: &App) -> Vec<Ringing<'_>> {
    let mut calls: Vec<Ringing<'_>> = app
        .accounts
        .iter()
        .enumerate()
        .flat_map(|(account, acc)| {
            acc.lines
                .iter()
                .enumerate()
                .filter_map(move |(line_idx, line)| match line {
                    CallUiState::Incoming {
                        id,
                        caller,
                        ringing_since,
                        ..
                    } => Some(Ringing {
                        account,
                        line_idx,
                        id,
                        caller,
                        ringing_since: *ringing_since,
                    }),
                    _ => None,
                })
        })
        .collect();
    calls.sort_by_key(|c| std::cmp::Reverse(c.ringing_since));
    calls
}

pub const WINDOW_WIDTH: f32 = 440.0;
const FIRST_CARD_HEIGHT: f32 = 300.0;
const EXTRA_CARD_HEIGHT: f32 = 240.0;

/// The window is content-sized (not resizable), so it grows with the
/// number of ringing calls instead of clipping anything.
pub fn window_size(cards: usize) -> iced::Size {
    let extra = cards.saturating_sub(1) as f32;
    iced::Size::new(WINDOW_WIDTH, FIRST_CARD_HEIGHT + extra * EXTRA_CARD_HEIGHT)
}

pub fn window_settings(cards: usize) -> window::Settings {
    let size = window_size(cards);
    window::Settings {
        resizable: false,
        minimizable: false,
        level: window::Level::AlwaysOnTop,
        ..branding::window_settings(size, Some(size))
    }
}

pub fn title(app: &App) -> String {
    let calls = ringing_calls(app);
    match calls.as_slice() {
        [] => format!("{} — Incoming call", branding::APP_NAME),
        [one] => format!(
            "Incoming call — {}",
            party_name(&app.contacts, one.caller).unwrap_or(&one.caller.user)
        ),
        many => format!("{} incoming calls — {}", many.len(), branding::APP_NAME),
    }
}

pub fn window_view(app: &App) -> Element<'_, Message> {
    let t = theme::tokens(app.mode());
    let calls = ringing_calls(app);
    let mut stack = column![].spacing(0).width(Length::Fill);
    for (i, call) in calls.iter().enumerate() {
        if i > 0 {
            stack = stack.push(rule::horizontal(1).style(theme::rule_style));
        }
        stack = stack.push(card(app, &t, call, i == 0));
    }
    container(stack)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(theme::app_background)
        .into()
}

/// One ringing call. `primary` marks the newest card, the one the keyboard
/// shortcuts act on (its button captions say so).
fn card<'a>(app: &'a App, t: &theme::Tokens, call: &Ringing<'a>, primary: bool) -> Element<'a, Message> {
    let acc: &AccountSession = &app.accounts[call.account];
    let contacts = &app.contacts;
    let caller = call.caller;
    let name = party_name(contacts, caller);
    let seed = name.unwrap_or(&caller.user);

    let reg_color = match &acc.registration {
        RegistrationStatus::Registered { .. } => t.success,
        RegistrationStatus::Connecting => t.warning,
        RegistrationStatus::Failed { .. } | RegistrationStatus::Halted { .. } => t.danger,
    };
    let account_name = if acc.config.name.trim().is_empty() {
        acc.config.username.clone()
    } else {
        acc.config.name.clone()
    };

    let header = row![
        icons::bell_ring(16.0, t.warning),
        text("Incoming call").size(theme::text_size::BODY).color(muted(t)),
        text(format_elapsed(call.ringing_since.elapsed()))
            .size(theme::text_size::BODY)
            .font(iced::Font::MONOSPACE)
            .color(muted(t)),
        container(text("")).width(Length::Fill),
        components::status_chip(reg_color, account_name),
    ]
    .spacing(theme::space::SM)
    .align_y(Alignment::Center);

    let mut identity = column![].spacing(2);
    match name {
        Some(name) => {
            identity = identity
                .push(
                    text(name)
                        .size(theme::text_size::TITLE + 2.0)
                        .font(branding::UI_FONT_SEMIBOLD),
                )
                .push(
                    text(caller.user.as_str())
                        .size(theme::text_size::SUBHEAD)
                        .color(muted(t)),
                );
        }
        None => {
            identity = identity.push(
                text(caller.user.as_str())
                    .size(theme::text_size::TITLE + 2.0)
                    .font(branding::UI_FONT_SEMIBOLD),
            )
        }
    }
    let host = caller.host();
    if !host.is_empty() {
        identity = identity.push(text(host.to_string()).size(theme::text_size::CAPTION).color(muted(t)));
    }
    let who = row![
        avatar(seed, call_avatar_label_for(contacts, caller), 64.0, 1.0),
        identity
    ]
    .spacing(theme::space::LG)
    .align_y(Alignment::Center);

    let answer_fg = if t.mode == theme::Mode::Dark {
        iced::color!(0x06201a)
    } else {
        Color::WHITE
    };
    let decline = pill_call_button(
        "Decline",
        theme::pill(Pill::Danger),
        icons::phone_off(18.0, Color::WHITE),
    )
    .on_press(Message::RejectIncomingCall(call.account, call.id.clone()))
    .padding([theme::space::LG, theme::space::XL]);
    let answer = pill_call_button("Answer", theme::pill(Pill::Success), icons::phone(18.0, answer_fg))
        .on_press(Message::AnswerIncomingCall(call.account, call.id.clone()))
        .padding([theme::space::LG, theme::space::XL]);
    let caption = |label: &'static str| -> Element<'a, Message> {
        container(
            text(if primary { label } else { "" })
                .size(theme::text_size::CAPTION)
                .color(muted(t)),
        )
        .width(Length::Fill)
        .align_x(Alignment::Center)
        .into()
    };
    let actions = column![
        row![decline, answer].spacing(theme::space::MD),
        row![caption("Esc"), caption("Enter")].spacing(theme::space::MD),
    ]
    .spacing(theme::space::XS);

    let silence = button(
        row![
            icons::bell_off(13.0, muted(t)),
            text("Silence ringtone").size(theme::text_size::CAPTION + 1.0)
        ]
        .spacing(theme::space::SM)
        .align_y(Alignment::Center),
    )
    .style(theme::button_ghost)
    .padding([theme::space::XS + 2.0, theme::space::MD])
    .on_press(Message::SilenceRingtone);

    column![
        header,
        who,
        actions,
        container(silence).width(Length::Fill).align_x(Alignment::Center),
    ]
    .spacing(theme::space::LG)
    .padding([theme::space::LG + 2.0, theme::space::XL])
    .width(Length::Fill)
    .into()
}
