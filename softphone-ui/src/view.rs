use crate::app::{
    AccountSession, App, AudioSettingsForm, CallUiState, ContactForm, ContactSort, Message, RegistrationStatus,
    Screen, SettingsForm, SipSettingsForm,
};
use crate::contacts::Contact;
use crate::history::{self, CallDirection, CallOutcome, HistoryEntry};
use crate::theme::{self, Pill};
use iced::widget::{
    button, column, container, pick_list, row, rule, scrollable, slider, stack, text, text_input, toggler,
    tooltip,
};
use iced::{Alignment, Color, Element, Length};
use softphone_core::config::{SipTransport, SIP_TRANSPORTS};
use std::time::Instant;

const DANGER_TEXT: Color = Color {
    r: 1.0,
    g: 0.4,
    b: 0.4,
    a: 1.0,
};

/// This is a phone-shaped dialer UI, not a fluid full-width app — letting it
/// stretch edge-to-edge on a maximized/ultrawide window just spreads the
/// same content across a lot of empty-feeling space and looks stretched
/// rather than "bigger." Capping the content at a comfortable reading width
/// and centering it (see `main_view`) keeps it looking deliberate at any
/// window size instead.
const MAX_CONTENT_WIDTH: f32 = 640.0;

pub fn main_view(app: &App) -> Element<'_, Message> {
    let scale = app.ui_scale();

    let Some(acc) = app.selected() else {
        return no_account_view();
    };

    if app.compact_mode
        && let CallUiState::Active {
            number,
            media,
            muted,
            on_hold,
            ..
        } = acc.selected_call()
    {
        return compact_call_view(&app.contacts, number, media.is_some(), *muted, *on_hold);
    }

    let body = match app.screen {
        Screen::Dialer => row![
            line_sidebar(acc, scale),
            rule::vertical(1),
            dialer_tab(app, acc, scale)
        ]
        .spacing(scaled(10.0, scale))
        .into(),
        Screen::Contacts => contacts_tab(app, scale),
        Screen::History => history_tab(app, scale),
    };

    let panel = container(body)
        .height(Length::Fill)
        .width(Length::Fill)
        .padding(scaled(14.0, scale))
        .style(theme::card);

    let mut content = column![tab_bar(app.screen, scale), panel, footer(acc),]
        .spacing(scaled(14.0, scale))
        .padding(scaled(16.0, scale))
        .height(Length::Fill);

    if let Some(error) = &app.error {
        content = content.push(text(error).size(13).color(DANGER_TEXT));
    }

    container(content)
        .width(Length::Fill)
        .height(Length::Fill)
        .max_width(MAX_CONTENT_WIDTH)
        .align_x(Alignment::Center)
        .into()
}

/// Shown instead of the normal dialer when there are no SIP accounts
/// configured yet — `App::boot` already opens the SIP settings window
/// automatically in this case, so this is just a calm placeholder behind
/// it rather than an empty/broken-looking main window.
fn no_account_view<'a>() -> Element<'a, Message> {
    column![
        text("OxideSip").size(24),
        text("No SIP account configured yet").size(14).color(muted_text()),
        button(text("Open SIP Settings").size(14))
            .style(theme::pill(Pill::Primary))
            .padding(12)
            .on_press(Message::OpenSipSettings),
    ]
    .spacing(14)
    .width(Length::Fill)
    .height(Length::Fill)
    .align_x(Alignment::Center)
    .into()
}

/// Small dropdown letting the user pick which registered account's lines
/// the Dialer tab shows/controls — sits in front of the SIP settings button
/// (see `idle_view`'s toolbar). Hidden entirely when there's only one
/// account (nothing to switch between).
#[derive(Debug, Clone, PartialEq)]
struct AccountOption {
    index: usize,
    name: String,
}

impl std::fmt::Display for AccountOption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name)
    }
}

fn account_switcher(app: &App, scale: f32) -> Option<Element<'_, Message>> {
    if app.accounts.len() < 2 {
        return None;
    }
    let options: Vec<AccountOption> = app
        .accounts
        .iter()
        .enumerate()
        .map(|(index, acc)| AccountOption {
            index,
            name: if acc.config.name.is_empty() {
                format!("Account {}", index + 1)
            } else {
                acc.config.name.clone()
            },
        })
        .collect();
    let selected = options.get(app.selected_account).cloned();
    Some(
        pick_list(options, selected, |opt: AccountOption| Message::AccountSwitched(opt.index))
            .text_size(scaled(11.0, scale))
            .padding(scaled(7.0, scale))
            .into(),
    )
}

/// The Line 1-5 column to the left of the dialpad — each button switches
/// which call slot the Dialer tab shows/controls (see `App::select_line`).
/// Every button carries a small corner LED with three levels: **off**
/// (this line is idle and not the one you've explicitly opened), **on** —
/// amber — (idle-but-opened/armed, ringing, dialing out, or an
/// occupied-but-held line), and **live** — red — (audio actually flowing:
/// the selected unheld leg, or either half of a joined pair). Tapping an
/// idle line opens it (LED on, dial tone, ready to dial); tapping the line
/// you're already on while it's idle toggles it back off — see
/// `App::select_line`/`App.line_open`.
fn line_sidebar(acc: &AccountSession, scale: f32) -> Element<'_, Message> {
    let width = scaled(46.0, scale);
    let mut col = column![].spacing(scaled(6.0, scale));
    for line in 1..=5u8 {
        let idx = (line - 1) as usize;
        let call = &acc.lines[idx];
        let selected = line == acc.selected_line;
        let joined = acc.joined[idx].is_some();
        let live = matches!(
            call,
            CallUiState::Active { on_hold, .. } if joined || (selected && !on_hold)
        );
        let armed_idle = selected && acc.line_open && matches!(call, CallUiState::Idle);
        let (fill, sub_label) = match call {
            CallUiState::Idle => (None, String::new()),
            CallUiState::Incoming { caller, .. } => (Some(theme::oxide_palette().warning), short_label(caller)),
            CallUiState::Outgoing { number, .. } => (Some(theme::oxide_palette().primary), short_label(number)),
            CallUiState::Active { number, .. } => {
                let label = if joined {
                    format!("{} J", short_label(number))
                } else {
                    short_label(number)
                };
                (
                    Some(if live {
                        theme::oxide_palette().danger
                    } else {
                        theme::oxide_palette().primary
                    }),
                    label,
                )
            }
        };
        // The LED lives *inside* the button's own content (a row above the
        // label), not stacked as a separate layer on top of it — an earlier
        // version used `stack!` to overlay a corner dot, but a transparent
        // container spanning the whole button's bounds sat in front of the
        // button for hit-testing purposes and silently ate most clicks, which
        // is what made the line buttons feel unresponsive ("takes a minute
        // to turn on"). Keeping the LED as regular button content avoids
        // that entirely — there's only ever one interactive widget here.
        let led_size = scaled(7.0, scale);
        let led_color = if live {
            theme::oxide_palette().danger
        } else if fill.is_some() || armed_idle {
            theme::oxide_palette().warning
        } else {
            Color {
                a: 0.25,
                ..Color::WHITE
            }
        };
        let led = container(text(""))
            .width(Length::Fixed(led_size))
            .height(Length::Fixed(led_size))
            .style(theme::led(led_color));
        let content = column![
            container(led).width(Length::Fill).align_x(Alignment::End),
            text(format!("L{line}")).size(scaled(11.0, scale)),
            text(sub_label).size(scaled(7.0, scale)),
        ]
        .spacing(1)
        .align_x(Alignment::Center);
        let style = theme::circle_state(fill, selected);
        let btn = button(
            container(content)
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(Alignment::Center)
                .align_y(Alignment::Center)
                .padding([2, 4]),
        )
        .style(style)
        .width(Length::Fixed(width))
        .height(Length::Fixed(width))
        .padding(0)
        .on_press(Message::LineSelected(line));
        col = col.push(with_tooltip(btn, line_tooltip(line, call, acc.joined[idx], armed_idle)));
    }
    col.into()
}

