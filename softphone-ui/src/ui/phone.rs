//! The Phone page: dial card on the left, the five lines and the selected
//! line's call card on the right.

use crate::app::{AccountSession, App, CallUiState, Message};
use crate::contacts::Contact;
use crate::history::CallDirection;
use crate::theme::{self, Pill, Tokens};
use crate::ui::components::{
    self, avatar, call_avatar_label, call_avatar_label_for, circle_button, contact_name_for, format_elapsed, input,
    muted, party_name, pill_call_button, with_tooltip,
};
use crate::{branding, icons};
use iced::widget::{button, column, container, row, scrollable, slider, stack, text};
use iced::{Alignment, Color, Element, Length};
use std::time::Instant;

const DIAL_CARD_WIDTH: f32 = 340.0;

pub fn view<'a>(app: &'a App, t: &Tokens) -> Element<'a, Message> {
    let Some(acc) = app.selected() else {
        return welcome(t);
    };
    let right = column![lines_card(app, acc, t), call_card(app, acc, t)]
        .spacing(theme::space::LG)
        .width(Length::Fill)
        .height(Length::Fill);
    row![dial_card(app, acc, t), right]
        .spacing(theme::space::LG)
        .height(Length::Fill)
        .into()
}

fn welcome<'a>(t: &Tokens) -> Element<'a, Message> {
    container(
        column![
            icons::logo(48.0, t.accent),
            text("Welcome to OxideSip")
                .size(theme::text_size::TITLE)
                .font(branding::UI_FONT_SEMIBOLD),
            text("Add a SIP account to start placing and receiving calls.")
                .size(theme::text_size::BODY)
                .color(muted(t)),
            button(text("Add your first account").size(theme::text_size::BODY))
                .style(theme::button_primary)
                .padding([theme::space::MD, theme::space::XL])
                .on_press(Message::OpenSipSettings),
        ]
        .spacing(theme::space::LG)
        .align_x(Alignment::Center),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .align_x(Alignment::Center)
    .align_y(Alignment::Center)
    .style(theme::panel)
    .into()
}

// ------------------------------------------------------------------ dial card

fn dial_card<'a>(app: &'a App, acc: &'a AccountSession, t: &Tokens) -> Element<'a, Message> {
    let idle = matches!(acc.selected_call(), CallUiState::Idle);
    let field = input("Enter a number", &app.dial_input)
        .id(crate::app::DIAL_INPUT_ID)
        .on_input(Message::DialInputChanged)
        .on_paste(Message::DialInputPasted)
        .on_submit(Message::CallPressed)
        .size(theme::text_size::TITLE + 2.0)
        .padding([theme::space::MD, theme::space::LG])
        .align_x(Alignment::Center);
    let recent = components::icon_button(
        if app.dial_history_open {
            icons::chevron_up(16.0, muted(t))
        } else {
            icons::clock(16.0, muted(t))
        },
        "Recent numbers",
        app.dial_history_open,
        Some(Message::DialHistoryToggled),
    );
    let input_row = row![field, recent].spacing(theme::space::SM).align_y(Alignment::Center);

    let has_last_outgoing = app.call_history.iter().any(|e| e.direction == CallDirection::Outgoing);
    let redial = circle_button(icons::refresh(18.0, t.text), 52.0, theme::circle(false));
    let redial = if has_last_outgoing && idle {
        redial.on_press(Message::RedialPressed)
    } else {
        redial
    };
    let backspace = circle_button(icons::arrow_left(18.0, t.text), 52.0, theme::circle(false));
    let backspace = if !app.dial_input.is_empty() {
        backspace.on_press(Message::DialBackspace)
    } else {
        backspace
    };
    let call = pill_call_button(
        "Call",
        theme::pill(Pill::Success),
        icons::phone(16.0, call_text_color(t)),
    );
    let call = if app.dial_input.is_empty() || !idle {
        call
    } else {
        call.on_press(Message::CallPressed)
    };

    let mut col = column![input_row].spacing(theme::space::LG).align_x(Alignment::Center);
    if app.dial_history_open {
        col = col.push(recent_numbers(app, t));
    }
    col = col.push(dialpad(app.dialpad_flash, t)).push(
        row![
            with_tooltip(redial, "Redial the last number"),
            call,
            with_tooltip(backspace, "Delete the last digit"),
        ]
        .spacing(theme::space::MD)
        .align_y(Alignment::Center),
    );

    container(col.width(Length::Fill))
        .padding(theme::space::XL)
        .width(Length::Fixed(DIAL_CARD_WIDTH))
        .height(Length::Fill)
        .style(theme::panel)
        .into()
}

fn call_text_color(t: &Tokens) -> Color {
    if t.mode == theme::Mode::Dark {
        iced::color!(0x06201a)
    } else {
        Color::WHITE
    }
}

fn recent_numbers<'a>(app: &'a App, t: &Tokens) -> Element<'a, Message> {
    let mut seen = std::collections::HashSet::new();
    let mut numbers = Vec::new();
    for entry in app.call_history.iter().rev() {
        if entry.direction != CallDirection::Outgoing {
            continue;
        }
        if seen.insert(entry.number.clone()) {
            numbers.push(entry.number.clone());
        }
        if numbers.len() >= 10 {
            break;
        }
    }
    if numbers.is_empty() {
        return container(text("No recent numbers").size(theme::text_size::BODY).color(muted(t)))
            .padding(theme::space::MD)
            .width(Length::Fill)
            .align_x(Alignment::Center)
            .style(theme::raised)
            .into();
    }
    let mut list = column![].spacing(theme::space::XS);
    for number in numbers {
        let name = contact_name_for(&app.contacts, &number).map(str::to_string);
        let label = match name {
            Some(name) => format!("{name} · {number}"),
            None => number.clone(),
        };
        list = list.push(
            button(text(label).size(theme::text_size::BODY))
                .style(theme::button_ghost)
                .padding([theme::space::SM, theme::space::MD])
                .width(Length::Fill)
                .on_press(Message::DialHistorySelected(number)),
        );
    }
    container(
        scrollable(list)
            .style(theme::scrollable_style)
            .height(Length::Fixed(150.0)),
    )
    .padding(theme::space::SM)
    .width(Length::Fill)
    .style(theme::raised)
    .into()
}

