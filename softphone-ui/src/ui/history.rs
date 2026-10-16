//! Call history page: filter chips, grouped rows, per-row actions.

use crate::app::{App, HistoryFilter, Message};
use crate::history::{self, CallDirection, CallOutcome, HistoryEntry};
use crate::icons;
use crate::theme::{self, Pill, Tokens};
use crate::ui::components::{self, avatar, call_avatar_label, contact_name_for, empty_state, muted};
use iced::widget::{button, column, container, mouse_area, row, scrollable, text};
use iced::{Alignment, Element, Length};

pub fn view<'a>(app: &'a App, t: &Tokens) -> Element<'a, Message> {
    let chip = |label: &'static str, filter: HistoryFilter| {
        let selected = app.history_filter == filter;
        button(text(label).size(theme::text_size::BODY))
            .style(theme::pill(Pill::Tab(if selected { 1.0 } else { 0.0 })))
            .padding([theme::space::XS + 2.0, theme::space::MD])
            .on_press(Message::HistoryFilterChanged(filter))
    };
    let filters = container(
        row![
            chip("All", HistoryFilter::All),
            chip("Missed", HistoryFilter::Missed),
            chip("Incoming", HistoryFilter::Incoming),
            chip("Outgoing", HistoryFilter::Outgoing),
        ]
        .spacing(theme::space::XS),
    )
    .padding(3)
    .style(theme::tab_track);

    let now = history::now_unix();
    let entries: Vec<(usize, &HistoryEntry)> = app
        .call_history
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, e)| match app.history_filter {
            HistoryFilter::All => true,
            HistoryFilter::Missed => e.outcome == CallOutcome::Missed,
            HistoryFilter::Incoming => e.direction == CallDirection::Incoming,
            HistoryFilter::Outgoing => e.direction == CallDirection::Outgoing,
        })
        .collect();

    let body: Element<'a, Message> = if entries.is_empty() {
        empty_state(
            t,
            icons::history(40.0, theme::tint(t.text_dim, 0.6)),
            if app.call_history.is_empty() {
                "No calls yet"
            } else {
                "Nothing matches this filter"
            },
            if app.call_history.is_empty() {
                "Calls you place and receive show up here."
            } else {
                ""
            },
        )
    } else {
        let mut list = column![].spacing(theme::space::XS);
        let mut last_day: Option<i64> = None;
        for (index, entry) in entries {
            // Same integer-day bucket `history::relative_label` uses, so the
            // group header never disagrees with the rows under it.
            let day = (now - entry.unix_secs).max(0) / 86_400;
            if last_day != Some(day) {
                last_day = Some(day);
                let label = match day {
                    0 => "Today".to_string(),
                    1 => "Yesterday".to_string(),
                    d if d < 7 => format!("{d} days ago"),
                    _ => history::relative_label(now, entry.unix_secs),
                };
                list = list.push(
                    container(text(label).size(theme::text_size::CAPTION).color(muted(t)))
                        .padding(iced::Padding::ZERO.top(theme::space::SM).left(theme::space::XS)),
                );
            }
            list = list.push(history_row(app, t, now, index, entry));
        }
        scrollable(list.padding(iced::Padding::ZERO.right(theme::space::SM)))
            .style(theme::scrollable_style)
            .height(Length::Fill)
            .into()
    };

    container(column![filters, body].spacing(theme::space::MD).height(Length::Fill))
        .padding(theme::space::LG)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(theme::panel)
        .into()
}

fn history_row<'a>(app: &'a App, t: &Tokens, now: i64, index: usize, entry: &'a HistoryEntry) -> Element<'a, Message> {
    let (glyph, outcome) = match (entry.direction, entry.outcome) {
        (_, CallOutcome::Missed) => (icons::phone_missed(16.0, t.danger), "Missed"),
        (_, CallOutcome::Rejected) => (icons::phone_off(16.0, t.text_dim), "Declined"),
        (_, CallOutcome::Failed) => (icons::x_circle(16.0, t.danger), "Failed"),
        (CallDirection::Incoming, CallOutcome::Answered) => (icons::phone_incoming(16.0, t.success), "Incoming"),
        (CallDirection::Outgoing, CallOutcome::Answered) => (icons::phone_outgoing(16.0, t.info), "Outgoing"),
    };
    let name = contact_name_for(&app.contacts, &entry.number).or(entry.name.as_deref());
    let primary = name.unwrap_or(&entry.number).to_string();
    let mut detail = match name {
        Some(_) => format!("{} · {outcome}", entry.number),
        None => outcome.to_string(),
    };
    if entry.outcome == CallOutcome::Answered {
        detail.push_str(&format!(" · {}", history::duration_label(entry.duration_secs)));
    }
    let when = history::relative_label(now, entry.unix_secs);
    let menu_open = app.history_context_menu == Some(index);

    let main = button(
        row![
            avatar(&primary, call_avatar_label_or_initials(app, entry), 36.0, 0.0),
            glyph,
            column![
                text(primary).size(theme::text_size::BODY + 1.0),
                text(detail).size(theme::text_size::CAPTION).color(muted(t)),
            ]
            .spacing(2)
            .width(Length::Fill),
            text(when).size(theme::text_size::CAPTION).color(muted(t)),
        ]
        .spacing(theme::space::MD)
        .align_y(Alignment::Center),
    )
    .style(theme::button_ghost)
    .padding([theme::space::SM, theme::space::MD])
    .width(Length::Fill)
    .on_press(Message::DialNumber(entry.number.clone()));
    let main = mouse_area(main).on_right_press(Message::HistoryContextMenuToggled(index));

    let mut actions = row![components::icon_button(
        icons::phone(15.0, t.success),
        "Call back",
        false,
        Some(Message::DialNumber(entry.number.clone()))
    ),]
    .spacing(theme::space::XS)
    .align_y(Alignment::Center);
    if name.is_none() || contact_name_for(&app.contacts, &entry.number).is_none() {
        actions = actions.push(components::icon_button(
            icons::user_plus(15.0, muted(t)),
            "Add to contacts",
            menu_open,
            Some(Message::AddHistoryEntryToContacts(index)),
        ));
    }

    container(row![main, actions].spacing(theme::space::XS).align_y(Alignment::Center))
        .padding(iced::Padding::ZERO.right(theme::space::XS))
        .style(theme::raised)
        .into()
}

fn call_avatar_label_or_initials(app: &App, entry: &HistoryEntry) -> String {
    match contact_name_for(&app.contacts, &entry.number).or(entry.name.as_deref()) {
        Some(name) => components::contact_initials(name),
        None => call_avatar_label(&app.contacts, &entry.number),
    }
}