fn short_label(s: &str) -> String {
    s.chars().take(6).collect()
}

fn line_tooltip(line: u8, call: &CallUiState, joined_with: Option<u8>, armed_idle: bool) -> String {
    match call {
        CallUiState::Idle if armed_idle => format!("Line {line} — open, ready to dial"),
        CallUiState::Idle => format!("Line {line} — idle"),
        CallUiState::Incoming { caller, .. } => format!("Line {line} — ringing: {caller}"),
        CallUiState::Outgoing { number, .. } => format!("Line {line} — calling {number}"),
        CallUiState::Active { number, on_hold, .. } => {
            if let Some(partner) = joined_with {
                format!("Line {line} — {number} (joined with Line {partner})")
            } else if *on_hold {
                format!("Line {line} — {number} (on hold)")
            } else {
                format!("Line {line} — {number}")
            }
        }
    }
}

/// Scales a base pixel value by the window-size-derived UI scale factor —
/// the shared building block for making font sizes/padding/control
/// dimensions grow or shrink with the window instead of just leaving (or
/// running out of) empty margin.
fn scaled(base: f32, scale: f32) -> f32 {
    base * scale
}

/// A minimal floating call-control strip for compact mode: one row, no tab
/// bar, no footer — meant to fit the small `COMPACT_WINDOW_SIZE` window
/// exactly, filling it completely so there's never a mismatch between the
/// window's actual size and what's rendered inside it.
fn compact_call_view<'a>(
    contacts: &[Contact],
    number: &'a str,
    media_ready: bool,
    muted: bool,
    on_hold: bool,
) -> Element<'a, Message> {
    let status = if !media_ready {
        "Connecting…"
    } else if on_hold {
        "On hold"
    } else {
        "Connected"
    };
    let status_color = if on_hold {
        theme::oxide_palette().warning
    } else {
        muted_text()
    };

    let avatar = container(text(call_avatar_label(contacts, number)).size(17))
        .width(Length::Fixed(46.0))
        .height(Length::Fixed(46.0))
        .align_x(Alignment::Center)
        .align_y(Alignment::Center)
        .style(theme::avatar(theme::avatar_color(number)));

    let header = row![
        avatar,
        column![
            text(number).size(15),
            text(status).size(11).color(status_color),
        ]
        .spacing(2)
        .width(Length::Fill),
        with_tooltip(
            circle_button(text("[ ]").size(10), 28.0, theme::circle(false))
                .on_press(Message::CompactToggled),
            "Expand to full window",
        ),
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    let controls = row![
        with_tooltip(
            circle_button(
                text(if muted { "MUTED" } else { "MIC" }).size(9),
                44.0,
                theme::circle(muted),
            )
            .on_press(Message::MuteToggled),
            if muted { "Unmute" } else { "Mute" },
        ),
        with_tooltip(
            circle_button(
                text(if on_hold { "RESUME" } else { "HOLD" }).size(8),
                44.0,
                theme::circle(on_hold),
            )
            .on_press(Message::HoldToggled),
            if on_hold { "Resume" } else { "Hold" },
        ),
        pill_action_button("Hang Up", theme::pill(Pill::Danger), 1.0).on_press(Message::HangUpPressed),
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    container(column![header, controls].spacing(16))
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(16)
        .align_y(Alignment::Center)
        .style(|_theme| container::Style {
            background: Some(iced::Background::Color(theme::oxide_palette().background)),
            ..container::Style::default()
        })
        .into()
}

pub fn sip_settings_window_view(app: &App) -> Element<'_, Message> {
    sip_settings_view(app, &app.sip_settings_form, app.error.as_deref())
}

pub fn audio_settings_window_view(app: &App) -> Element<'_, Message> {
    audio_settings_view(
        &app.audio_settings_form,
        &app.input_devices,
        &app.output_devices,
        app.error.as_deref(),
    )
}

pub fn settings_window_view(app: &App) -> Element<'_, Message> {
    settings_view(
        &app.settings_form,
        &app.output_devices,
        &app.app_capture_streams,
        app.error.as_deref(),
    )
}

/// Wraps `content` with a small hover tooltip — used throughout for the
/// icon-only circular buttons, whose meaning isn't otherwise labeled.
fn with_tooltip<'a>(
    content: impl Into<Element<'a, Message>>,
    label: impl Into<String>,
) -> Element<'a, Message> {
    tooltip(content, text(label.into()).size(12), tooltip::Position::Top)
        .style(theme::card)
        .into()
}

fn tab_bar(current: Screen, scale: f32) -> Element<'static, Message> {
    let tab = |label: &'static str, screen: Screen| {
        // The label needs its own `width(Fill) + align_x(Center)` — a bare
        // `text(label)` shrinks to its natural size and sits at the *left*
        // edge of the button's padded area, so on a wide `width(Fill)` tab
        // button the label read as left-stuck rather than centered. Same
        // fix already used by `pill_action_button`.
        button(
            text(label)
                .size(scaled(12.0, scale))
                .align_x(Alignment::Center)
                .width(Length::Fill),
        )
        .style(theme::pill(Pill::Tab(current == screen)))
        .padding(scaled(9.0, scale))
        .width(Length::Fill)
        .on_press(Message::TabSelected(screen))
    };
    container(
        row![
            tab("Dialer", Screen::Dialer),
            tab("Contacts", Screen::Contacts),
            tab("History", Screen::History),
        ]
        .spacing(4),
    )
    .padding(4)
    .style(theme::tab_track)
    .into()
}

fn footer(acc: &AccountSession) -> Element<'_, Message> {
    let mut bar = row![].align_y(Alignment::Center);
    if let Some(status) = line_status_label(acc) {
        bar = bar.push(text(status).size(11).color(muted_text()));
    }
    bar = bar.push(iced::widget::space::horizontal());
    bar = bar.push(status_led(&acc.registration));

    column![rule::horizontal(1), bar].spacing(8).into()
}

/// The footer's bottom-left readout — a live "(L1 0:25)" elapsed-time timer
/// for whatever the selected line is currently doing (open/armed, ringing,
/// dialing out, or an active call), so you can see how long something's
/// been going on without needing to be on the Dialer tab (this is visible
/// on Contacts/History too, unlike the in-call header's own timer). Falls
/// back to the last call's outcome (e.g. "(hung up)") once the line's back
/// to fully idle. Ticks live off `Instant::elapsed()` computed fresh on
/// every render — no separate per-second `Message` needed, since the
/// existing `Tick` subscription already redraws often enough to read as
/// live.
fn line_status_label(acc: &AccountSession) -> Option<String> {
    let idx = acc.selected_idx();
    let line = acc.selected_line;
    match &acc.lines[idx] {
        CallUiState::Idle => {
            if acc.line_open {
                let since = acc.line_open_at?;
                Some(format!("(L{line} {})", format_elapsed(since.elapsed())))
            } else {
                acc.last_call_status[idx].as_deref().map(|label| format!("({label})"))
            }
        }
        CallUiState::Incoming { ringing_since, .. } => {
            Some(format!("(L{line} {})", format_elapsed(ringing_since.elapsed())))
        }
        CallUiState::Outgoing { started_at, .. } => {
            Some(format!("(L{line} {})", format_elapsed(started_at.elapsed())))
        }
        CallUiState::Active { answered_at, .. } => {
            Some(format!("(L{line} {})", format_elapsed(answered_at.elapsed())))
        }
    }
}