const DIALPAD_KEYS: [[char; 3]; 4] = [['1', '2', '3'], ['4', '5', '6'], ['7', '8', '9'], ['*', '0', '#']];
const DIALPAD_FLASH_MS: f32 = 260.0;

fn dialpad_letters(key: char) -> &'static str {
    match key {
        '2' => "ABC",
        '3' => "DEF",
        '4' => "GHI",
        '5' => "JKL",
        '6' => "MNO",
        '7' => "PQRS",
        '8' => "TUV",
        '9' => "WXYZ",
        '0' => "+",
        _ => "",
    }
}

pub fn dialpad<'a>(flash: Option<(char, Instant)>, t: &Tokens) -> Element<'a, Message> {
    let key_size = 64.0;
    let mut rows = column![].spacing(theme::space::SM);
    for keys in DIALPAD_KEYS {
        let mut r = row![].spacing(theme::space::SM);
        for key in keys {
            let letters = dialpad_letters(key);
            let label: Element<'_, Message> = column![
                text(key.to_string())
                    .size(theme::text_size::TITLE)
                    .font(branding::UI_FONT_MEDIUM),
                text(letters).size(theme::text_size::CAPTION - 1.0).color(muted(t)),
            ]
            .spacing(0)
            .align_x(Alignment::Center)
            .into();
            let amount = match flash {
                Some((k, at)) if k == key => {
                    let p = (at.elapsed().as_secs_f32() * 1000.0 / DIALPAD_FLASH_MS).min(1.0);
                    (1.0 - p).powi(2)
                }
                _ => 0.0,
            };
            r = r.push(
                circle_button(label, key_size, theme::circle_flash(amount)).on_press(Message::DialpadPressed(key)),
            );
        }
        rows = rows.push(r);
    }
    container(rows).width(Length::Fill).align_x(Alignment::Center).into()
}

