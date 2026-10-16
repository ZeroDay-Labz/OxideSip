//! Remembered window geometry and navigation — `ui_state.toml` next to the
//! other config files. Nothing here affects calls; losing it is harmless.

use crate::app::{HistoryFilter, Page, SettingsSection};

const FILENAME: &str = "ui_state.toml";

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct UiState {
    /// `[x, y, width, height]` of the main window in logical pixels; `x`/`y`
    /// are NaN-free only when the compositor reported a position (Wayland
    /// never does, so they stay unset there and only the size is reused).
    #[serde(default)]
    pub window_size: Option<[f32; 2]>,
    #[serde(default)]
    pub window_position: Option<[f32; 2]>,
    #[serde(default)]
    pub page: Page,
    #[serde(default)]
    pub settings_section: SettingsSection,
    #[serde(default)]
    pub history_filter: HistoryFilter,
    #[serde(default)]
    pub sidebar_collapsed: bool,
    #[serde(default)]
    pub contacts_io_path: Option<String>,
}

pub fn load() -> UiState {
    std::fs::read_to_string(crate::paths::config_file(FILENAME))
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn save(state: &UiState) -> std::io::Result<()> {
    if !crate::paths::persistence_enabled() {
        return Ok(());
    }
    let text = toml::to_string_pretty(state).map_err(std::io::Error::other)?;
    std::fs::write(crate::paths::config_file(FILENAME), text)
}