fn format_elapsed(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// Shows both a color-coded dot *and* a short text label (not just a
/// hover-only tooltip) — the connection state should be readable at a
/// glance. Deliberately small and set off from the main content by the
/// divider rule above, so it reads as a status strip rather than competing
/// with the actual screen content for attention.
fn status_led(registration: &RegistrationStatus) -> Element<'_, Message> {
    let palette = theme::oxide_palette();
    let (fill, label, detail) = match registration {
        RegistrationStatus::Connecting => (palette.warning, "Connecting", "Connecting to SIP server…".to_string()),
        RegistrationStatus::Registered {
            expires,
            registered_at,
            rtt_ms,
        } => {
            let remaining = expires.saturating_sub(registered_at.elapsed().as_secs() as u32);
            (
                palette.success,
                "Connected",
                format!(
                    "Registered with SIP server — renews in {}:{:02} — last REGISTER took {}ms",
                    remaining / 60,
                    remaining % 60,
                    rtt_ms,
                ),
            )
        }
        RegistrationStatus::Failed { reason } => {
            (palette.danger, "Offline", format!("Registration failed: {reason}"))
        }
    };
    let dot = container(text(""))
        .width(Length::Fixed(7.0))
        .height(Length::Fixed(7.0))
        .style(theme::led(fill));
    let content = row![dot, text(label).size(10).color(muted_text())]
        .spacing(6)
        .align_y(Alignment::Center);
    with_tooltip(content, detail)
}

fn centered_text(label: &str) -> Element<'_, Message> {
    column![text(label).size(18)]
        .width(Length::Fill)
        .align_x(Alignment::Center)
        .into()
}

/// What the JOIN/SPLIT control on the active-call view should show for
/// whichever line is currently selected — computed from `App.joined`/
/// `App.pending_join` rather than carried on `CallUiState` itself, since it
/// depends on *other* lines' state too (is there another active call to
/// join at all?).
enum JoinUi {
    /// Fewer than 2 active lines exist — nothing to join, hide the control.
    Hidden,
    Available,
    /// Waiting for the user to tap another active line's sidebar button.
    Pending,
    Joined { partner_number: String },
}

fn selected_join_ui(acc: &AccountSession) -> JoinUi {
    let idx = acc.selected_idx();
    if acc.pending_join == Some(acc.selected_line) {
        return JoinUi::Pending;
    }
    if let Some(partner_line) = acc.joined[idx] {
        let partner_idx = (partner_line.saturating_sub(1)) as usize;
        let partner_number = match acc.lines.get(partner_idx) {
            Some(CallUiState::Active { number, .. }) => number.clone(),
            _ => String::new(),
        };
        return JoinUi::Joined { partner_number };
    }
    let has_other_active = (1..=5u8).any(|l| {
        l != acc.selected_line
            && matches!(acc.lines.get((l - 1) as usize), Some(CallUiState::Active { .. }))
    });
    if has_other_active {
        JoinUi::Available
    } else {
        JoinUi::Hidden
    }
}

fn dialer_tab<'a>(app: &'a App, acc: &'a AccountSession, scale: f32) -> Element<'a, Message> {
    let join_ui = selected_join_ui(acc);
    match acc.selected_call() {
        CallUiState::Idle => {
            let has_last_outgoing = app
                .call_history
                .iter()
                .any(|e| e.direction == CallDirection::Outgoing);
            idle_view(app, &app.dial_input, has_last_outgoing, scale)
        }
        CallUiState::Incoming { caller, .. } => incoming_view(&app.contacts, caller, scale),
        CallUiState::Outgoing { number, .. } => outgoing_view(&app.contacts, number, scale),
        CallUiState::Active {
            number,
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
            ..
        } => active_view(
            &app.contacts,
            number,
            media.is_some(),
            *answered_at,
            *input_level,
            *output_level,
            *muted,
            *output_volume,
            *input_volume,
            *on_hold,
            transfer_input.as_deref(),
            dtmf_feedback,
            join_ui,
            scale,
        ),
    }
}

/// Centers `content` inside a fixed-size circle — the shared shape behind
/// dialpad keys and the mute/hold/transfer toggles.
fn circle_button<'a>(
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

/// A wide, rounded-rectangle "pill" button carrying a text label — used for
/// the primary call actions (Call/Answer/Decline/Hang Up), which need to
/// read clearly rather than be squeezed into a small circle.
fn pill_action_button<'a>(
    label: &'a str,
    style: impl Fn(&iced::Theme, button::Status) -> button::Style + 'a,
    scale: f32,
) -> button::Button<'a, Message> {
    button(
        text(label)
            .size(scaled(15.0, scale))
            .align_x(Alignment::Center)
            .width(Length::Fill),
    )
    .style(style)
    .padding([scaled(12.0, scale), scaled(20.0, scale)])
    .width(Length::Fill)
}

fn idle_view<'a>(app: &'a App, dial_input: &'a str, has_last_outgoing: bool, scale: f32) -> Element<'a, Message> {
    let gear_size = scaled(36.0, scale);
    let mut toolbar = row![iced::widget::space::horizontal()].spacing(8);
    if let Some(switcher) = account_switcher(app, scale) {
        toolbar = toolbar.push(switcher);
    }
    toolbar = toolbar.push(with_tooltip(
        circle_button(text("SIP").size(scaled(10.0, scale)), gear_size, theme::circle(false))
            .on_press(Message::OpenSipSettings),
        "SIP account setup",
    ));
    toolbar = toolbar.push(with_tooltip(
        circle_button(text("AUDIO").size(scaled(8.0, scale)), gear_size, theme::circle(false))
            .on_press(Message::OpenAudioSettings),
        "Audio & codec settings",
    ));
    toolbar = toolbar.push(with_tooltip(
        circle_button(text("SETTINGS").size(scaled(6.0, scale)), gear_size, theme::circle(false))
            .on_press(Message::OpenSettings),
        "Call handling: DND, auto-answer, forwarding, recording",
    ));

    // Slimmer than the original 13px padding / 22px text — a tall, huge
    // dial input read as out of step with the rest of the app's tightened,
    // carded visual language.
    let input = text_input("Enter number", dial_input)
        .on_input(Message::DialInputChanged)
        .on_submit(Message::CallPressed)
        .padding([scaled(9.0, scale), scaled(13.0, scale)])
        .size(scaled(18.0, scale))
        .align_x(Alignment::Center);

    let history_toggle = with_tooltip(
        circle_button(
            text(if app.dial_history_open { "^" } else { "v" }).size(scaled(11.0, scale)),
            scaled(38.0, scale),
            theme::circle(app.dial_history_open),
        )
        .on_press(Message::DialHistoryToggled),
        "Recent numbers",
    );
    let input_row = row![input, history_toggle].spacing(8).align_y(Alignment::Center);

    let call_button = {
        let b = pill_action_button("Call", theme::pill(Pill::Success), scale);
        if dial_input.is_empty() {
            b
        } else {
            b.on_press(Message::CallPressed)
        }
    };
    let redial_button = {
        let b = circle_button(text("REDIAL").size(scaled(8.0, scale)), scaled(48.0, scale), theme::circle(false));
        if has_last_outgoing {
            b.on_press(Message::RedialPressed)
        } else {
            b
        }
    };

    let actions = row![with_tooltip(redial_button, "Redial last number"), call_button]
        .spacing(18)
        .align_y(Alignment::Center);

    let mut content = column![toolbar, input_row];
    if app.dial_history_open {
        content = content.push(dial_history_panel(app));
    }
    content = content.push(dialpad(scale)).push(actions);

    content.spacing(scaled(16.0, scale)).into()
}

