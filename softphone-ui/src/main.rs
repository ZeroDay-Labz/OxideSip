mod app;
mod app_settings;
mod audio_devices;
mod bridge;
mod contacts;
mod history;
mod icon;
mod paths;
mod theme;
mod view;

use app::App;
use std::path::Path;

const ACCOUNTS_FILENAME: &str = "accounts.toml";
/// Only ever checked relative to the CWD — genuinely legacy, from before
/// `accounts.toml` existed at all. `paths::config_file` already handles
/// migrating an existing CWD-relative `accounts.toml` into the real XDG
/// location; this only matters for the (now rare) case of a dev setup that
/// predates `accounts.toml` entirely.
const LEGACY_CONFIG_PATH: &str = "./oxidesip.toml";

/// Loads every configured account. If `accounts.toml` doesn't exist yet but
/// a legacy single-account `oxidesip.toml` (or `OXIDESIP_*` env vars) does,
/// migrates it into a one-entry accounts file so existing single-account
/// setups keep working without the user having to reconfigure anything.
fn load_accounts() -> Vec<softphone_core::config::SipAccountConfig> {
    let accounts_path = paths::config_file(ACCOUNTS_FILENAME);
    let accounts = softphone_core::config::load_accounts(&accounts_path).unwrap_or_default();
    if !accounts.is_empty() {
        return accounts;
    }

    let legacy = softphone_core::config::load_config_from_env()
        .or_else(|_| softphone_core::config::load_config(Path::new(LEGACY_CONFIG_PATH)))
        .ok();
    let Some(mut legacy) = legacy else {
        return Vec::new();
    };
    if legacy.name.trim().is_empty() {
        legacy.name = "Default".to_string();
    }
    let migrated = vec![legacy];
    if let Err(e) = softphone_core::config::save_accounts(&accounts_path, &migrated) {
        tracing::warn!(%e, "failed to persist migrated account");
    }
    migrated
}

pub fn main() -> iced::Result {
    tracing_subscriber::fmt::init();

    let accounts = load_accounts();

    iced::daemon(move || App::boot(accounts.clone()), App::update, App::view)
        .title(App::title)
        .subscription(App::subscription)
        .theme(App::theme)
        .run()
}
