use crate::app::{App, Message};
use crate::theme::{self, Tokens};
use crate::ui::components::{muted, section, text_button};
use crate::ui::settings::heading;
use crate::{branding, icons};
use iced::widget::{column, container, row, text};
use iced::{Alignment, Element, Length};

const SHORTCUTS: &[(&str, &str)] = &[
    ("Enter", "Call the typed number / answer a ringing call"),
    ("Esc", "Decline a ringing call · press twice to hang up"),
    ("0–9 * #", "Dial, or send DTMF during a call"),
    ("Backspace", "Delete the last digit"),
    ("Ctrl+1 … Ctrl+4", "Phone, Contacts, History, Settings"),
    ("Ctrl+K", "Focus the number field (or contact search)"),
    ("Ctrl+M / Ctrl+H", "Mute / hold the current call"),
    ("Ctrl+T", "Transfer the current call"),
    ("Ctrl+Shift+C", "Compact call bar"),
    ("Ctrl+B", "Collapse the sidebar"),
    ("Ctrl+,", "Settings"),
    ("Ctrl+Q", "Quit"),
    ("A / D / S (incoming window)", "Answer / Decline / Silence"),
];

pub fn view<'a>(app: &'a App, t: &Tokens) -> Element<'a, Message> {
    let _ = app;
    let identity = row![
        icons::logo(56.0, t.accent),
        column![
            text(branding::APP_NAME)
                .size(theme::text_size::DISPLAY)
                .font(branding::UI_FONT_SEMIBOLD),
            text(format!("Version {}", env!("CARGO_PKG_VERSION")))
                .size(theme::text_size::BODY)
                .color(muted(t)),
            text("A native Linux SIP softphone with PipeWire audio.").size(theme::text_size::BODY),
        ]
        .spacing(theme::space::XS),
    ]
    .spacing(theme::space::LG)
    .align_y(Alignment::Center);

    let links = row![
        text_button("Project page", theme::button_soft)
            .on_press(Message::OpenUrl("https://github.com/ZeroDay-Labz/OxideSip".into())),
        text_button("Report an issue", theme::button_soft).on_press(Message::OpenUrl(
            "https://github.com/ZeroDay-Labz/OxideSip/issues".into()
        )),
        text_button("Open config folder", theme::button_soft).on_press(Message::OpenConfigFolder),
    ]
    .spacing(theme::space::SM);

    let mut shortcuts = column![].spacing(theme::space::SM);
    for (keys, what) in SHORTCUTS {
        shortcuts = shortcuts.push(
            row![
                container(text(*keys).size(theme::text_size::CAPTION).font(iced::Font::MONOSPACE),)
                    .padding([3, 8])
                    .style(theme::chip)
                    .width(Length::Fixed(230.0)),
                text(*what).size(theme::text_size::BODY),
            ]
            .spacing(theme::space::MD)
            .align_y(Alignment::Center),
        );
    }

    let credits = column![
        text("Built with iced, rsipstack and PipeWire. UI font: Inter (SIL OFL 1.1). Icons: Lucide (ISC).")
            .size(theme::text_size::BODY),
        text("MIT licensed · © ZeroDay-Labz")
            .size(theme::text_size::CAPTION)
            .color(muted(t)),
    ]
    .spacing(theme::space::XS);

    column![
        heading(t, "About", "Version, links, keyboard shortcuts and credits."),
        section("OxideSip", column![identity, links].spacing(theme::space::LG)),
        section("Keyboard shortcuts", shortcuts),
        section("Credits", credits),
    ]
    .spacing(theme::space::LG)
    .into()
}