/// Recent *outgoing* numbers only (matching the plan's "fills the box, does
/// not dial" behavior) — most-recent-first, deduplicated, capped at 12 so
/// the panel never dwarfs the dialpad beneath it.
fn dial_history_panel(app: &App) -> Element<'_, Message> {
    let mut seen = std::collections::HashSet::new();
    let mut numbers = Vec::new();
    for entry in app.call_history.iter().rev() {
        if entry.direction != CallDirection::Outgoing {
            continue;
        }
        if seen.insert(entry.number.clone()) {
            numbers.push(entry.number.clone());
        }
        if numbers.len() >= 12 {
            break;
        }
    }

    if numbers.is_empty() {
        return container(text("No recent numbers").size(12).color(muted_text()))
            .padding(10)
            .into();
    }

    let mut list = column![].spacing(4);
    for number in numbers {
        list = list.push(
            button(text(number.clone()).size(13))
                .style(theme::list_row)
                .padding(8)
                .width(Length::Fill)
                .on_press(Message::DialHistorySelected(number)),
        );
    }
    container(scrollable(list).height(Length::Fixed(160.0)))
        .style(theme::card)
        .padding(6)
        .width(Length::Fill)
        .into()
}

fn incoming_view<'a>(contacts: &[Contact], caller: &'a str, scale: f32) -> Element<'a, Message> {
    let avatar = container(text(call_avatar_label(contacts, caller)).size(scaled(20.0, scale)))
        .width(Length::Fixed(scaled(78.0, scale)))
        .height(Length::Fixed(scaled(78.0, scale)))
        .align_x(Alignment::Center)
        .align_y(Alignment::Center)
        .style(theme::avatar_state(theme::avatar_color(caller), true));

    column![
        column![avatar].width(Length::Fill).align_x(Alignment::Center),
        column![
            text("Incoming call").size(13).color(muted_text()),
            text(caller).size(20),
        ]
        .spacing(4)
        .width(Length::Fill)
        .align_x(Alignment::Center),
        row![
            pill_action_button("Decline", theme::pill(Pill::Danger), scale).on_press(Message::RejectPressed),
            pill_action_button("Answer", theme::pill(Pill::Success), scale).on_press(Message::AnswerPressed),
        ]
        .spacing(16),
    ]
    .spacing(22)
    .width(Length::Fill)
    .align_x(Alignment::Center)
    .into()
}

/// The dialing-out screen — between pressing Call and the far end actually
/// answering. Previously just static "Calling..." text with no way to back
/// out of it, so canceling a call before it was answered meant waiting for
/// it to either connect or time out on its own; this gives it the same
/// Hang Up affordance the in-call screen has (`Message::HangUpPressed`
/// already handles `Outgoing` the same as `Active` — see `app.rs`).
fn outgoing_view<'a>(contacts: &[Contact], number: &'a str, scale: f32) -> Element<'a, Message> {
    let avatar = container(text(call_avatar_label(contacts, number)).size(scaled(20.0, scale)))
        .width(Length::Fixed(scaled(78.0, scale)))
        .height(Length::Fixed(scaled(78.0, scale)))
        .align_x(Alignment::Center)
        .align_y(Alignment::Center)
        .style(theme::avatar_state(theme::avatar_color(number), true));

    column![
        column![avatar].width(Length::Fill).align_x(Alignment::Center),
        column![
            text("Calling…").size(13).color(muted_text()),
            text(number).size(20),
        ]
        .spacing(4)
        .width(Length::Fill)
        .align_x(Alignment::Center),
        pill_action_button("Hang Up", theme::pill(Pill::Danger), scale).on_press(Message::HangUpPressed),
    ]
    .spacing(22)
    .width(Length::Fill)
    .align_x(Alignment::Center)
    .into()
}

/// First letter of a name/number, uppercased, for an avatar circle. Falls
/// back to a plain `#` (never a symbol glyph — see `circle_button`'s
/// call-site comments on why this app sticks to ASCII for anything that
/// needs to reliably render) when there's nothing alphanumeric to show.
fn initial(label: &str) -> String {
    label
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "#".to_string())
}

/// A more meaningful avatar label for the big in-call/ringing/dialing
/// circle than `initial`'s single leading character — for a raw dialed
/// number that leading character is very often just a stray `*`/`+`/`1`
/// (a feature code prefix or country code), which read as an arbitrary,
/// meaningless digit in an otherwise-plain circle. This shows the matching
/// saved contact's initials when we recognize the number, or otherwise the
/// *last* two digits — more identifying in practice (e.g. a PBX
/// extension's own number) than an arbitrary leading one.
fn call_avatar_label(contacts: &[Contact], raw: &str) -> String {
    if let Some(contact) = contacts.iter().find(|c| c.number == raw) {
        return contact_initials(&contact.name);
    }
    let digits: String = raw.chars().rev().filter(|c| c.is_ascii_digit()).take(2).collect();
    if digits.is_empty() {
        "#".to_string()
    } else {
        digits.chars().rev().collect()
    }
}

fn contact_initials(name: &str) -> String {
    let mut letters = name.split_whitespace().filter_map(|w| w.chars().next());
    match (letters.next(), letters.next()) {
        (Some(a), Some(b)) => format!("{}{}", a.to_ascii_uppercase(), b.to_ascii_uppercase()),
        (Some(a), None) => a.to_ascii_uppercase().to_string(),
        _ => "#".to_string(),
    }
}

