//! General call-handling preferences (DND, auto-answer, forwarding, deny
//! list, call recording) — global, not tied to any one SIP account, so
//! persisted separately from `accounts.toml` the same way
//! `audio_devices.rs`'s device selection is.

const SETTINGS_FILENAME: &str = "settings.toml";

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AppSettings {
    /// Auto-reject every incoming call while enabled.
    #[serde(default)]
    pub dnd: bool,
    /// Auto-accept every incoming call while enabled (checked after `dnd`
    /// and the deny list, so a denied number is still rejected even with
    /// auto-answer on).
    #[serde(default)]
    pub auto_answer: bool,
    #[serde(default)]
    pub forwarding_enabled: bool,
    #[serde(default)]
    pub forwarding_number: String,
    /// Numbers/domains checked against an incoming call's caller before it
    /// ever rings — a match rejects the call outright. Plain substring
    /// match against the caller string (SIP `From`), not a strict E.164
    /// comparison, so a partial number or a domain both work as entries.
    #[serde(default)]
    pub deny_list: Vec<String>,
    #[serde(default)]
    pub recording_enabled: bool,
    #[serde(default)]
    pub recording_path: String,
    /// PipeWire `node.name` to also stream the far end's voice to — either
    /// an ordinary sink, or a live app capture stream (e.g. Discord's own
    /// voice-engine node while it's in a voice channel; see
    /// `softphone_media::devices::list_app_capture_streams`), linked to
    /// directly the same way qpwgraph/helvum patch one client straight into
    /// another. `None` disables the secondary output entirely. Applied
    /// when a call's `MediaSession` starts (see `app.rs`'s `MediaReady`
    /// handler), same "takes effect on the next call" timing as the
    /// primary device pickers.
    #[serde(default)]
    pub secondary_output_target: Option<String>,
}

pub fn load() -> AppSettings {
    std::fs::read_to_string(crate::paths::config_file(SETTINGS_FILENAME))
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn save(settings: &AppSettings) -> std::io::Result<()> {
    let text = toml::to_string_pretty(settings).unwrap_or_default();
    std::fs::write(crate::paths::config_file(SETTINGS_FILENAME), text)
}
