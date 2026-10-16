use crate::app::{App, Message};
use crate::theme::{self, Tokens};
use crate::ui::components::{self, field, input, muted, picker, section};
use crate::ui::settings::{actions, heading};
use crate::{branding, icons};
use iced::widget::{button, column, container, row, text};
use iced::{Alignment, Element, Length};
use softphone_core::config::{SIP_TRANSPORTS, SipTransport};

pub fn view<'a>(app: &'a App, t: &Tokens) -> Element<'a, Message> {
    let form = &app.sip_settings_form;

    let mut list = column![].spacing(theme::space::XS);
    for (index, acc) in app.accounts.iter().enumerate() {
        let name = if acc.config.name.trim().is_empty() {
            format!("Account {}", index + 1)
        } else {
            acc.config.name.clone()
        };
        let editing = app.editing_account == Some(index);
        let reg = match &acc.registration {
            crate::app::RegistrationStatus::Registered { .. } => t.success,
            crate::app::RegistrationStatus::Connecting => t.warning,
            _ => t.danger,
        };
        list = list.push(
            row![
                button(
                    row![
                        components::dot(reg, 8.0),
                        column![
                            text(name).size(theme::text_size::BODY).font(if editing {
                                branding::UI_FONT_SEMIBOLD
                            } else {
                                branding::UI_FONT
                            }),
                            text(format!("{}@{}", acc.config.username, acc.config.sip_server_host))
                                .size(theme::text_size::CAPTION)
                                .color(muted(t)),
                        ]
                        .spacing(1)
                        .width(Length::Fill),
                    ]
                    .spacing(theme::space::MD)
                    .align_y(Alignment::Center),
                )
                .style(theme::nav(if editing { 1.0 } else { 0.0 }))
                .padding([theme::space::SM, theme::space::MD])
                .width(Length::Fill)
                .on_press(Message::SelectAccountForEditing(index)),
                components::icon_button(
                    icons::trash(15.0, t.danger),
                    "Remove this account",
                    false,
                    Some(Message::DeleteAccountPressed(index)),
                ),
            ]
            .spacing(theme::space::XS)
            .align_y(Alignment::Center),
        );
    }
    let add = button(
        row![
            icons::plus(14.0, t.text),
            text("Add account").size(theme::text_size::BODY)
        ]
        .spacing(theme::space::SM)
        .align_y(Alignment::Center),
    )
    .style(theme::button_soft)
    .padding([theme::space::SM, theme::space::MD])
    .on_press(Message::AddAccountPressed);
    let accounts_panel = section("Accounts", column![list, add].spacing(theme::space::MD));

    let title = match app.editing_account {
        Some(_) => "Edit account",
        None => "New account",
    };
    let transport_options: Vec<SipTransport> = SIP_TRANSPORTS.to_vec();
    let form_fields = column![
        field(
            t,
            "Display name",
            input("e.g. Office", &form.name).on_input(Message::SipSettingsNameChanged)
        ),
        row![
            field(
                t,
                "SIP server",
                input("pbx.example.com", &form.host).on_input(Message::SipSettingsHostChanged),
            ),
            container(field(
                t,
                "Port",
                input("5060", &form.port).on_input(Message::SipSettingsPortChanged)
            ))
            .width(Length::Fixed(110.0)),
            container(field(
                t,
                "Transport",
                picker(
                    transport_options,
                    Some(form.transport),
                    Message::SipSettingsTransportChanged
                )
                .width(Length::Fill),
            ))
            .width(Length::Fixed(120.0)),
        ]
        .spacing(theme::space::MD),
        row![
            field(
                t,
                "Username / extension",
                input("1001", &form.username).on_input(Message::SipSettingsUsernameChanged),
            ),
            field(
                t,
                "Password",
                input("", &form.password)
                    .on_input(Message::SipSettingsPasswordChanged)
                    .on_submit(Message::SipSettingsSavePressed)
                    .secure(true),
            ),
        ]
        .spacing(theme::space::MD),
        actions(
            Message::SipSettingsSavePressed,
            Message::SipSettingsCancelPressed,
            if app.editing_account.is_some() {
                "Save & re-register"
            } else {
                "Add account"
            },
        ),
    ]
    .spacing(theme::space::LG);

    column![
        heading(
            t,
            "Accounts",
            "Each account registers on its own and has five call lines."
        ),
        accounts_panel,
        section(title, form_fields),
    ]
    .spacing(theme::space::LG)
    .into()
}