#[allow(clippy::too_many_arguments)]
fn active_view<'a>(
    contacts: &[Contact],
    number: &'a str,
    media_ready: bool,
    answered_at: Instant,
    input_level: f32,
    output_level: f32,
    muted: bool,
    output_volume: f32,
    input_volume: f32,
    on_hold: bool,
    transfer_input: Option<&'a str>,
    dtmf_feedback: &'a [char],
    join_ui: JoinUi,
    scale: f32,
) -> Element<'a, Message> {
    let status = if let JoinUi::Joined { partner_number } = &join_ui {
        format!("Joined with {partner_number}")
    } else if !media_ready {
        "Connecting audio…".to_string()
    } else if on_hold {
        "On hold".to_string()
    } else {
        let elapsed = answered_at.elapsed().as_secs();
        format!("{:02}:{:02}", elapsed / 60, elapsed % 60)
    };
    let status_color = if matches!(join_ui, JoinUi::Joined { .. }) {
        theme::oxide_palette().success
    } else if on_hold {
        theme::oxide_palette().warning
    } else {
        muted_text()
    };

    let digits: String = dtmf_feedback.iter().rev().take(8).rev().collect();

    let avatar_style = theme::avatar_state(theme::avatar_color(number), media_ready && !on_hold);
    let avatar = container(text(call_avatar_label(contacts, number)).size(scaled(19.0, scale)))
        .width(Length::Fixed(scaled(60.0, scale)))
        .height(Length::Fixed(scaled(60.0, scale)))
        .align_x(Alignment::Center)
        .align_y(Alignment::Center)
        .style(avatar_style);

    let compact_toggle = with_tooltip(
        circle_button(text("-").size(scaled(16.0, scale)), scaled(28.0, scale), theme::circle(false))
            .on_press(Message::CompactToggled),
        "Shrink to compact call bar",
    );

    let header = row![
        row![
            avatar,
            column![
                text(number).size(scaled(16.0, scale)),
                text(status).size(scaled(12.0, scale)).color(status_color),
            ]
            .spacing(2),
        ]
        .spacing(10)
        .align_y(Alignment::Center)
        .width(Length::Fill),
        compact_toggle,
    ]
    .align_y(Alignment::Center);

    // A touch smaller than the other circle buttons in the app (was 46) —
    // with the join/split button, this row can hold five of these at once,
    // and five at 46px plus 16px spacing (294px) didn't fit the default
    // window's available content width (~260px after the sidebar and
    // panel/window padding), overflowing and looking oversized/broken —
    // exactly what showed up once a second line made the join button
    // appear. 40px + tighter spacing keeps all five comfortably inside.
    let toggle_size = scaled(40.0, scale);
    let mute_label = if muted { "MUTED" } else { "MIC" };
    let hold_label = if on_hold { "RESUME" } else { "HOLD" };

    let join_button: Option<Element<'_, Message>> = match &join_ui {
        JoinUi::Hidden => None,
        JoinUi::Available => Some(
            with_tooltip(
                circle_button(text("JOIN").size(scaled(9.0, scale)), toggle_size, theme::circle(false))
                    .on_press(Message::JoinCallsPressed),
                "Bridge this call with another active line",
            ),
        ),
        JoinUi::Pending => Some(
            with_tooltip(
                circle_button(text("TAP LINE").size(scaled(7.0, scale)), toggle_size, theme::circle(true))
                    .on_press(Message::JoinCallsPressed),
                "Tap another active line in the sidebar to join it — tap again to cancel",
            ),
        ),
        JoinUi::Joined { .. } => Some(
            with_tooltip(
                circle_button(text("SPLIT").size(scaled(9.0, scale)), toggle_size, theme::circle(true))
                    .on_press(Message::SplitCallPressed),
                "Split the joined call back into separate lines",
            ),
        ),
    };

    let mut controls = row![
        with_tooltip(
            circle_button(text(mute_label).size(scaled(10.0, scale)), toggle_size, theme::circle(muted))
                .on_press(Message::MuteToggled),
            if muted { "Unmute microphone" } else { "Mute microphone" },
        ),
        with_tooltip(
            circle_button(text(hold_label).size(scaled(9.0, scale)), toggle_size, theme::circle(on_hold))
                .on_press(Message::HoldToggled),
            if on_hold { "Resume call" } else { "Hold call" },
        ),
        with_tooltip(
            circle_button(
                text("XFER").size(scaled(10.0, scale)),
                toggle_size,
                theme::circle(transfer_input.is_some()),
            )
            .on_press(Message::TransferPanelToggled),
            "Transfer call",
        ),
        with_tooltip(
            circle_button(text("+ CALL").size(scaled(8.0, scale)), toggle_size, theme::circle(false))
                .on_press(Message::AddCallPressed),
            "Start a new call on the next free line",
        ),
    ]
    .spacing(8);
    if let Some(join_button) = join_button {
        controls = controls.push(join_button);
    }

    let transfer_section: Option<Element<'_, Message>> = transfer_input.map(|target| {
        row![
            text_input("Transfer to...", target)
                .on_input(Message::TransferTargetChanged)
                .on_submit(Message::TransferConfirmed)
                .padding(8)
                .width(Length::Fill),
            with_tooltip(
                circle_button(text("SEND").size(9), 38.0, theme::circle(true))
                    .on_press(Message::TransferConfirmed),
                "Send transfer",
            ),
        ]
        .spacing(6)
        .align_y(Alignment::Center)
        .into()
    });

    let mut content = column![
        header,
        meter_slider("Mic", input_level, input_volume, Message::InputVolumeChanged, scale),
        meter_slider("Out", output_level, output_volume, Message::OutputVolumeChanged, scale),
        column![controls].width(Length::Fill).align_x(Alignment::Center),
    ]
    .spacing(scaled(14.0, scale));

    if let Some(transfer_section) = transfer_section {
        content = content.push(transfer_section);
    }
    if !digits.is_empty() {
        content = content.push(
            text(digits)
                .size(16)
                .color(theme::oxide_palette().primary),
        );
    }

    content = content.push(dialpad(scale));
    content = content.push(pill_action_button("Hang Up", theme::pill(Pill::Danger), scale).on_press(Message::HangUpPressed));

    content.into()
}

const METER_SLIDER_HEIGHT: f32 = 24.0;

/// A live level meter *fused* with the gain slider that controls it: the
/// fill shows the real-time signal level (from `MediaSession::input_level`/
/// `output_level`, sampled on every `Message::Tick`), and a thin draggable
/// handle bar sits on top of it to adjust gain — one compact control
/// instead of a separate meter row plus a separate slider row.
fn meter_slider<'a>(
    label: &'a str,
    level: f32,
    value: f32,
    on_change: impl Fn(f32) -> Message + 'a,
    scale: f32,
) -> Element<'a, Message> {
    let palette = theme::oxide_palette();
    let height = scaled(METER_SLIDER_HEIGHT, scale);

    let fill = container(text(""))
        .width(Length::FillPortion((level.clamp(0.0, 1.0) * 1000.0) as u16 + 1))
        .height(Length::Fill)
        .style(theme::led(palette.success));
    let empty = container(text(""))
        .width(Length::FillPortion(((1.0 - level.clamp(0.0, 1.0)) * 1000.0) as u16 + 1))
        .height(Length::Fill);
    let meter_track = container(row![fill, empty])
        .width(Length::Fill)
        .height(Length::Fixed(height))
        .style(theme::card);

    let handle = slider(0.0..=2.0, value, on_change)
        .step(0.05_f32)
        .width(Length::Fill)
        .style(theme::meter_slider);

    let combined = stack![
        meter_track,
        container(handle).height(Length::Fixed(height)).align_y(Alignment::Center),
    ];

    row![
        text(label).size(scaled(12.0, scale)).width(scaled(34.0, scale)).color(muted_text()),
        combined,
        text(format!("{:.0}%", value * 100.0))
            .size(scaled(12.0, scale))
            .width(scaled(34.0, scale)),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into()
}

const DIALPAD_KEYS: [[char; 3]; 4] = [
    ['1', '2', '3'],
    ['4', '5', '6'],
    ['7', '8', '9'],
    ['*', '0', '#'],
];

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

fn dialpad<'a>(scale: f32) -> Element<'a, Message> {
    let key_size = scaled(42.0, scale);
    let mut rows = column![].spacing(scaled(6.0, scale));
    for keys in DIALPAD_KEYS {
        let mut r = row![].spacing(scaled(6.0, scale));
        for key in keys {
            let letters = dialpad_letters(key);
            let label: Element<'_, Message> = if letters.is_empty() {
                text(key.to_string()).size(scaled(15.0, scale)).into()
            } else {
                column![
                    text(key.to_string()).size(scaled(14.0, scale)),
                    text(letters).size(scaled(6.0, scale)).color(muted_text()),
                ]
                .spacing(0)
                .align_x(Alignment::Center)
                .into()
            };
            r = r.push(
                circle_button(label, key_size, theme::circle(false))
                    .on_press(Message::DialpadPressed(key)),
            );
        }
        rows = rows.push(r);
    }
    column![rows].width(Length::Fill).align_x(Alignment::Center).into()
}

fn contacts_tab(app: &App, scale: f32) -> Element<'_, Message> {
    if let Some(form) = &app.contact_form {
        return contact_form_view(form);
    }

    let filter = app.contact_filter.to_lowercase();
    let search = text_input("Search contacts", &app.contact_filter)
        .on_input(Message::ContactFilterChanged)
        .padding(9)
        .width(Length::Fill);
    let add_button = with_tooltip(
        circle_button(text("+").size(scaled(18.0, scale)), scaled(38.0, scale), theme::circle(true))
            .on_press(Message::AddContactPressed),
        "Add contact",
    );
    let sort_button = with_tooltip(
        circle_button(
            text(app.contact_sort.label()).size(scaled(10.0, scale)),
            scaled(38.0, scale),
            theme::circle(false),
        )
        .on_press(Message::ContactSortToggled),
        "Sort contacts",
    );

    let mut matches: Vec<_> = app
        .contacts
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            filter.is_empty()
                || c.name.to_lowercase().contains(&filter)
                || c.number.contains(&filter)
        })
        .collect();
    match app.contact_sort {
        ContactSort::NameAsc => matches.sort_by_key(|(_, c)| c.name.to_lowercase()),
        ContactSort::NameDesc => {
            matches.sort_by_key(|(_, c)| c.name.to_lowercase());
            matches.reverse();
        }
    }

    if matches.is_empty() {
        return column![
            row![search, sort_button, add_button].spacing(10).align_y(Alignment::Center),
            centered_text(if app.contacts.is_empty() {
                "No contacts yet"
            } else {
                "No matches"
            }),
        ]
        .spacing(16)
        .into();
    }

    let count = matches.len();
    let mut list = column![].spacing(8);
    for (index, contact) in matches {
        list = list.push(contact_row(index, contact));
    }
    let count_label = text(format!("{count} contact{}", if count == 1 { "" } else { "s" }))
        .size(11)
        .color(muted_text());

    column![
        row![search, sort_button, add_button].spacing(10).align_y(Alignment::Center),
        count_label,
        scrollable(list).height(Length::Fill),
        contacts_io_row(app),
    ]
    .spacing(10)
    .into()
}

