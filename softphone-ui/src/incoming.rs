//! The incoming-call window: one always-on-top window listing every line
//! that is currently ringing (newest first), each with its own Answer /
//! Decline. A single window rather than one per call because Wayland
//! ignores requested positions, so several popups would land wherever the
//! compositor put them and fight each other for activation.

use crate::app::{AccountSession, App, CallUiState, Message, RegistrationStatus};
use crate::theme::{self, Pill};
use crate::view::{call_avatar_label_for, format_elapsed, muted_text, party_name, pill_call_button};
use crate::{branding, icon};
use iced::widget::{button, column, container, row, rule, text};
use iced::{Alignment, Element, Length, window};
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

pub const WINDOW_WIDTH: f32 = 420.0;
const FIRST_CARD_HEIGHT: f32 = 296.0;
const EXTRA_CARD_HEIGHT: f32 = 236.0;

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
    let calls = ringing_calls(app);
    let mut stack = column![].spacing(0).width(Length::Fill);
    for (i, call) in calls.iter().enumerate() {
        if i > 0 {
            stack = stack.push(rule::horizontal(1));
        }
        stack = stack.push(card(app, call, i == 0));
    }
    container(stack)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(theme::app_background)
        .into()
}

/// One ringing call. `primary` marks the newest card, the one the keyboard
/// shortcuts act on (its button captions say so).
pub fn card<'a>(app: &'a App, call: &Ringing<'a>, primary: bool) -> Element<'a, Message> {
    let palette = theme::oxide_palette();
    let acc: &AccountSession = &app.accounts[call.account];
    let contacts = &app.contacts;
    let caller = call.caller;
    let name = party_name(contacts, caller);
    let avatar_seed = name.unwrap_or(&caller.user);

    let reg_color = match &acc.registration {
        RegistrationStatus::Registered { .. } => palette.success,
        RegistrationStatus::Connecting => palette.warning,
        RegistrationStatus::Failed { .. } | RegistrationStatus::Halted { .. } => palette.danger,
    };
    let account_chip = container(
        row![
            container(text("")).width(8).height(8).style(theme::led(reg_color)),
            text(if acc.config.name.trim().is_empty() {
                acc.config.username.clone()
            } else {
                acc.config.name.clone()
            })
            .size(12),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .padding([4, 10])
    .style(theme::chip);

    let header = row![
        icon::telephone_inbound_fill().size(14).color(palette.warning),
        text("Incoming call").size(13).color(muted_text()),
        text(format_elapsed(call.ringing_since.elapsed()))
            .size(13)
            .font(iced::Font::MONOSPACE)
            .color(muted_text()),
        container(text("")).width(Length::Fill),
        account_chip,
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let avatar = container(text(call_avatar_label_for(contacts, caller)).size(22))
        .width(Length::Fixed(64.0))
        .height(Length::Fixed(64.0))
        .align_x(Alignment::Center)
        .align_y(Alignment::Center)
        .style(theme::avatar_state(theme::avatar_color(avatar_seed), 1.0));

    let mut identity = column![].spacing(2);
    match name {
        Some(name) => {
            identity = identity
                .push(text(name).size(22))
                .push(text(caller.user.as_str()).size(14).color(muted_text()));
        }
        None => identity = identity.push(text(caller.user.as_str()).size(22)),
    }
    let host = caller.host();
    if !host.is_empty() {
        identity = identity.push(text(host.to_string()).size(11).color(muted_text()));
    }

    let who = row![avatar, identity].spacing(14).align_y(Alignment::Center);

    let decline = pill_call_button("Decline", theme::pill(Pill::Danger), icon::telephone_x_fill(), 1.15)
        .on_press(Message::RejectIncomingCall(call.account, call.id.clone()));
    let answer = pill_call_button("Answer", theme::pill(Pill::Success), icon::telephone_fill(), 1.15)
        .on_press(Message::AnswerIncomingCall(call.account, call.id.clone()));
    let caption = |label: &'static str| -> Element<'a, Message> {
        container(text(if primary { label } else { "" }).size(11).color(muted_text()))
            .width(Length::Fill)
            .align_x(Alignment::Center)
            .into()
    };
    let actions = column![
        row![decline, answer].spacing(12),
        row![caption("Esc"), caption("Enter")].spacing(12),
    ]
    .spacing(4);

    let silence = button(text("Silence ringtone").size(12))
        .style(theme::pill(Pill::Neutral))
        .padding([6, 12])
        .on_press(Message::SilenceRingtone);

    column![
        header,
        who,
        actions,
        container(silence).width(Length::Fill).align_x(Alignment::Center),
    ]
    .spacing(16)
    .padding([18, 22])
    .width(Length::Fill)
    .into()
}
