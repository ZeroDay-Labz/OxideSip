use crate::app::{App, Message};
use crate::theme::{self, ThemeSetting, Tokens};
use crate::ui::components::{field, muted, picker, section};
use crate::ui::settings::heading;
use iced::widget::{column, text};
use iced::{Element, Length};

pub fn view<'a>(app: &'a App, t: &Tokens) -> Element<'a, Message> {
    let appearance = column![
        field(
            t,
            "Colour scheme",
            picker(
                ThemeSetting::ALL.to_vec(),
                Some(app.settings.theme),
                Message::SettingsThemeChanged
            )
            .width(Length::Fixed(260.0)),
        ),
        text("\"Follow system\" tracks the desktop's light/dark preference live. Changes apply immediately.")
            .size(theme::text_size::CAPTION)
            .color(muted(t)),
    ]
    .spacing(theme::space::MD);

    let window = column![
        text("The window remembers its size and position. Below 860 px wide the sidebar collapses to icons.")
            .size(theme::text_size::BODY),
        text("During a call, the shrink button in the top bar (or Ctrl+Shift+C) opens a small always-on-top call bar you can leave over other windows.")
            .size(theme::text_size::CAPTION)
            .color(muted(t)),
    ]
    .spacing(theme::space::SM);

    column![
        heading(t, "Appearance", "Theme and window behaviour."),
        section("Theme", appearance),
        section("Window", window),
    ]
    .spacing(theme::space::LG)
    .into()
}