fn contacts_io_row(app: &App) -> Element<'_, Message> {
    let mut content = column![
        row![
            text_input("Path to a .json file", &app.contacts_io_path)
                .on_input(Message::ContactsIoPathChanged)
                .size(12)
                .padding(7)
                .width(Length::Fill),
            with_tooltip(
                button(text("...").size(10))
                    .style(theme::pill(Pill::Neutral))
                    .padding(7)
                    .on_press(Message::BrowseContactsImportPressed),
                "Choose a JSON file to import",
            ),
            with_tooltip(
                button(text("IMPORT").size(9))
                    .style(theme::pill(Pill::Neutral))
                    .padding(7)
                    .on_press(Message::ContactsImportPressed),
                "Merge contacts from this JSON file",
            ),
            with_tooltip(
                button(text("...").size(10))
                    .style(theme::pill(Pill::Neutral))
                    .padding(7)
                    .on_press(Message::BrowseContactsExportPressed),
                "Choose where to save the export",
            ),
            with_tooltip(
                button(text("EXPORT").size(9))
                    .style(theme::pill(Pill::Neutral))
                    .padding(7)
                    .on_press(Message::ContactsExportPressed),
                "Save all contacts to this JSON file",
            ),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    ]
    .spacing(5);

    if let Some(status) = &app.contacts_io_status {
        content = content.push(text(status.clone()).size(11).color(muted_text()));
    }

    content.into()
}

fn avatar_small(seed: &str) -> Element<'_, Message> {
    container(text(initial(seed)).size(13))
        .width(Length::Fixed(34.0))
        .height(Length::Fixed(34.0))
        .align_x(Alignment::Center)
        .align_y(Alignment::Center)
        .style(theme::avatar(theme::avatar_color(seed)))
        .into()
}

fn contact_row(index: usize, contact: &Contact) -> Element<'_, Message> {
    container(
        row![
            button(
                row![
                    avatar_small(&contact.name),
                    column![
                        text(contact.name.clone()).size(14),
                        text(contact.number.clone()).size(11).color(muted_text()),
                    ]
                    .spacing(2),
                ]
                .spacing(10)
                .align_y(Alignment::Center),
            )
            .style(theme::list_row)
            .padding(9)
            .width(Length::Fill)
            .on_press(Message::DialNumber(contact.number.clone())),
            with_tooltip(
                circle_button(text("EDIT").size(8), 32.0, theme::circle(false))
                    .on_press(Message::EditContactPressed(index)),
                "Edit contact",
            ),
            with_tooltip(
                circle_button(text("DEL").size(8).color(DANGER_TEXT), 32.0, theme::circle(false))
                    .on_press(Message::DeleteContactPressed(index)),
                "Delete contact",
            ),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .into()
}

fn contact_form_view(form: &ContactForm) -> Element<'_, Message> {
    let title = if form.editing_index.is_some() {
        "Edit Contact"
    } else {
        "New Contact"
    };
    let preview_seed = if form.name.is_empty() { &form.number } else { &form.name };
    let avatar = container(text(initial(preview_seed)).size(22))
        .width(Length::Fixed(58.0))
        .height(Length::Fixed(58.0))
        .align_x(Alignment::Center)
        .align_y(Alignment::Center)
        .style(theme::avatar(theme::avatar_color(preview_seed)));

    let fields = column![
        field(
            "Name",
            text_input("Name", &form.name)
                .on_input(Message::ContactNameChanged)
                .padding(9),
        ),
        field(
            "Number",
            text_input("Number", &form.number)
                .on_input(Message::ContactNumberChanged)
                .on_submit(Message::ContactSavePressed)
                .padding(9),
        ),
    ]
    .spacing(12);

    column![
        column![avatar].width(Length::Fill).align_x(Alignment::Center),
        text(title).size(15).color(muted_text()),
        section("Details", fields),
        row![
            button(text("Save").size(14))
                .style(theme::pill(Pill::Primary))
                .padding(11)
                .width(Length::Fill)
                .on_press(Message::ContactSavePressed),
            button(text("Cancel").size(14))
                .style(theme::pill(Pill::Neutral))
                .padding(11)
                .width(Length::Fill)
                .on_press(Message::ContactCancelPressed),
        ]
        .spacing(10),
    ]
    .spacing(14)
    .into()
}

fn history_tab(app: &App, _scale: f32) -> Element<'_, Message> {
    if app.call_history.is_empty() {
        return centered_text("No calls yet");
    }
    let now = history::now_unix();
    let mut list = column![].spacing(8);
    for entry in app.call_history.iter().rev() {
        list = list.push(history_row(now, entry));
    }
    let heading = text("Recent Calls").size(11).color(muted_text());
    column![heading, scrollable(list).height(Length::Fill)]
        .spacing(10)
        .into()
}

fn history_row(now: i64, entry: &HistoryEntry) -> Element<'_, Message> {
    let palette = theme::oxide_palette();
    let (glyph, glyph_color) = match (entry.direction, entry.outcome) {
        (_, CallOutcome::Missed) => ("IN", palette.danger),
        (_, CallOutcome::Rejected) | (_, CallOutcome::Failed) => ("X", palette.danger),
        (CallDirection::Incoming, CallOutcome::Answered) => ("IN", palette.success),
        (CallDirection::Outgoing, CallOutcome::Answered) => ("OUT", palette.text),
    };
    let detail = if entry.outcome == CallOutcome::Answered {
        format!(
            "{} - {}",
            history::relative_label(now, entry.unix_secs),
            history::duration_label(entry.duration_secs)
        )
    } else {
        history::relative_label(now, entry.unix_secs)
    };

    button(
        row![
            avatar_small(&entry.number),
            text(glyph).color(glyph_color).size(11),
            column![
                text(entry.number.clone()).size(14),
                text(detail).size(11).color(muted_text()),
            ]
            .spacing(2),
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    )
    .style(theme::list_row)
    .padding(9)
    .width(Length::Fill)
    .on_press(Message::DialNumber(entry.number.clone()))
    .into()
}

/// Adapter so `AudioDevice` (from `softphone_media`, no `Display`/`PartialEq`)
/// can be used as a `pick_list` item, which needs both.
#[derive(Debug, Clone, PartialEq)]
struct DeviceOption {
    id: String,
    description: String,
}

impl std::fmt::Display for DeviceOption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.description)
    }
}

/// Sentinel `DeviceOption::id` meaning "no explicit device pinned, use
/// PipeWire's system default" — matches `capture_target`/`playback_target:
/// None` in `MediaSession::start`. Empty string can never collide with a
/// real `node.name`, so it's a safe sentinel without widening the `Message`
/// variant to `Option<String>`.
const SYSTEM_DEFAULT_ID: &str = "";

fn muted_text() -> Color {
    Color {
        a: 0.55,
        ..theme::oxide_palette().text
    }
}

/// Stacks a small muted caption above `input` — the field-label convention
/// used throughout the settings windows instead of bare placeholder-only
/// inputs.
fn field<'a>(label: &'a str, input: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    column![text(label).size(11).color(muted_text()), input.into()]
        .spacing(4)
        .into()
}

