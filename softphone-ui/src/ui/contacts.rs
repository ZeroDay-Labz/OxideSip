//! Contacts page and the add/edit modal.

use crate::app::{App, ContactForm, ContactSort, Message};
use crate::contacts::Contact;
use crate::theme::{self, Tokens};
use crate::ui::components::{self, avatar, empty_state, initial, input, muted, text_button, with_tooltip};
use crate::{branding, icons};
use iced::widget::{button, column, container, row, scrollable, text};
use iced::{Alignment, Element, Length};

pub fn view<'a>(app: &'a App, t: &Tokens) -> Element<'a, Message> {
    let filter = app.contact_filter.to_lowercase();
    let search = input("Search by name or number", &app.contact_filter)
        .id(crate::app::SEARCH_INPUT_ID)
        .on_input(Message::ContactFilterChanged)
        .width(Length::Fill);
    let sort_label = match app.contact_sort {
        ContactSort::NameAsc => "A → Z",
        ContactSort::NameDesc => "Z → A",
        ContactSort::Recent => "Recently added",
    };
    let sort = with_tooltip(
        button(
            row![
                icons::arrow_left_right(14.0, muted(t)),
                text(sort_label).size(theme::text_size::BODY)
            ]
            .spacing(theme::space::SM)
            .align_y(Alignment::Center),
        )
        .style(theme::button_soft)
        .padding([theme::space::SM + 1.0, theme::space::MD])
        .on_press(Message::ContactSortToggled),
        "Change sort order",
    );
    let toolbar = row![search, sort].spacing(theme::space::SM).align_y(Alignment::Center);

    let mut matches: Vec<(usize, &Contact)> = app
        .contacts
        .iter()
        .enumerate()
        .filter(|(_, c)| filter.is_empty() || c.name.to_lowercase().contains(&filter) || c.number.contains(&filter))
        .collect();
    match app.contact_sort {
        ContactSort::NameAsc => matches.sort_by_key(|(_, c)| c.name.to_lowercase()),
        ContactSort::NameDesc => {
            matches.sort_by_key(|(_, c)| c.name.to_lowercase());
            matches.reverse();
        }
        ContactSort::Recent => matches.reverse(),
    }

    let body: Element<'a, Message> = if matches.is_empty() {
        if app.contacts.is_empty() {
            empty_state(
                t,
                icons::book_user(40.0, theme::tint(t.text_dim, 0.6)),
                "No contacts yet",
                "Add one with the button above, or import a JSON export below.",
            )
        } else {
            empty_state(t, icons::search(40.0, theme::tint(t.text_dim, 0.6)), "No matches", "")
        }
    } else {
        let mut list = column![].spacing(theme::space::XS);
        for (index, contact) in matches {
            list = list.push(contact_row(t, index, contact));
        }
        scrollable(list.padding(iced::Padding::ZERO.right(theme::space::SM)))
            .style(theme::scrollable_style)
            .height(Length::Fill)
            .into()
    };

    column![
        container(column![toolbar, body].spacing(theme::space::MD).height(Length::Fill))
            .padding(theme::space::LG)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(theme::panel),
        io_row(app, t),
    ]
    .spacing(theme::space::MD)
    .height(Length::Fill)
    .into()
}

fn contact_row<'a>(t: &Tokens, index: usize, contact: &'a Contact) -> Element<'a, Message> {
    let main = button(
        row![
            avatar(&contact.name, initial(&contact.name), 36.0, 0.0),
            column![
                text(contact.name.as_str()).size(theme::text_size::BODY + 1.0),
                text(contact.number.as_str())
                    .size(theme::text_size::CAPTION)
                    .color(muted(t)),
            ]
            .spacing(2)
            .width(Length::Fill),
        ]
        .spacing(theme::space::MD)
        .align_y(Alignment::Center),
    )
    .style(theme::button_ghost)
    .padding([theme::space::SM, theme::space::MD])
    .width(Length::Fill)
    .on_press(Message::DialNumber(contact.number.clone()));

    container(
        row![
            main,
            components::icon_button(
                icons::phone(15.0, t.success),
                "Call",
                false,
                Some(Message::DialNumber(contact.number.clone()))
            ),
            components::icon_button(
                icons::pencil(15.0, muted(t)),
                "Edit",
                false,
                Some(Message::EditContactPressed(index))
            ),
            components::icon_button(
                icons::trash(15.0, t.danger),
                "Delete",
                false,
                Some(Message::DeleteContactPressed(index))
            ),
        ]
        .spacing(theme::space::XS)
        .align_y(Alignment::Center),
    )
    .padding(iced::Padding::ZERO.right(theme::space::XS))
    .style(theme::raised)
    .into()
}

fn io_row<'a>(app: &'a App, t: &Tokens) -> Element<'a, Message> {
    let mut col = column![
        row![
            icons::folder(14.0, muted(t)),
            input("Path to a .json file", &app.contacts_io_path)
                .on_input(Message::ContactsIoPathChanged)
                .width(Length::Fill),
            with_tooltip(
                text_button("Import…", theme::button_soft).on_press(Message::BrowseContactsImportPressed),
                "Pick a JSON file to merge into Contacts",
            ),
            with_tooltip(
                text_button("Export…", theme::button_soft).on_press(Message::BrowseContactsExportPressed),
                "Save all contacts to a JSON file",
            ),
        ]
        .spacing(theme::space::SM)
        .align_y(Alignment::Center),
    ]
    .spacing(theme::space::XS);
    if let Some(status) = &app.contacts_io_status {
        col = col.push(text(status.as_str()).size(theme::text_size::CAPTION).color(muted(t)));
    }
    container(col)
        .padding([theme::space::MD, theme::space::LG])
        .width(Length::Fill)
        .style(theme::panel)
        .into()
}

/// The add/edit contact card (shown inside a modal).
pub fn form_view<'a>(t: &Tokens, form: &'a ContactForm) -> Element<'a, Message> {
    let title = if form.editing_index.is_some() {
        "Edit contact"
    } else {
        "New contact"
    };
    let seed = if form.name.is_empty() { &form.number } else { &form.name };
    column![
        row![
            avatar(seed, initial(seed), 48.0, 0.0),
            text(title)
                .size(theme::text_size::TITLE)
                .font(branding::UI_FONT_SEMIBOLD),
        ]
        .spacing(theme::space::MD)
        .align_y(Alignment::Center),
        components::field(
            t,
            "Name",
            input("Full name", &form.name)
                .id(crate::app::CONTACT_NAME_ID)
                .on_input(Message::ContactNameChanged)
                .on_submit(Message::ContactSavePressed),
        ),
        components::field(
            t,
            "Number",
            input("Extension or phone number", &form.number)
                .on_input(Message::ContactNumberChanged)
                .on_submit(Message::ContactSavePressed),
        ),
        row![
            text_button("Cancel", theme::button_soft).on_press(Message::ContactCancelPressed),
            container(text("")).width(Length::Fill),
            text_button("Save", theme::button_primary).on_press(Message::ContactSavePressed),
        ]
        .spacing(theme::space::SM)
        .align_y(Alignment::Center),
    ]
    .spacing(theme::space::LG)
    .into()
}
