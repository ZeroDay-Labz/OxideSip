//! The main window: a left sidebar, a top bar and the current page, with
//! modal overlays (contact form, quit confirmation) and toasts stacked on
//! top. Pages live in their own modules.

pub mod compact;
pub mod components;
pub mod contacts;
pub mod history;
pub mod phone;
pub mod settings;
pub mod sidebar;

use crate::app::{App, CallUiState, Message, Page};
use crate::theme::{self, Tokens};
use crate::ui::components::{muted, text_button};
use crate::{branding, icons};
use iced::widget::{button, column, container, mouse_area, row, stack, text, toggler};
use iced::{Alignment, Element, Length};

/// Below this window width the sidebar collapses to an icon rail.
pub const COLLAPSE_BELOW: f32 = 860.0;

pub fn main_view(app: &App) -> Element<'_, Message> {
    let t = theme::tokens(app.mode());

    if app.compact_mode
        && let Some(acc) = app.selected()
        && let CallUiState::Active {
            number,
            remote_name,
            media,
            muted,
            on_hold,
            answered_at,
            ..
        } = acc.selected_call()
    {
        return with_toasts(
            app,
            compact::view(
                &t,
                &app.contacts,
                number,
                remote_name.as_deref(),
                app.media_ready(media),
                *muted,
                *on_hold,
                app.elapsed(*answered_at),
            ),
        );
    }

    let collapsed = app.sidebar_collapsed || app.main_window_size.width < COLLAPSE_BELOW;
    let page: Element<'_, Message> = match app.page {
        Page::Phone => phone::view(app, &t),
        Page::Contacts => contacts::view(app, &t),
        Page::History => history::view(app, &t),
        Page::Settings => settings::view(app, &t),
    };

    let content = column![topbar(app, &t), page]
        .spacing(theme::space::LG)
        .padding(iced::Padding {
            top: theme::space::LG,
            right: theme::space::XL,
            bottom: theme::space::XL,
            left: theme::space::XL,
        })
        .width(Length::Fill)
        .height(Length::Fill);

    let shell = container(row![sidebar::view(app, &t, collapsed), content].height(Length::Fill))
        .width(Length::Fill)
        .height(Length::Fill)
        .style(theme::app_background);

    let mut layers: Vec<Element<'_, Message>> = vec![shell.into()];
    if let Some(form) = &app.contact_form {
        layers.push(modal(contacts::form_view(&t, form), Message::ContactCancelPressed));
    }
    if app.quit_prompt {
        layers.push(modal(quit_prompt(&t), Message::QuitCancelled));
    }
    with_toasts(app, stack(layers).into())
}

/// Overlays the toast layer on a window's content.
pub fn with_toasts<'a>(app: &'a App, content: Element<'a, Message>) -> Element<'a, Message> {
    if app.toasts.is_empty() {
        return content;
    }
    let theme = theme::theme(app.mode());
    stack![content, crate::toast::layer(&app.toasts, &theme)].into()
}

/// A dimmed scrim with a centred card; clicking the scrim sends `dismiss`.
fn modal<'a>(card: Element<'a, Message>, dismiss: Message) -> Element<'a, Message> {
    let scrim = mouse_area(
        container(text(""))
            .width(Length::Fill)
            .height(Length::Fill)
            .style(theme::scrim),
    )
    .on_press(dismiss);
    let centred = container(
        container(card)
            .style(theme::overlay)
            .padding(theme::space::XL)
            .width(Length::Fixed(440.0)),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .align_x(Alignment::Center)
    .align_y(Alignment::Center);
    stack![scrim, centred].into()
}