/// A titled card grouping related fields — gives the settings windows
/// visual structure instead of one flat list of inputs.
fn section<'a>(title: &'a str, content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(
        column![
            text(title.to_uppercase())
                .size(10)
                .color(theme::oxide_palette().primary),
            content.into(),
        ]
        .spacing(9),
    )
    .style(theme::card)
    .padding(12)
    .width(Length::Fill)
    .into()
}

fn device_picker<'a>(
    label: &'a str,
    devices: &[softphone_media::AudioDevice],
    selected_id: &Option<String>,
    on_selected: impl Fn(String) -> Message + 'a,
) -> Element<'a, Message> {
    let mut options: Vec<DeviceOption> = vec![DeviceOption {
        id: SYSTEM_DEFAULT_ID.to_string(),
        description: "System Default".to_string(),
    }];
    options.extend(devices.iter().map(|d| DeviceOption {
        id: d.id.clone(),
        description: d.description.clone(),
    }));
    let selected = options
        .iter()
        .find(|o| o.id == selected_id.clone().unwrap_or_default())
        .cloned();

    field(
        label,
        pick_list(options, selected, move |opt: DeviceOption| {
            on_selected(opt.id)
        })
        .width(Length::Fill)
        .padding(10),
    )
}

fn settings_header<'a>(title: &'a str, subtitle: &'a str) -> Element<'a, Message> {
    column![
        text("OxideSip").size(19),
        text(title).size(13).color(theme::oxide_palette().primary),
        text(subtitle).size(11).color(muted_text()),
    ]
    .spacing(2)
    .into()
}

/// A left sidebar listing every configured account by name (click to load
/// it into the editor on the right, DEL to remove it) plus an "+ Add"
/// button that clears the editor to compose a new one — the whole
/// point being that adding an account never loses the ones you already
/// have, and switching which one you're editing is one click.
fn accounts_sidebar(app: &App) -> Element<'_, Message> {
    let mut list = column![].spacing(5);
    for (index, acc) in app.accounts.iter().enumerate() {
        let name = if acc.config.name.is_empty() {
            format!("Account {}", index + 1)
        } else {
            acc.config.name.clone()
        };
        let editing = app.editing_account == Some(index);
        let row_content = row![
            button(text(name).size(12))
                .style(theme::pill(Pill::Tab(editing)))
                .padding(7)
                .width(Length::Fill)
                .on_press(Message::SelectAccountForEditing(index)),
            with_tooltip(
                circle_button(text("DEL").size(7).color(DANGER_TEXT), 24.0, theme::circle(false))
                    .on_press(Message::DeleteAccountPressed(index)),
                "Remove this account",
            ),
        ]
        .spacing(4)
        .align_y(Alignment::Center);
        list = list.push(row_content);
    }
    let add_button = button(text("+ Add Account").size(11))
        .style(theme::pill(Pill::Neutral))
        .padding(7)
        .width(Length::Fill)
        .on_press(Message::AddAccountPressed);

    // This sidebar's own `scrollable` is independent of the editor's (see
    // `sip_settings_view`) — a long account list scrolls on its own without
    // affecting the credentials form, and vice versa.
    container(
        column![
            text("ACCOUNTS").size(10).color(theme::oxide_palette().primary),
            scrollable(list).height(Length::Fill),
            add_button,
        ]
        .spacing(8)
        .width(Length::Fixed(150.0))
        .height(Length::Fill),
    )
    .padding(iced::Padding {
        top: 0.0,
        right: 12.0,
        bottom: 0.0,
        left: 0.0,
    })
    .into()
}

fn sip_settings_view<'a>(app: &'a App, form: &'a SipSettingsForm, error: Option<&'a str>) -> Element<'a, Message> {
    let transport_options: Vec<SipTransport> = SIP_TRANSPORTS.to_vec();
    let account = column![
        field(
            "Account name",
            text_input("e.g. Front Desk", &form.name)
                .on_input(Message::SipSettingsNameChanged)
                .size(13)
                .padding(8),
        ),
        field(
            "Server",
            text_input("sip.example.com", &form.host)
                .on_input(Message::SipSettingsHostChanged)
                .size(13)
                .padding(8),
        ),
        field(
            "Port",
            text_input("5060", &form.port)
                .on_input(Message::SipSettingsPortChanged)
                .size(13)
                .padding(8),
        ),
        field(
            "Transport",
            pick_list(
                transport_options,
                Some(form.transport),
                Message::SipSettingsTransportChanged,
            )
            .text_size(13)
            .width(Length::Fill)
            .padding(8),
        ),
        field(
            "Username",
            text_input("extension", &form.username)
                .on_input(Message::SipSettingsUsernameChanged)
                .size(13)
                .padding(8),
        ),
        field(
            "Password",
            text_input("", &form.password)
                .on_input(Message::SipSettingsPasswordChanged)
                .on_submit(Message::SipSettingsSavePressed)
                .secure(true)
                .size(13)
                .padding(8),
        ),
    ]
    .spacing(12);

    let actions = row![
        button(text("Save").size(12))
            .style(theme::pill(Pill::Primary))
            .padding(9)
            .width(Length::Fill)
            .on_press(Message::SipSettingsSavePressed),
        button(text("Cancel").size(12))
            .style(theme::pill(Pill::Neutral))
            .padding(9)
            .width(Length::Fill)
            .on_press(Message::SipSettingsCancelPressed),
    ]
    .spacing(10);

    let mut editor = column![
        settings_header("SIP Setup", "Registrar account credentials"),
        section("SIP Account", account),
    ]
    .spacing(16);

    if let Some(error) = error {
        editor = editor.push(text(error).size(12).color(DANGER_TEXT));
    }
    editor = editor.push(actions);

    // A visible divider (not just spacing) between the accounts list and
    // the credentials editor — before this they ran right up against each
    // other with no separation at all.
    row![
        accounts_sidebar(app),
        rule::vertical(1),
        scrollable(editor.padding(18).width(Length::Fill)).width(Length::Fill).height(Length::Fill),
    ]
    .spacing(10)
    .height(Length::Fill)
    .into()
}