// ----------------------------------------------------------------- lines card

fn lines_card<'a>(app: &'a App, acc: &'a AccountSession, t: &Tokens) -> Element<'a, Message> {
    let mut list = column![].spacing(theme::space::XS);
    for line in 1..=5u8 {
        let idx = (line - 1) as usize;
        let call = &acc.lines[idx];
        let selected = line == acc.selected_line;
        let joined = acc.joined[idx];
        let live = matches!(call, CallUiState::Active { on_hold, .. } if joined.is_some() || (selected && !on_hold))
            || matches!(call, CallUiState::EarlyMedia { .. } if selected);
        let armed_idle = selected && acc.line_open && matches!(call, CallUiState::Idle);
        let (fill, who, state, timer): (Option<Color>, String, String, Option<Instant>) = match call {
            CallUiState::Idle if armed_idle => (
                Some(t.warning),
                "Dial tone".to_string(),
                "ready to dial".to_string(),
                acc.line_open_at,
            ),
            CallUiState::Idle => (
                None,
                "Free".to_string(),
                acc.last_call_status[idx].clone().unwrap_or_default(),
                None,
            ),
            CallUiState::Incoming {
                caller, ringing_since, ..
            } => (
                Some(t.warning),
                party_name(&app.contacts, caller).unwrap_or(&caller.user).to_string(),
                "ringing".to_string(),
                Some(*ringing_since),
            ),
            CallUiState::Outgoing { number, started_at, .. } | CallUiState::EarlyMedia { number, started_at, .. } => (
                Some(t.accent),
                contact_name_for(&app.contacts, number).unwrap_or(number).to_string(),
                "calling…".to_string(),
                Some(*started_at),
            ),
            CallUiState::Active {
                number,
                remote_name,
                on_hold,
                answered_at,
                ..
            } => (
                Some(if live { t.danger } else { t.accent }),
                contact_name_for(&app.contacts, number)
                    .or(remote_name.as_deref())
                    .unwrap_or(number)
                    .to_string(),
                match (joined, on_hold) {
                    (Some(partner), _) => format!("joined with L{partner}"),
                    (None, true) => "on hold".to_string(),
                    (None, false) => "connected".to_string(),
                },
                Some(*answered_at),
            ),
        };
        let led_color = if live {
            t.danger
        } else if fill.is_some() {
            t.warning
        } else {
            theme::tint(t.text_dim, 0.4)
        };
        let mut content = row![
            components::dot(led_color, 8.0),
            text(format!("L{line}"))
                .size(theme::text_size::BODY)
                .font(branding::UI_FONT_SEMIBOLD)
                .width(Length::Fixed(28.0)),
            column![
                text(who).size(theme::text_size::BODY),
                text(state).size(theme::text_size::CAPTION).color(muted(t)),
            ]
            .spacing(1)
            .width(Length::Fill),
        ]
        .spacing(theme::space::MD)
        .align_y(Alignment::Center);
        if let Some(since) = timer {
            content = content.push(
                text(format_elapsed(app.elapsed(since)))
                    .size(theme::text_size::CAPTION)
                    .font(iced::Font::MONOSPACE)
                    .color(muted(t)),
            );
        }
        let b = button(content)
            .style(theme::circle_state(fill, selected))
            .padding([theme::space::SM, theme::space::MD])
            .width(Length::Fill)
            .on_press(Message::LineSelected(line));
        let tip = if acc.pending_join.is_some() && !selected {
            "Tap to join with the selected call".to_string()
        } else if armed_idle {
            format!("Line {line} is open — tap to close it")
        } else {
            format!("Select line {line}")
        };
        list = list.push(with_tooltip(b, tip));
    }

    let secondary_configured = app.settings.secondary_input_target.is_some();
    let secondary_on = matches!(
        acc.selected_call(),
        CallUiState::Active {
            secondary_input_on: true,
            ..
        }
    );
    let secondary = components::icon_button(
        icons::audio_lines(16.0, if secondary_on { Color::WHITE } else { muted(t) }),
        if !secondary_configured {
            "Pick a secondary input in Settings ▸ Calls to enable this"
        } else if secondary_on {
            "Stop mixing the secondary input into this call"
        } else {
            "Mix the secondary input (e.g. Discord) into this call"
        },
        secondary_on,
        secondary_configured.then_some(Message::ToggleSecondaryInputInjection),
    );
    let header = row![
        text("Lines")
            .size(theme::text_size::SUBHEAD)
            .font(branding::UI_FONT_SEMIBOLD)
            .width(Length::Fill),
        secondary,
        components::icon_button(
            icons::plus(16.0, muted(t)),
            "New call on the next free line",
            false,
            Some(Message::AddCallPressed),
        ),
    ]
    .spacing(theme::space::XS)
    .align_y(Alignment::Center);

    container(column![header, list].spacing(theme::space::MD))
        .padding(theme::space::LG)
        .width(Length::Fill)
        .style(theme::panel)
        .into()
}

