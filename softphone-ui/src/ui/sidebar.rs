//! Left navigation: logo, pages, account switcher and the registration pill.

use crate::app::{App, Message, Page, RegistrationStatus};
use crate::theme::{self, Tokens};
use crate::ui::components::{self, muted, picker, with_tooltip};
use crate::{branding, icons};
use iced::widget::{button, column, container, row, text};
use iced::{Alignment, Color, Element, Length};
use std::time::Instant;

pub const WIDTH: f32 = 224.0;
pub const RAIL_WIDTH: f32 = 68.0;

pub fn view<'a>(app: &'a App, t: &Tokens, collapsed: bool) -> Element<'a, Message> {
    let now = Instant::now();
    let width = if collapsed { RAIL_WIDTH } else { WIDTH };

    let logo = container(
        row![
            icons::logo(26.0, t.accent),
            if collapsed {
                text("")
            } else {
                text(branding::APP_NAME)
                    .size(theme::text_size::TITLE - 2.0)
                    .font(branding::UI_FONT_SEMIBOLD)
            },
        ]
        .spacing(theme::space::MD)
        .align_y(Alignment::Center),
    )
    .padding(iced::Padding {
        top: theme::space::SM,
        right: theme::space::SM,
        bottom: theme::space::LG,
        left: theme::space::SM + 2.0,
    });

    let nav_item = |page: Page, label: &'static str, glyph: fn(f32, Color) -> Element<'a, Message>, badge: u32| {
        let amount = app.nav_animations.amount(page, now);
        let color = theme::lerp_color(t.text_dim, t.accent, amount);
        let mut content = row![glyph(18.0, color)]
            .spacing(theme::space::MD)
            .align_y(Alignment::Center);
        if !collapsed {
            content = content.push(text(label).size(theme::text_size::BODY + 1.0).width(Length::Fill));
        }
        if badge > 0 {
            content = content.push(
                container(
                    text(badge.to_string())
                        .size(theme::text_size::CAPTION)
                        .color(Color::WHITE),
                )
                .padding([1, 6])
                .style(theme::led(t.danger)),
            );
        }
        let b = button(content)
            .style(theme::nav(amount))
            .padding([theme::space::SM + 2.0, theme::space::MD])
            .width(Length::Fill)
            .on_press(Message::PageSelected(page));
        if collapsed { with_tooltip(b, label) } else { b.into() }
    };

    let nav = column![
        nav_item(Page::Phone, "Phone", icons::phone, 0),
        nav_item(Page::Contacts, "Contacts", icons::book_user, 0),
        nav_item(Page::History, "History", icons::history, app.unseen_missed),
        nav_item(Page::Settings, "Settings", icons::settings, 0),
    ]
    .spacing(theme::space::XS);

    let mut col = column![logo, nav].spacing(theme::space::SM).height(Length::Fill);

    if app.accounts.len() > 1 && !collapsed {
        let options: Vec<AccountOption> = app
            .accounts
            .iter()
            .enumerate()
            .map(|(index, a)| AccountOption {
                index,
                name: if a.config.name.trim().is_empty() {
                    a.config.username.clone()
                } else {
                    a.config.name.clone()
                },
            })
            .collect();
        let selected = options.get(app.selected_account).cloned();
        col = col.push(
            column![
                text("ACCOUNT").size(theme::text_size::CAPTION - 1.0).color(muted(t)),
                picker(options, selected, |opt: AccountOption| Message::AccountSwitched(
                    opt.index
                ))
                .width(Length::Fill),
            ]
            .spacing(theme::space::XS)
            .padding(iced::Padding {
                top: theme::space::LG,
                right: 0.0,
                bottom: 0.0,
                left: 0.0,
            }),
        );
    }

    col = col.push(container(text("")).height(Length::Fill));
    col = col.push(status_pill(app, t, collapsed));

    container(col.padding(theme::space::MD).width(Length::Fixed(width)))
        .height(Length::Fill)
        .style(theme::sidebar)
        .into()
}

#[derive(Debug, Clone, PartialEq)]
struct AccountOption {
    index: usize,
    name: String,
}

impl std::fmt::Display for AccountOption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}

/// Registration state of the selected account, as a pill; click opens the
/// Accounts settings.
fn status_pill<'a>(app: &'a App, t: &Tokens, collapsed: bool) -> Element<'a, Message> {
    let Some(acc) = app.selected() else {
        let b = button(
            row![
                components::dot(t.text_dim, 8.0),
                if collapsed {
                    text("")
                } else {
                    text("No account").size(theme::text_size::CAPTION)
                }
            ]
            .spacing(theme::space::SM)
            .align_y(Alignment::Center),
        )
        .style(theme::button_soft)
        .padding([theme::space::SM, theme::space::MD])
        .width(Length::Fill)
        .on_press(Message::OpenSipSettings);
        return with_tooltip(b, "Add a SIP account");
    };
    let (color, label, detail) = match &acc.registration {
        RegistrationStatus::Connecting => (
            t.warning,
            "Connecting…".to_string(),
            "Registering with the SIP server".to_string(),
        ),
        RegistrationStatus::Registered {
            expires,
            registered_at,
            rtt_ms,
        } => {
            let remaining = expires.saturating_sub(registered_at.elapsed().as_secs() as u32);
            (
                t.success,
                format!("Registered · {rtt_ms} ms"),
                format!(
                    "Registered with {} — renews in {}:{:02}",
                    acc.config.sip_server_host,
                    remaining / 60,
                    remaining % 60
                ),
            )
        }
        RegistrationStatus::Failed { reason } => (
            t.danger,
            "Offline".to_string(),
            format!("Registration failed: {reason}"),
        ),
        RegistrationStatus::Halted { reason } => (
            t.danger,
            "Stopped".to_string(),
            format!("Registration stopped: {reason} — re-save the account to retry"),
        ),
    };
    let mut content = row![components::dot(color, 8.0)]
        .spacing(theme::space::SM)
        .align_y(Alignment::Center);
    if !collapsed {
        let name = if acc.config.name.trim().is_empty() {
            acc.config.username.clone()
        } else {
            acc.config.name.clone()
        };
        content = content.push(
            column![
                text(name).size(theme::text_size::CAPTION + 1.0),
                text(label).size(theme::text_size::CAPTION).color(muted(t)),
            ]
            .spacing(1),
        );
    }
    let b = button(content)
        .style(theme::button_soft)
        .padding([theme::space::SM, theme::space::MD])
        .width(Length::Fill)
        .on_press(Message::OpenSipSettings);
    with_tooltip(b, detail)
}
