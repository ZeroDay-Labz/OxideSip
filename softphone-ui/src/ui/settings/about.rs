use crate::app::{App, Message};
use crate::theme::{self, Tokens};
use crate::ui::components::{muted, section, text_button, toggle_row};
use crate::ui::settings::heading;
use crate::updater::{self, Stage, UpdateState};
use crate::{branding, icons};
use iced::widget::{column, container, progress_bar, row, text};
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
        section("Updates", updates(app, t)),
        section("Keyboard shortcuts", shortcuts),
        section("Credits", credits),
    ]
    .spacing(theme::space::LG)
    .into()
}

/// Status line, action button and the startup-check toggle.
fn updates<'a>(app: &'a App, t: &Tokens) -> Element<'a, Message> {
    let kind = &app.install_kind;
    let line = |message: String, color: iced::Color| text(message).size(theme::text_size::BODY).color(color);
    let check = || text_button("Check for updates", theme::button_soft).on_press(Message::UpdateCheckPressed);

    let mut status = column![].spacing(theme::space::SM);
    let mut buttons = row![].spacing(theme::space::SM).align_y(Alignment::Center);
    match &app.update {
        UpdateState::Idle => {
            status = status.push(line(format!("Installed as a {}.", kind.label()), muted(t)));
            buttons = buttons.push(check());
        }
        UpdateState::Checking => {
            status = status.push(line("Checking GitHub for a newer release…".into(), muted(t)));
            buttons = buttons.push(text_button("Check for updates", theme::button_soft));
        }
        UpdateState::UpToDate => {
            status = status.push(line(
                format!("You have the latest version ({}).", env!("CARGO_PKG_VERSION")),
                t.success,
            ));
            buttons = buttons.push(check());
        }
        UpdateState::Available(release) => {
            status = status.push(line(
                format!(
                    "Version {} is available (you have {}).",
                    release.version(),
                    env!("CARGO_PKG_VERSION")
                ),
                t.text,
            ));
            if kind.can_self_update() {
                let how = match kind {
                    updater::InstallKind::Tarball(_) => "The program file is replaced in place.",
                    _ => {
                        "The download is checked against its published checksum, then your system asks for your password to install it."
                    }
                };
                status = status.push(text(how).size(theme::text_size::CAPTION).color(muted(t)));
                buttons = buttons
                    .push(text_button("Update now", theme::button_primary).on_press(Message::UpdateInstallPressed));
            } else {
                let how = match kind {
                    updater::InstallKind::Flatpak => {
                        "A Flatpak cannot replace itself from inside its sandbox; download the new bundle and install it with `flatpak install`."
                    }
                    _ => "This copy was not installed from a release package, so it is not updated automatically.",
                };
                status = status.push(text(how).size(theme::text_size::CAPTION).color(muted(t)));
                buttons = buttons.push(
                    text_button("Open download page", theme::button_primary).on_press(Message::UpdateInstallPressed),
                );
            }
            buttons = buttons
                .push(text_button("What's new", theme::button_soft).on_press(Message::OpenUrl(release.page.clone())));
        }
        UpdateState::Working(release, progress) => {
            let (message, fraction) = match progress.stage() {
                Stage::Downloading => (
                    match progress.fraction() {
                        Some(f) => format!("Downloading {} — {:.0}%", release.version(), f * 100.0),
                        None => format!("Downloading {}…", release.version()),
                    },
                    progress.fraction().unwrap_or(0.0),
                ),
                Stage::Verifying => ("Checking the download…".to_string(), 1.0),
                Stage::Installing => (
                    "Installing — answer your system's password prompt if one appears.".to_string(),
                    1.0,
                ),
            };
            status = status.push(line(message, t.text));
            status = status.push(container(progress_bar(0.0..=1.0, fraction).girth(6)).width(Length::Fixed(320.0)));
        }
        UpdateState::Installed(version) => {
            status = status.push(line(
                format!("Version {version} is installed. Restart OxideSip to start using it."),
                t.success,
            ));
            buttons =
                buttons.push(text_button("Restart now", theme::button_primary).on_press(Message::UpdateRestartPressed));
        }
        UpdateState::Failed(error) => {
            status = status.push(line(format!("Update failed: {error}"), t.danger));
            buttons = buttons.push(check()).push(
                text_button("Open download page", theme::button_soft)
                    .on_press(Message::OpenUrl(updater::RELEASES_PAGE.into())),
            );
        }
    }

    column![
        status,
        buttons,
        toggle_row(
            t,
            "Check for updates when OxideSip starts",
            "Asks GitHub once per start whether a newer release exists. Nothing is downloaded or installed until you choose to.",
            app.settings.check_updates,
            Message::UpdateAutoCheckToggled,
        ),
    ]
    .spacing(theme::space::LG)
    .into()
}
