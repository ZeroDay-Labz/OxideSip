//! Local call history — derived UI state (not a SIP account concern), so it
//! lives here rather than in `softphone-core`, same reasoning as
//! `audio_devices.rs`.

use std::time::{SystemTime, UNIX_EPOCH};

const HISTORY_FILENAME: &str = "call_history.toml";
const MAX_ENTRIES: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CallDirection {
    Incoming,
    Outgoing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CallOutcome {
    Answered,
    Missed,
    Rejected,
    Failed,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HistoryEntry {
    pub number: String,
    /// The caller/callee's display name as the network reported it (SIP
    /// display name), if any. Contacts are matched by `number` at render
    /// time, so this is only the fallback when no contact matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub direction: CallDirection,
    pub outcome: CallOutcome,
    pub unix_secs: i64,
    pub duration_secs: u32,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct HistoryFile {
    #[serde(default)]
    entries: Vec<HistoryEntry>,
}

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn load() -> Vec<HistoryEntry> {
    std::fs::read_to_string(crate::paths::config_file(HISTORY_FILENAME))
        .ok()
        .and_then(|text| toml::from_str::<HistoryFile>(&text).ok())
        .map(|f| f.entries)
        .unwrap_or_default()
        .into_iter()
        .map(repair_legacy_entry)
        .collect()
}

/// Versions before 0.2.0 stored the raw SIP `From` header as the number for
/// incoming calls (`From: "Alice" <sip:1001@pbx>;tag=…`). Split such entries
/// into a proper number + name once, on load; the next save writes the
/// repaired form.
fn repair_legacy_entry(mut entry: HistoryEntry) -> HistoryEntry {
    let looks_raw =
        entry.number.starts_with("From:") || entry.number.contains("<sip:") || entry.number.contains("sip:");
    if looks_raw {
        let party = softphone_core::events::RemoteParty::from_header_value(&entry.number);
        entry.number = party.user;
        if entry.name.is_none() {
            entry.name = party.display_name;
        }
    }
    entry
}

/// Persists `entries`, keeping only the most recent `MAX_ENTRIES` (oldest
/// dropped first) so the file doesn't grow unbounded. `entries` is expected
/// oldest-first (append-on-call-end order).
pub fn save(entries: &[HistoryEntry]) {
    if !crate::paths::persistence_enabled() {
        return;
    }
    let start = entries.len().saturating_sub(MAX_ENTRIES);
    let file = HistoryFile {
        entries: entries[start..].to_vec(),
    };
    if let Ok(text) = toml::to_string_pretty(&file) {
        let _ = std::fs::write(crate::paths::config_file(HISTORY_FILENAME), text);
    }
}

/// A short "3m ago" / "2h ago" style label. Deliberately relative rather
/// than a wall-clock date/time string: correctly rendering a local
/// wall-clock timestamp needs a timezone-database crate, and the common
/// options for reading the local offset on Linux are either unsound in a
/// multi-threaded process (the `time` crate's `local-offset` feature) or
/// pull in a much heavier dependency than a call log needs — a relative
/// label sidesteps the whole problem.
pub fn relative_label(now: i64, unix_secs: i64) -> String {
    let diff = (now - unix_secs).max(0);
    if diff < 60 {
        "just now".to_string()
    } else if diff < 3600 {
        format!("{}m ago", diff / 60)
    } else if diff < 86_400 {
        format!("{}h ago", diff / 3600)
    } else if diff < 30 * 86_400 {
        format!("{}d ago", diff / 86_400)
    } else {
        format!("{}w ago", diff / (7 * 86_400))
    }
}

pub fn duration_label(duration_secs: u32) -> String {
    format!("{:02}:{:02}", duration_secs / 60, duration_secs % 60)
}
