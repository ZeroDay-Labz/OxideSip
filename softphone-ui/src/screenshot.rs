//! `oxidesip --screenshot <dir>`: renders each documentation page with the
//! demo backend and saves it as PNG, then exits. Mirrors how voice-changer
//! produces its README images, so they can be regenerated for every release.

use std::collections::VecDeque;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShotPage {
    Phone,
    PhoneActive,
    Incoming,
    Contacts,
    History,
    SettingsAccounts,
    SettingsAudio,
    SettingsCalls,
    /// The compact call bar (an active call shrunk).
    Compact,
    /// Back to the full window after a compact round trip.
    CompactRestored,
}

impl ShotPage {
    pub const ALL: [ShotPage; 9] = [
        ShotPage::Phone,
        ShotPage::PhoneActive,
        ShotPage::Incoming,
        ShotPage::Contacts,
        ShotPage::History,
        ShotPage::SettingsAccounts,
        ShotPage::SettingsAudio,
        ShotPage::SettingsCalls,
        ShotPage::Compact,
    ];

    pub fn parse(name: &str) -> Option<Vec<ShotPage>> {
        Some(match name {
            "all" => ShotPage::ALL.to_vec(),
            "phone" => vec![ShotPage::Phone],
            "phone-active" | "call" => vec![ShotPage::PhoneActive],
            "incoming" => vec![ShotPage::Incoming],
            "contacts" => vec![ShotPage::Contacts],
            "history" => vec![ShotPage::History],
            "settings" | "settings-accounts" => vec![ShotPage::SettingsAccounts],
            "settings-audio" => vec![ShotPage::SettingsAudio],
            "settings-calls" => vec![ShotPage::SettingsCalls],
            "compact" => vec![ShotPage::Compact],
            "compact-restored" => vec![ShotPage::CompactRestored],
            _ => return None,
        })
    }

    pub fn file_name(self) -> &'static str {
        match self {
            ShotPage::Phone => "phone",
            ShotPage::PhoneActive => "phone-active",
            ShotPage::Incoming => "incoming",
            ShotPage::Contacts => "contacts",
            ShotPage::History => "history",
            ShotPage::SettingsAccounts => "settings-accounts",
            ShotPage::SettingsAudio => "settings-audio",
            ShotPage::SettingsCalls => "settings-calls",
            ShotPage::Compact => "compact",
            ShotPage::CompactRestored => "compact-restored",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptStep {
    ToggleCompact,
}

/// The run's state: pages still to capture and how many ticks the current
/// one has been settling for.
#[derive(Debug)]
pub struct ShotPlan {
    pub dir: PathBuf,
    pub suffix: String,
    pub queue: VecDeque<ShotPage>,
    pub current: Option<ShotPage>,
    pub frames: u32,
    pub settle_frames: u32,
    /// Set while a `window::screenshot` task is in flight.
    pub capturing: bool,
}

impl ShotPlan {
    pub fn new(dir: PathBuf, pages: Vec<ShotPage>, suffix: String, settle_frames: u32) -> Self {
        ShotPlan {
            dir,
            suffix,
            queue: pages.into(),
            current: None,
            frames: 0,
            settle_frames,
            capturing: false,
        }
    }

    /// Extra scripted input for pages that need interaction after staging:
    /// `(tick at which to fire, what to do)`.
    pub fn script(page: ShotPage) -> &'static [(u32, ScriptStep)] {
        match page {
            ShotPage::Compact => &[(30, ScriptStep::ToggleCompact)],
            ShotPage::CompactRestored => &[(30, ScriptStep::ToggleCompact), (55, ScriptStep::ToggleCompact)],
            _ => &[],
        }
    }

    /// Ticks to wait before capturing this page (scripts need longer).
    pub fn settle_frames(page: ShotPage, default: u32) -> u32 {
        let last = Self::script(page).iter().map(|(at, _)| *at).max().unwrap_or(0);
        default.max(last + 25)
    }

    pub fn path_for(&self, page: ShotPage) -> PathBuf {
        self.dir.join(format!("{}{}.png", page.file_name(), self.suffix))
    }
}

pub fn save_png(path: &std::path::Path, shot: &iced::window::Screenshot) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let img = image::RgbaImage::from_raw(shot.size.width, shot.size.height, shot.rgba.to_vec())
        .ok_or_else(|| "screenshot buffer has the wrong size".to_string())?;
    img.save(path).map_err(|e| e.to_string())
}
