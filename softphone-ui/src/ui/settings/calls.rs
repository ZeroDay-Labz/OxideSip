use crate::app::{App, Message};
use crate::icons;
use crate::theme::{self, Tokens};
use crate::ui::components::{self, device_picker, field, input, muted, section, text_button, toggle_row};
use crate::ui::settings::{actions, heading};
use iced::widget::{column, row, text};
use iced::{Alignment, Element, Length};

pub fn view<'a>(app: &'a App, t: &Tokens) -> Element<'a, Message> {
    let form = &app.settings_form;
    let incoming = column![
        toggle_row(
            t,
            "Do Not Disturb",
            "Decline every incoming call automatically (486 Busy).",
            form.dnd,
            Message::SettingsDndToggled,
        ),
        toggle_row(
            t,
            "Auto-answer",
            "Pick up every incoming call as soon as it arrives.",
            form.auto_answer,
            Message::SettingsAutoAnswerToggled,
        ),
        toggle_row(
            t,
            "Desktop notifications",
            "Also announce calls through the notification center, with Answer and Decline buttons.",
            form.notify_incoming,
            Message::SettingsNotifyToggled,
        ),
    ]
    .spacing(theme::space::LG);

    let mut deny_rows = column![].spacing(theme::space::XS);
    for (index, entry) in form.deny_list.iter().enumerate() {
        deny_rows = deny_rows.push(
            row![
                text(entry.as_str()).size(theme::text_size::BODY).width(Length::Fill),
                components::icon_button(
                    icons::x(14.0, muted(t)),
                    "Remove",
                    false,
                    Some(Message::SettingsDenyListRemovePressed(index)),
                ),
            ]
            .spacing(theme::space::SM)
            .align_y(Alignment::Center),
        );
    }
    let deny_list = column![
        text("Calls from these numbers, names or domains are refused before they ring. A number matches by digits; anything with letters or @ matches the caller's name or SIP address.")
            .size(theme::text_size::CAPTION)
            .color(muted(t)),
        row![
            input("Number, name or domain", &form.deny_list_input)
                .on_input(Message::SettingsDenyListInputChanged)
                .on_submit(Message::SettingsDenyListAddPressed)
                .width(Length::Fill),
            text_button("Add", theme::button_soft).on_press(Message::SettingsDenyListAddPressed),
        ]
        .spacing(theme::space::SM)
        .align_y(Alignment::Center),
        deny_rows,
    ]
    .spacing(theme::space::MD);

    let recording = column![
        toggle_row(
            t,
            "Record calls",
            "Save a WAV file of every call (both sides mixed) to the folder below.",
            form.recording_enabled,
            Message::SettingsRecordingToggled,
        ),
        field(
            t,
            "Save recordings to",
            row![
                input("/home/you/Recordings", &form.recording_path)
                    .on_input(Message::SettingsRecordingPathChanged)
                    .width(Length::Fill),
                text_button("Browse…", theme::button_soft).on_press(Message::BrowseRecordingPathPressed),
            ]
            .spacing(theme::space::SM)
            .align_y(Alignment::Center),
        ),
    ]
    .spacing(theme::space::LG);

    let routing = column![
        row![
            text("Route the call into another app (for example Discord) or out of one.")
                .size(theme::text_size::CAPTION)
                .color(muted(t))
                .width(Length::Fill),
            components::icon_button(
                icons::refresh(14.0, muted(t)),
                "Re-scan devices and live app streams (join the voice channel first)",
                false,
                Some(Message::RefreshSecondaryOutputTargets),
            ),
        ]
        .align_y(Alignment::Center),
        device_picker(
            t,
            "Also send the other party's voice to",
            "Nowhere",
            &app.output_devices,
            &app.app_capture_streams,
            &form.secondary_output_target,
            |id| Message::SettingsSecondaryOutputChanged((!id.is_empty()).then_some(id)),
        ),
        device_picker(
            t,
            "Mix into what the other party hears, from",
            "Nothing",
            &app.input_devices,
            &app.app_playback_streams,
            &form.secondary_input_target,
            |id| Message::SettingsSecondaryInputChanged((!id.is_empty()).then_some(id)),
        ),
        text("The mix-in is switched on per call with the wave button in the Lines card.")
            .size(theme::text_size::CAPTION)
            .color(muted(t)),
    ]
    .spacing(theme::space::LG);

    column![
        heading(
            t,
            "Calls",
            "What happens when a call comes in, and where call audio goes."
        ),
        section("Incoming calls", incoming),
        section("Deny list", deny_list),
        section("Recording", recording),
        section("Secondary audio", routing),
        actions(Message::SettingsSavePressed, Message::SettingsCancelPressed, "Save"),
    ]
    .spacing(theme::space::LG)
    .into()
}