// ------------------------------------------------------------------ call card

fn call_card<'a>(app: &'a App, acc: &'a AccountSession, t: &Tokens) -> Element<'a, Message> {
    let (content, accent): (Element<'a, Message>, Option<Color>) = match acc.selected_call() {
        CallUiState::Idle => (idle_card(acc, t), None),
        CallUiState::Incoming {
            caller, ringing_since, ..
        } => (
            incoming_card(app, &app.contacts, caller, *ringing_since, t),
            Some(t.warning),
        ),
        CallUiState::Outgoing { number, started_at, .. } | CallUiState::EarlyMedia { number, started_at, .. } => (
            outgoing_card(app, &app.contacts, number, *started_at, t),
            Some(t.accent),
        ),
        CallUiState::Active {
            number,
            remote_name,
            media,
            dtmf_feedback,
            answered_at,
            input_level,
            output_level,
            muted,
            output_volume,
            input_volume,
            on_hold,
            transfer_input,
            avatar_glow,
            ..
        } => (
            active_card(
                app,
                acc,
                t,
                ActiveView {
                    number,
                    remote_name: remote_name.as_deref(),
                    media_ready: app.media_ready(media),
                    glow: avatar_glow.interpolate(0.0_f32, 1.0_f32, Instant::now()),
                    answered_at: *answered_at,
                    input_level: *input_level,
                    output_level: *output_level,
                    muted: *muted,
                    output_volume: *output_volume,
                    input_volume: *input_volume,
                    on_hold: *on_hold,
                    transfer_input: transfer_input.as_deref(),
                    dtmf_feedback,
                },
            ),
            Some(if *on_hold { t.warning } else { t.success }),
        ),
    };
    let card = container(content)
        .padding(theme::space::XL)
        .width(Length::Fill)
        .height(Length::Fill);
    match accent {
        Some(color) => card.style(theme::panel_accent(color)).into(),
        None => card.style(theme::panel).into(),
    }
}

