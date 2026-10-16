//! Transient feedback messages shown bottom-right of the main window.
//! Replaces the old single `App.error` string that every window rendered
//! inline with no way to dismiss it.

use crate::app::Message;
use crate::icons;
use crate::theme::{self, ToastKind};
use iced::widget::{button, column, container, row, text};
use iced::{Alignment, Element, Length};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Toast {
    pub id: u64,
    pub kind: ToastKind,
    pub title: String,
    pub detail: Option<String>,
    pub created: Instant,
    /// `None` = stays until dismissed.
    pub ttl: Option<Duration>,
}

impl Toast {
    pub fn expired(&self, now: Instant) -> bool {
        self.ttl.is_some_and(|ttl| now.duration_since(self.created) >= ttl)
    }
}

/// Default lifetime per kind: errors stay longer, successes go quickly.
pub fn default_ttl(kind: ToastKind) -> Option<Duration> {
    Some(match kind {
        ToastKind::Error => Duration::from_secs(9),
        ToastKind::Warning => Duration::from_secs(7),
        ToastKind::Info => Duration::from_secs(5),
        ToastKind::Success => Duration::from_secs(3),
    })
}

/// The stacked toast layer, to be placed over the page in a `stack!`.
pub fn layer<'a>(toasts: &'a [Toast], theme: &iced::Theme) -> Element<'a, Message> {
    let muted_text = || theme::t(theme).text_dim;
    let mut col = column![].spacing(theme::space::SM).align_x(Alignment::End);
    for toast in toasts.iter().rev().take(4) {
        let accent = theme::toast_accent(toast.kind, theme);
        let glyph = match toast.kind {
            ToastKind::Info => icons::info(16.0, accent),
            ToastKind::Success => icons::circle_check(16.0, accent),
            ToastKind::Warning => icons::alert(16.0, accent),
            ToastKind::Error => icons::x_circle(16.0, accent),
        };
        let mut body = column![text(&toast.title).size(theme::text_size::BODY)].spacing(2);
        if let Some(detail) = &toast.detail {
            body = body.push(text(detail).size(theme::text_size::CAPTION).color(muted_text()));
        }
        let close = button(icons::x(14.0, muted_text()))
            .style(theme::button_ghost)
            .padding(4)
            .on_press(Message::ToastDismissed(toast.id));
        col = col.push(
            container(
                row![glyph, body.width(Length::Fill), close]
                    .spacing(theme::space::MD)
                    .align_y(Alignment::Center),
            )
            .padding([theme::space::MD, theme::space::LG])
            .width(Length::Fixed(360.0))
            .style(theme::toast(toast.kind)),
        );
    }
    container(col)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Alignment::End)
        .align_y(Alignment::End)
        .padding(theme::space::LG)
        .into()
}
