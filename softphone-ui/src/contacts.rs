//! Local phone book — same local-file persistence pattern as
//! `audio_devices.rs`/`history.rs`.

use std::io;
use std::path::Path;

const CONTACTS_FILENAME: &str = "contacts.toml";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Contact {
    pub name: String,
    pub number: String,
}

/// Import-only shape, deliberately looser than `Contact`: accepts either a
/// plain `name`, or MicroSIP-style `firstname`/`lastname`, and either
/// `number`, `mobile`, or `phone` — so exports from other softphones import
/// without the user having to hand-edit the file first. Export always
/// writes plain `Contact`s (name + number), so round-tripping our own
/// exports is always lossless.
#[derive(Debug, Default, serde::Deserialize)]
struct ImportContact {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    firstname: Option<String>,
    #[serde(default)]
    lastname: Option<String>,
    #[serde(default)]
    number: Option<String>,
    #[serde(default)]
    mobile: Option<String>,
    #[serde(default)]
    phone: Option<String>,
}

impl ImportContact {
    fn into_contact(self) -> Option<Contact> {
        let name = self.name.filter(|s| !s.trim().is_empty()).or_else(|| {
            let combined = [self.firstname, self.lastname]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ");
            let combined = combined.trim().to_string();
            (!combined.is_empty()).then_some(combined)
        })?;
        let number = self
            .number
            .or(self.mobile)
            .or(self.phone)
            .filter(|s| !s.trim().is_empty())?;
        Some(Contact { name, number })
    }
}

/// Writes every contact as a JSON array of `{name, number}` objects.
pub fn export_json(contacts: &[Contact], path: &Path) -> io::Result<()> {
    let text = serde_json::to_string_pretty(contacts).map_err(io::Error::other)?;
    std::fs::write(path, text)
}

/// Reads a JSON array of contact-like objects (see `ImportContact`).
/// Entries missing both a usable name and number are silently skipped
/// rather than failing the whole import.
pub fn import_json(path: &Path) -> io::Result<Vec<Contact>> {
    let text = std::fs::read_to_string(path)?;
    let entries: Vec<ImportContact> = serde_json::from_str(&text).map_err(io::Error::other)?;
    Ok(entries.into_iter().filter_map(ImportContact::into_contact).collect())
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct ContactsFile {
    #[serde(default)]
    contacts: Vec<Contact>,
}

pub fn load() -> Vec<Contact> {
    std::fs::read_to_string(crate::paths::config_file(CONTACTS_FILENAME))
        .ok()
        .and_then(|text| toml::from_str::<ContactsFile>(&text).ok())
        .map(|f| f.contacts)
        .unwrap_or_default()
}

pub fn save(contacts: &[Contact]) {
    let mut sorted = contacts.to_vec();
    sorted.sort_by_key(|c| c.name.to_lowercase());
    let file = ContactsFile { contacts: sorted };
    if let Ok(text) = toml::to_string_pretty(&file) {
        let _ = std::fs::write(crate::paths::config_file(CONTACTS_FILENAME), text);
    }
}