fn quit_prompt<'a>(t: &Tokens) -> Element<'a, Message> {
    column![
        row![
            icons::alert(20.0, t.warning),
            text("A call is in progress")
                .size(theme::text_size::TITLE)
                .font(branding::UI_FONT_SEMIBOLD),
        ]
        .spacing(theme::space::MD)
        .align_y(Alignment::Center),
        text("Quitting hangs up every active call on every account.")
            .size(theme::text_size::BODY)
            .color(muted(t)),
        row![
            text_button("Keep running", theme::button_soft).on_press(Message::QuitCancelled),
            text_button("Hang up and quit", theme::button_danger).on_press(Message::QuitConfirmed),
        ]
        .spacing(theme::space::MD),
    ]
    .spacing(theme::space::LG)
    .into()
}

/// Page title, subtitle and the page's primary actions.
fn topbar<'a>(app: &'a App, t: &Tokens) -> Element<'a, Message> {
    let acc = app.selected();
    let account_label = acc.map(|a| {
        let name = if a.config.name.trim().is_empty() {
            a.config.username.clone()
        } else {
            a.config.name.clone()
        };
        format!("{name} · ext {}", a.config.username)
    });
    let (title, subtitle) = match app.page {
        Page::Phone => (
            "Phone",
            account_label.unwrap_or_else(|| "No account configured".to_string()),
        ),
        Page::Contacts => ("Contacts", format!("{} saved", app.contacts.len())),
        Page::History => ("History", format!("{} calls", app.call_history.len())),
        Page::Settings => ("Settings", app.settings_section.subtitle().to_string()),
    };
    let heading = column![
        text(title)
            .size(theme::text_size::TITLE)
            .font(branding::UI_FONT_SEMIBOLD),
        text(subtitle).size(theme::text_size::CAPTION).color(muted(t)),
    ]
    .spacing(2)
    .width(Length::Fill);

    let mut actions = row![].spacing(theme::space::SM).align_y(Alignment::Center);
    match app.page {
        Page::Phone => {
            actions = actions.push(
                container(
                    row![
                        text("DND").size(theme::text_size::CAPTION).color(muted(t)),
                        toggler(app.settings.dnd)
                            .on_toggle(Message::DndQuickToggled)
                            .style(theme::toggler_style)
                            .size(20),
                    ]
                    .spacing(theme::space::SM)
                    .align_y(Alignment::Center),
                )
                .padding([4, 10])
                .style(theme::chip),
            );
            let in_call = acc.is_some_and(|a| matches!(a.selected_call(), CallUiState::Active { .. }));
            actions = actions.push(components::icon_button(
                icons::minimize(16.0, muted(t)),
                "Shrink to a compact call bar (Ctrl+Shift+C)",
                false,
                in_call.then_some(Message::CompactToggled),
            ));
        }
        Page::Contacts => {
            actions = actions.push(
                button(
                    row![
                        icons::user_plus(15.0, iced::Color::WHITE),
                        text("Add contact").size(theme::text_size::BODY)
                    ]
                    .spacing(theme::space::SM)
                    .align_y(Alignment::Center),
                )
                .style(theme::button_primary)
                .padding([theme::space::SM + 1.0, theme::space::LG])
                .on_press(Message::AddContactPressed),
            );
        }
        Page::History => {
            if !app.call_history.is_empty() {
                actions = actions.push(
                    button(
                        row![icons::trash(14.0, t.danger), text("Clear").size(theme::text_size::BODY)]
                            .spacing(theme::space::SM)
                            .align_y(Alignment::Center),
                    )
                    .style(theme::button_danger)
                    .padding([theme::space::SM + 1.0, theme::space::LG])
                    .on_press(Message::ClearHistoryPressed),
                );
            }
        }
        Page::Settings => {}
    }
    let theme_icon = match app.mode() {
        theme::Mode::Dark => icons::sun(16.0, muted(t)),
        theme::Mode::Light => icons::moon(16.0, muted(t)),
    };
    actions = actions.push(components::icon_button(
        theme_icon,
        "Toggle light / dark",
        false,
        Some(Message::ThemeQuickToggled),
    ));

    row![heading, actions]
        .align_y(Alignment::Center)
        .spacing(theme::space::LG)
        .into()
}