fn idle_card<'a>(acc: &'a AccountSession, t: &Tokens) -> Element<'a, Message> {
    let line = acc.selected_line;
    let idx = acc.selected_idx();
    let (title, hint) = if acc.line_open {
        (
            format!("Line {line} is open"),
            "Dial tone is playing — enter a number and press Call, or tap the line again to close it.".to_string(),
        )
    } else {
        (
            format!("Line {line} is free"),
            match &acc.last_call_status[idx] {
                Some(status) => format!("Last call: {status}."),
                None => "Enter a number on the left, pick a contact, or wait for a call.".to_string(),
            },
        )
    };
    container(
        column![
            icons::phone(36.0, theme::tint(t.text_dim, 0.6)),
            text(title)
                .size(theme::text_size::TITLE)
                .font(branding::UI_FONT_SEMIBOLD),
            text(hint)
                .size(theme::text_size::BODY)
                .color(muted(t))
                .align_x(Alignment::Center),
        ]
        .spacing(theme::space::MD)
        .align_x(Alignment::Center)
        .max_width(420.0),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .align_x(Alignment::Center)
    .align_y(Alignment::Center)
    .into()
}

fn identity_block<'a>(
    name: Option<&'a str>,
    number: &'a str,
    status: String,
    status_color: Color,
    t: &Tokens,
) -> Element<'a, Message> {
    let mut col = column![].spacing(2);
    match name {
        Some(name) => {
            col = col
                .push(
                    text(name)
                        .size(theme::text_size::TITLE + 2.0)
                        .font(branding::UI_FONT_SEMIBOLD),
                )
                .push(text(number).size(theme::text_size::BODY).color(muted(t)));
        }
        None => {
            col = col.push(
                text(number)
                    .size(theme::text_size::TITLE + 2.0)
                    .font(branding::UI_FONT_SEMIBOLD),
            )
        }
    }
    col.push(text(status).size(theme::text_size::BODY).color(status_color))
        .into()
}

fn incoming_card<'a>(
    app: &'a App,
    contacts: &'a [Contact],
    caller: &'a softphone_core::events::RemoteParty,
    ringing_since: Instant,
    t: &Tokens,
) -> Element<'a, Message> {
    let name = party_name(contacts, caller);
    let seed = name.unwrap_or(&caller.user);
    let header = row![
        avatar(seed, call_avatar_label_for(contacts, caller), 72.0, 1.0),
        identity_block(
            name,
            &caller.user,
            format!("Incoming call · {}", format_elapsed(app.elapsed(ringing_since))),
            t.warning,
            t,
        ),
    ]
    .spacing(theme::space::LG)
    .align_y(Alignment::Center);
    column![
        header,
        container(text("")).height(Length::Fill),
        row![
            pill_call_button(
                "Decline",
                theme::pill(Pill::Danger),
                icons::phone_off(16.0, Color::WHITE)
            )
            .on_press(Message::RejectPressed),
            pill_call_button(
                "Answer",
                theme::pill(Pill::Success),
                icons::phone(16.0, call_text_color(t))
            )
            .on_press(Message::AnswerPressed),
        ]
        .spacing(theme::space::MD),
    ]
    .spacing(theme::space::LG)
    .height(Length::Fill)
    .into()
}

fn outgoing_card<'a>(
    app: &'a App,
    contacts: &'a [Contact],
    number: &'a str,
    started_at: Instant,
    t: &Tokens,
) -> Element<'a, Message> {
    let header = row![
        avatar(number, call_avatar_label(contacts, number), 72.0, 1.0),
        identity_block(
            contact_name_for(contacts, number),
            number,
            format!("Calling… {}", format_elapsed(app.elapsed(started_at))),
            t.accent,
            t,
        ),
    ]
    .spacing(theme::space::LG)
    .align_y(Alignment::Center);
    column![
        header,
        container(text("")).height(Length::Fill),
        pill_call_button(
            "Hang up",
            theme::pill(Pill::Danger),
            icons::phone_off(16.0, Color::WHITE)
        )
        .on_press(Message::HangUpPressed),
    ]
    .spacing(theme::space::LG)
    .height(Length::Fill)
    .into()
}

struct ActiveView<'a> {
    number: &'a str,
    remote_name: Option<&'a str>,
    media_ready: bool,
    glow: f32,
    answered_at: Instant,
    input_level: f32,
    output_level: f32,
    muted: bool,
    output_volume: f32,
    input_volume: f32,
    on_hold: bool,
    transfer_input: Option<&'a str>,
    dtmf_feedback: &'a [char],
}

