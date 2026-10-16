//! General call-handling preferences (DND, auto-answer, forwarding, deny
//! list, call recording) — global, not tied to any one SIP account, so
//! persisted separately from `accounts.toml` the same way
//! `audio_devices.rs`'s device selection is.

const SETTINGS_FILENAME: &str = "settings.toml";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
    /// Show a desktop notification (with Answer/Decline actions) for each
    /// incoming call, in addition to the always-on-top window.
    #[serde(default = "default_true")]
    pub notify_incoming: bool,
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
    /// PipeWire `node.name` to pull additional audio *from* and mix into
    /// this call's outgoing (RTP-sent) audio — the input counterpart to
    /// `secondary_output_target`. Expected to be another app's own
    /// *playback* stream (e.g. Discord's "what other members are saying",
    /// `Stream/Output/Audio`; see
    /// `softphone_media::devices::list_app_playback_streams`), not a
    /// capture stream — sourcing from a capture stream would loop our own
    /// injected audio back on itself. `None` disables it. Configuring a
    /// target here only sets up the PipeWire stream; whether it's actually
    /// mixed in is controlled per-call by the live toggle next to the line
    /// buttons (`MediaSession::set_secondary_input_enabled`), which starts
    /// off for every new call regardless of this setting.
    #[serde(default)]
    pub secondary_input_target: Option<String>,
    /// Colour scheme: follow the desktop, or force dark/light.
    #[serde(default)]
    pub theme: crate::theme::ThemeSetting,
}

impl Default for AppSettings {
    fn default() -> Self {
        AppSettings {
            dnd: false,
            auto_answer: false,
            forwarding_enabled: false,
            forwarding_number: String::new(),
            deny_list: Vec::new(),
            notify_incoming: true,
            recording_enabled: false,
            recording_path: String::new(),
            secondary_output_target: None,
            secondary_input_target: None,
            theme: crate::theme::ThemeSetting::default(),
        }
    }
}

fn default_true() -> bool {
    true
}

pub fn load() -> AppSettings {
    std::fs::read_to_string(crate::paths::config_file(SETTINGS_FILENAME))
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn save(settings: &AppSettings) -> std::io::Result<()> {
    if !crate::paths::persistence_enabled() {
        return Ok(());
    }
    let text = toml::to_string_pretty(settings).unwrap_or_default();
    std::fs::write(crate::paths::config_file(SETTINGS_FILENAME), text)
}
