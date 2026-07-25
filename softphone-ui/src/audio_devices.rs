//! Chosen PipeWire input/output device, persisted separately from
//! `SipAccountConfig` — this is local hardware state, not a SIP account
//! credential, so it doesn't belong in the same file/struct.

const AUDIO_DEVICES_FILENAME: &str = "audio_devices.toml";

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AudioDeviceConfig {
    pub input_device: Option<String>,
    pub output_device: Option<String>,
}

pub fn load() -> AudioDeviceConfig {
    std::fs::read_to_string(crate::paths::config_file(AUDIO_DEVICES_FILENAME))
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn save(config: &AudioDeviceConfig) -> std::io::Result<()> {
    let text = toml::to_string_pretty(config).unwrap_or_default();
    std::fs::write(crate::paths::config_file(AUDIO_DEVICES_FILENAME), text)
}