fn audio_settings_view<'a>(
    form: &'a AudioSettingsForm,
    input_devices: &[softphone_media::AudioDevice],
    output_devices: &[softphone_media::AudioDevice],
    error: Option<&'a str>,
) -> Element<'a, Message> {
    let devices = column![
        device_picker(
            "Microphone",
            input_devices,
            &form.input_device,
            Message::AudioSettingsInputDeviceChanged,
        ),
        device_picker(
            "Speaker",
            output_devices,
            &form.output_device,
            Message::AudioSettingsOutputDeviceChanged,
        ),
    ]
    .spacing(14);

    let mut codec_rows = column![].spacing(5);
    let last = form.codecs.len().saturating_sub(1);
    for (index, codec) in form.codecs.iter().enumerate() {
        let mut up = circle_button(text("^").size(10), 22.0, theme::circle(false));
        if index > 0 {
            up = up.on_press(Message::AudioSettingsCodecMoved(index, true));
        }
        let mut down = circle_button(text("v").size(10), 22.0, theme::circle(false));
        if index < last {
            down = down.on_press(Message::AudioSettingsCodecMoved(index, false));
        }
        codec_rows = codec_rows.push(
            row![
                text(format!("{}.", index + 1)).size(12).color(muted_text()),
                text(codec.to_string()).size(12).width(Length::Fill),
                up,
                down,
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );
    }
    let codec_picker = field(
        "Codec priority (for calls we place)",
        column![
            text("Highest first — the top entry the other side also supports wins.")
                .size(11)
                .color(muted_text()),
            codec_rows,
        ]
        .spacing(8),
    );
    let srtp_toggle = row![
        column![
            text("SRTP media encryption").size(13),
            text("Encrypts call audio (SDES-SRTP). Requires PBX support.")
                .size(11)
                .color(muted_text()),
        ]
        .spacing(2)
        .width(Length::Fill),
        toggler(form.srtp).on_toggle(Message::AudioSettingsSrtpToggled),
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    let power_user = column![codec_picker, srtp_toggle].spacing(16);

    let actions = row![
        button(text("Save").size(12))
            .style(theme::pill(Pill::Primary))
            .padding(9)
            .width(Length::Fill)
            .on_press(Message::AudioSettingsSavePressed),
        button(text("Cancel").size(12))
            .style(theme::pill(Pill::Neutral))
            .padding(9)
            .width(Length::Fill)
            .on_press(Message::AudioSettingsCancelPressed),
    ]
    .spacing(10);

    let mut content = column![
        settings_header("Audio & Codecs", "Devices, encryption, and codec preference"),
        section("Audio Devices", devices),
        section("Codec & Encryption", power_user),
    ]
    .spacing(18);

    if let Some(error) = error {
        content = content.push(text(error).size(13).color(DANGER_TEXT));
    }
    content = content.push(actions);

    scrollable(content.padding(20)).into()
}

fn toggle_row<'a>(
    title: &'a str,
    detail: &'a str,
    value: bool,
    on_toggle: impl Fn(bool) -> Message + 'a,
) -> Element<'a, Message> {
    row![
        column![
            text(title).size(13),
            text(detail).size(11).color(muted_text()),
        ]
        .spacing(2)
        .width(Length::Fill),
        toggler(value).on_toggle(on_toggle),
    ]
    .spacing(10)
    .align_y(Alignment::Center)
    .into()
}

/// Combines two source lists: ordinary PipeWire sinks (hardware or a
/// virtual sink the user set up as an app's input) and live application
/// capture streams (e.g. Discord's own voice-engine node, only present
/// while it's actually in a voice channel — see `list_app_capture_streams`)
/// — plus an explicit "None" entry to disable streaming entirely, distinct
/// from `device_picker`'s "System Default" since there's no sensible
/// default target for this feature. App streams are prefixed "App:" so
/// they're distinguishable from hardware sinks in one flat dropdown.
fn secondary_output_picker<'a>(
    sinks: &[softphone_media::AudioDevice],
    app_streams: &[softphone_media::AudioDevice],
    selected: &Option<String>,
) -> Element<'a, Message> {
    let mut options: Vec<DeviceOption> = vec![DeviceOption {
        id: SYSTEM_DEFAULT_ID.to_string(),
        description: "None".to_string(),
    }];
    options.extend(app_streams.iter().map(|d| DeviceOption {
        id: d.id.clone(),
        description: format!("App: {}", d.description),
    }));
    options.extend(sinks.iter().map(|d| DeviceOption {
        id: d.id.clone(),
        description: d.description.clone(),
    }));
    let selected = options
        .iter()
        .find(|o| o.id == selected.clone().unwrap_or_default())
        .cloned();

    let refresh = with_tooltip(
        button(text("REFRESH").size(9))
            .style(theme::pill(Pill::Neutral))
            .padding(7)
            .on_press(Message::RefreshSecondaryOutputTargets),
        "Re-scan for sinks and live app streams (e.g. after joining a Discord voice channel)",
    );

    field(
        "Stream callee's voice to",
        column![
            row![
                pick_list(options, selected, |opt: DeviceOption| {
                    Message::SettingsSecondaryOutputChanged(
                        (opt.id != SYSTEM_DEFAULT_ID).then_some(opt.id),
                    )
                })
                .width(Length::Fill)
                .padding(10),
                refresh,
            ]
            .spacing(6)
            .align_y(Alignment::Center),
            text(
                "Pick a live app stream (e.g. Discord, once you're in a voice channel) to send \
                 the other party's voice straight there, or a sink if you've set up your own \
                 routing."
            )
            .size(11)
            .color(muted_text()),
        ]
        .spacing(6),
    )
}

fn settings_view<'a>(
    form: &'a SettingsForm,
    output_devices: &[softphone_media::AudioDevice],
    app_capture_streams: &[softphone_media::AudioDevice],
    error: Option<&'a str>,
) -> Element<'a, Message> {
    let call_handling = column![
        toggle_row(
            "Do Not Disturb",
            "Automatically decline every incoming call.",
            form.dnd,
            Message::SettingsDndToggled,
        ),
        toggle_row(
            "Auto-Answer",
            "Automatically accept every incoming call.",
            form.auto_answer,
            Message::SettingsAutoAnswerToggled,
        ),
    ]
    .spacing(14);

    let forwarding = column![
        toggle_row(
            "Call Forwarding",
            "Redirect incoming calls instead of ringing here.",
            form.forwarding_enabled,
            Message::SettingsForwardingToggled,
        ),
        field(
            "Forward to",
            text_input("Number or SIP URI", &form.forwarding_number)
                .on_input(Message::SettingsForwardingNumberChanged)
                .size(13)
                .padding(8),
        ),
    ]
    .spacing(12);

    let mut deny_list_rows = column![].spacing(5);
    for (index, entry) in form.deny_list.iter().enumerate() {
        deny_list_rows = deny_list_rows.push(
            row![
                text(entry.clone()).size(12).width(Length::Fill),
                with_tooltip(
                    circle_button(text("DEL").size(7).color(DANGER_TEXT), 24.0, theme::circle(false))
                        .on_press(Message::SettingsDenyListRemovePressed(index)),
                    "Remove from deny list",
                ),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        );
    }
    let deny_list = column![
        text("Numbers or domains listed here are rejected before they ever ring.")
            .size(11)
            .color(muted_text()),
        row![
            text_input("Number or domain", &form.deny_list_input)
                .on_input(Message::SettingsDenyListInputChanged)
                .on_submit(Message::SettingsDenyListAddPressed)
                .size(13)
                .padding(8)
                .width(Length::Fill),
            button(text("Add").size(12))
                .style(theme::pill(Pill::Neutral))
                .padding(8)
                .on_press(Message::SettingsDenyListAddPressed),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
        deny_list_rows,
    ]
    .spacing(10);

    let recording = column![
        toggle_row(
            "Record Calls",
            "Save a WAV recording of every call to the folder below.",
            form.recording_enabled,
            Message::SettingsRecordingToggled,
        ),
        field(
            "Save to folder",
            row![
                text_input("/home/you/Recordings", &form.recording_path)
                    .on_input(Message::SettingsRecordingPathChanged)
                    .size(13)
                    .padding(8)
                    .width(Length::Fill),
                button(text("BROWSE").size(9))
                    .style(theme::pill(Pill::Neutral))
                    .padding(8)
                    .on_press(Message::BrowseRecordingPathPressed),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        ),
    ]
    .spacing(12);

    let secondary_output =
        secondary_output_picker(output_devices, app_capture_streams, &form.secondary_output_target);

    let actions = row![
        button(text("Save").size(12))
            .style(theme::pill(Pill::Primary))
            .padding(9)
            .width(Length::Fill)
            .on_press(Message::SettingsSavePressed),
        button(text("Cancel").size(12))
            .style(theme::pill(Pill::Neutral))
            .padding(9)
            .width(Length::Fill)
            .on_press(Message::SettingsCancelPressed),
    ]
    .spacing(10);

    let mut content = column![
        settings_header("Settings", "Call handling and recording"),
        section("Incoming Calls", call_handling),
        section("Forwarding", forwarding),
        section("Deny List", deny_list),
        section("Recording", recording),
        section("Secondary Output", secondary_output),
    ]
    .spacing(16);

    if let Some(error) = error {
        content = content.push(text(error).size(12).color(DANGER_TEXT));
    }
    content = content.push(actions);

    scrollable(content.padding(18)).into()
}
