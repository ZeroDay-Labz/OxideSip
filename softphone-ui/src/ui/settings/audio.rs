use crate::app::{App, Message};
use crate::icons;
use crate::theme::{self, Tokens};
use crate::ui::components::{self, device_picker, field, muted, picker, section, text_button, toggle_row};
use crate::ui::settings::{actions, heading};
use iced::widget::{column, row, text};
use iced::{Alignment, Element, Length};
use softphone_core::config::DTMF_MODES;

pub fn view<'a>(app: &'a App, t: &Tokens) -> Element<'a, Message> {
    let form = &app.audio_settings_form;
    let devices = column![
        device_picker(
            t,
            "Microphone",
            "System default",
            &app.input_devices,
            &[],
            &form.input_device,
            Message::AudioSettingsInputDeviceChanged,
        ),
        device_picker(
            t,
            "Speaker / headset",
            "System default",
            &app.output_devices,
            &[],
            &form.output_device,
            Message::AudioSettingsOutputDeviceChanged,
        ),
        row![
            text_button("Test ring", theme::button_soft).on_press(Message::TestRingPressed),
            text("Plays the ringtone on the selected output for two seconds. Device changes apply to the next call.")
                .size(theme::text_size::CAPTION)
                .color(muted(t)),
        ]
        .spacing(theme::space::MD)
        .align_y(Alignment::Center),
    ]
    .spacing(theme::space::LG);

    let mut codec_rows = column![].spacing(theme::space::XS);
    let last = form.codecs.len().saturating_sub(1);
    for (index, codec) in form.codecs.iter().enumerate() {
        codec_rows = codec_rows.push(
            row![
                text(format!("{}.", index + 1))
                    .size(theme::text_size::BODY)
                    .color(muted(t)),
                text(codec.to_string()).size(theme::text_size::BODY).width(Length::Fill),
                components::icon_button(
                    icons::chevron_up(14.0, muted(t)),
                    "Higher priority",
                    false,
                    (index > 0).then_some(Message::AudioSettingsCodecMoved(index, true)),
                ),
                components::icon_button(
                    icons::chevron_down(14.0, muted(t)),
                    "Lower priority",
                    false,
                    (index < last).then_some(Message::AudioSettingsCodecMoved(index, false)),
                ),
            ]
            .spacing(theme::space::SM)
            .align_y(Alignment::Center),
        );
    }
    let negotiation = column![
        field(
            t,
            "Codec priority — highest first; the top entry the other side also supports wins",
            codec_rows,
        ),
        toggle_row(
            t,
            "SRTP media encryption",
            "Encrypts call audio (SDES-SRTP). The PBX must support it.",
            form.srtp,
            Message::AudioSettingsSrtpToggled,
        ),
        field(
            t,
            "DTMF — Auto uses RFC 4733 when negotiated and falls back to SIP INFO; change only for interop troubleshooting",
            picker(DTMF_MODES.to_vec(), Some(form.dtmf_mode), Message::AudioSettingsDtmfModeChanged).width(Length::Fill),
        ),
    ]
    .spacing(theme::space::LG);

    column![
        heading(
            t,
            "Audio & codecs",
            "Devices for calls and tones, and how media is negotiated for the selected account."
        ),
        section("Devices", devices),
        section("Negotiation", negotiation),
        actions(
            Message::AudioSettingsSavePressed,
            Message::AudioSettingsCancelPressed,
            "Save"
        ),
    ]
    .spacing(theme::space::LG)
    .into()
}