enum JoinUi {
    Hidden,
    Available,
    Pending,
    Joined { partner: u8 },
}

fn join_ui(acc: &AccountSession) -> JoinUi {
    let idx = acc.selected_idx();
    if acc.pending_join == Some(acc.selected_line) {
        return JoinUi::Pending;
    }
    if let Some(partner) = acc.joined[idx] {
        return JoinUi::Joined { partner };
    }
    let has_other_active = (1..=5u8)
        .any(|l| l != acc.selected_line && matches!(acc.lines.get((l - 1) as usize), Some(CallUiState::Active { .. })));
    if has_other_active {
        JoinUi::Available
    } else {
        JoinUi::Hidden
    }
}

fn active_card<'a>(app: &'a App, acc: &'a AccountSession, t: &Tokens, v: ActiveView<'a>) -> Element<'a, Message> {
    let join = join_ui(acc);
    let (status, status_color) = match (&join, v.media_ready, v.on_hold) {
        (JoinUi::Joined { partner }, _, _) => (format!("Joined with line {partner}"), t.success),
        (_, false, _) => ("Connecting audio…".to_string(), t.warning),
        (_, _, true) => ("On hold".to_string(), t.warning),
        _ => (format_elapsed(app.elapsed(v.answered_at)), t.success),
    };
    let name = contact_name_for(&app.contacts, v.number).or(v.remote_name);
    let seed = name.unwrap_or(v.number);
    let header = row![
        avatar(seed, call_avatar_label(&app.contacts, v.number), 72.0, v.glow),
        identity_block(name, v.number, status, status_color, t),
    ]
    .spacing(theme::space::LG)
    .align_y(Alignment::Center);

    let meters = column![
        meter_slider(t, "Mic", v.input_level, v.input_volume, Message::InputVolumeChanged),
        meter_slider(
            t,
            "Speaker",
            v.output_level,
            v.output_volume,
            Message::OutputVolumeChanged
        ),
    ]
    .spacing(theme::space::SM);

    let control = |glyph: Element<'a, Message>, label: &'static str, active: bool, msg: Option<Message>| {
        let mut b = button(
            column![glyph, text(label).size(theme::text_size::CAPTION)]
                .spacing(theme::space::XS)
                .align_x(Alignment::Center)
                .width(Length::Fill),
        )
        .style(theme::circle(active))
        .padding([theme::space::SM + 2.0, theme::space::SM])
        .width(Length::Fixed(76.0));
        if let Some(m) = msg {
            b = b.on_press(m);
        }
        b
    };
    let fg = |active: bool| if active { Color::WHITE } else { t.text };
    let mut controls = row![
        control(
            if v.muted {
                icons::mic_off(18.0, fg(true))
            } else {
                icons::mic(18.0, fg(false))
            },
            if v.muted { "Unmute" } else { "Mute" },
            v.muted,
            Some(Message::MuteToggled),
        ),
        control(
            if v.on_hold {
                icons::play(18.0, fg(true))
            } else {
                icons::pause(18.0, fg(false))
            },
            if v.on_hold { "Resume" } else { "Hold" },
            v.on_hold,
            Some(Message::HoldToggled),
        ),
        control(
            icons::phone_forwarded(18.0, fg(v.transfer_input.is_some())),
            "Transfer",
            v.transfer_input.is_some(),
            Some(Message::TransferPanelToggled),
        ),
        control(
            icons::layout_grid(18.0, fg(app.keypad_open)),
            "Keypad",
            app.keypad_open,
            Some(Message::KeypadToggled),
        ),
    ]
    .spacing(theme::space::SM);
    match join {
        JoinUi::Hidden => {}
        JoinUi::Available => {
            controls = controls.push(control(
                icons::merge(18.0, fg(false)),
                "Join",
                false,
                Some(Message::JoinCallsPressed),
            ))
        }
        JoinUi::Pending => {
            controls = controls.push(control(
                icons::merge(18.0, fg(true)),
                "Pick line…",
                true,
                Some(Message::JoinCallsPressed),
            ))
        }
        JoinUi::Joined { .. } => {
            controls = controls.push(control(
                icons::split(18.0, fg(true)),
                "Split",
                true,
                Some(Message::SplitCallPressed),
            ))
        }
    }

    let mut body = column![
        header,
        meters,
        container(controls).width(Length::Fill).align_x(Alignment::Center)
    ]
    .spacing(theme::space::LG);

    if let Some(target) = v.transfer_input {
        body = body.push(
            row![
                input("Transfer to number or SIP URI", target)
                    .on_input(Message::TransferTargetChanged)
                    .on_submit(Message::TransferConfirmed)
                    .width(Length::Fill),
                button(text("Transfer").size(theme::text_size::BODY))
                    .style(theme::button_primary)
                    .padding([theme::space::SM + 1.0, theme::space::LG])
                    .on_press(Message::TransferConfirmed),
            ]
            .spacing(theme::space::SM)
            .align_y(Alignment::Center),
        );
    }
    let digits: String = v.dtmf_feedback.iter().rev().take(12).rev().collect();
    if !digits.is_empty() {
        body = body.push(
            container(
                text(digits)
                    .size(theme::text_size::SUBHEAD)
                    .font(iced::Font::MONOSPACE)
                    .color(t.accent),
            )
            .width(Length::Fill)
            .align_x(Alignment::Center),
        );
    }
    if app.keypad_open {
        body = body.push(dialpad(app.dialpad_flash, t));
    }

    let hang_up = pill_call_button(
        "Hang up",
        theme::pill(Pill::Danger),
        icons::phone_off(16.0, Color::WHITE),
    )
    .on_press(Message::HangUpPressed);

    column![
        scrollable(body.padding(iced::Padding::ZERO.right(theme::space::SM)))
            .style(theme::scrollable_style)
            .height(Length::Fill),
        hang_up,
    ]
    .spacing(theme::space::LG)
    .height(Length::Fill)
    .into()
}

