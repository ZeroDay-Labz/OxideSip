//! Chosen PipeWire input/output device, persisted separately from
//! `SipAccountConfig` — this is local hardware state, not a SIP account
//! credential, so it doesn't belong in the same file/struct.

use std::path::Path;

const AUDIO_DEVICES_PATH: &str = "./audio_devices.toml";

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AudioDeviceConfig {
    pub input_device: Option<String>,
    pub output_device: Option<String>,
}

pub fn load() -> AudioDeviceConfig {
    std::fs::read_to_string(Path::new(AUDIO_DEVICES_PATH))
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn save(config: &AudioDeviceConfig) -> std::io::Result<()> {
    let text = toml::to_string_pretty(config).unwrap_or_default();
    std::fs::write(Path::new(AUDIO_DEVICES_PATH), text)
}
