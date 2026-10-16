//! Settings page: a section list on the left, the selected section's form
//! on the right. Replaces the three detached settings windows.

pub mod about;
pub mod accounts;
pub mod appearance;
pub mod audio;
pub mod calls;

use crate::app::{App, Message, SettingsSection};
use crate::theme::{self, Tokens};
use crate::ui::components::muted;
use crate::{branding, icons};
use iced::widget::{button, column, container, row, scrollable, text};
use iced::{Alignment, Color, Element, Length};

pub fn view<'a>(app: &'a App, t: &Tokens) -> Element<'a, Message> {
    let item = |section: SettingsSection, glyph: fn(f32, Color) -> Element<'a, Message>| {
        let selected = app.settings_section == section;
        let color = if selected { t.accent } else { t.text_dim };
        button(
            row![glyph(16.0, color), text(section.label()).size(theme::text_size::BODY)]
                .spacing(theme::space::MD)
                .align_y(Alignment::Center),
        )
        .style(theme::nav(if selected { 1.0 } else { 0.0 }))
        .padding([theme::space::SM + 1.0, theme::space::MD])
        .width(Length::Fill)
        .on_press(Message::SettingsSectionSelected(section))
    };
    let list = column![
        item(SettingsSection::Accounts, icons::server),
        item(SettingsSection::Audio, icons::headphones),
        item(SettingsSection::Calls, icons::phone_call),
        item(SettingsSection::Appearance, icons::sun),
        item(SettingsSection::About, icons::info),
    ]
    .spacing(theme::space::XS);
    let nav = container(list)
        .padding(theme::space::SM)
        .width(Length::Fixed(200.0))
        .style(theme::panel);

    let content: Element<'a, Message> = match app.settings_section {
        SettingsSection::Accounts => accounts::view(app, t),
        SettingsSection::Audio => audio::view(app, t),
        SettingsSection::Calls => calls::view(app, t),
        SettingsSection::Appearance => appearance::view(app, t),
        SettingsSection::About => about::view(app, t),
    };
    let content = scrollable(container(content).padding(iced::Padding::ZERO.right(theme::space::SM)))
        .style(theme::scrollable_style)
        .height(Length::Fill)
        .width(Length::Fill);

    row![nav, content].spacing(theme::space::LG).height(Length::Fill).into()
}

/// Section heading with a one-line description.
pub fn heading<'a>(t: &Tokens, title: &'a str, detail: &'a str) -> Element<'a, Message> {
    column![
        text(title)
            .size(theme::text_size::TITLE)
            .font(branding::UI_FONT_SEMIBOLD),
        text(detail).size(theme::text_size::BODY).color(muted(t)),
    ]
    .spacing(theme::space::XS)
    .into()
}

/// Save / Cancel row for a form.
pub fn actions<'a>(save: Message, cancel: Message, save_label: &'a str) -> Element<'a, Message> {
    row![
        crate::ui::components::text_button("Cancel", theme::button_soft).on_press(cancel),
        container(text("")).width(Length::Fill),
        crate::ui::components::text_button(save_label, theme::button_primary).on_press(save),
    ]
    .spacing(theme::space::SM)
    .align_y(Alignment::Center)
    .into()
}