/// A live level meter fused with the gain slider that controls it.
fn meter_slider<'a>(
    t: &Tokens,
    label: &'a str,
    level: f32,
    value: f32,
    on_change: impl Fn(f32) -> Message + 'a,
) -> Element<'a, Message> {
    let height = 22.0;
    let fill = container(text(""))
        .width(Length::FillPortion((level.clamp(0.0, 1.0) * 1000.0) as u16 + 1))
        .height(Length::Fill)
        .style(theme::led(t.success));
    let empty = container(text(""))
        .width(Length::FillPortion(((1.0 - level.clamp(0.0, 1.0)) * 1000.0) as u16 + 1))
        .height(Length::Fill);
    let track = container(row![fill, empty])
        .width(Length::Fill)
        .height(Length::Fixed(height))
        .padding(3)
        .style(theme::raised);
    let handle = slider(0.0..=2.0, value, on_change)
        .step(0.05_f32)
        .width(Length::Fill)
        .style(theme::meter_slider);
    let combined = stack![
        track,
        container(handle)
            .height(Length::Fixed(height))
            .align_y(Alignment::Center)
    ];
    row![
        text(label)
            .size(theme::text_size::CAPTION)
            .width(Length::Fixed(56.0))
            .color(muted(t)),
        combined,
        text(format!("{:.0}%", value * 100.0))
            .size(theme::text_size::CAPTION)
            .font(iced::Font::MONOSPACE)
            .width(Length::Fixed(40.0))
            .align_x(Alignment::End),
    ]
    .spacing(theme::space::SM)
    .align_y(Alignment::Center)
    .into()
}
