mod app;
mod app_settings;
mod audio_devices;
mod branding;
mod bridge;
mod contacts;
mod demo;
mod history;
mod icons;
mod incoming;
mod launcher;
mod notify;
mod number;
mod paths;
mod screenshot;
mod theme;
mod toast;
mod ui;
mod ui_state;

use app::{App, BootOptions};
use clap::Parser;
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

/// OxideSip — a native Linux SIP softphone.
#[derive(Parser, Debug)]
#[command(name = "oxidesip", version, about)]
struct Cli {
    /// Run against a scripted demo backend: seeded accounts, contacts and
    /// history, no SIP server or audio, and nothing written to disk.
    #[arg(long)]
    demo: bool,
    /// Render documentation screenshots into DIR and exit (implies --demo).
    #[arg(long, value_name = "DIR")]
    screenshot: Option<std::path::PathBuf>,
    /// Page(s) to capture: all, phone, phone-active, incoming, contacts,
    /// history, settings-accounts, settings-audio, settings-calls.
    #[arg(long, default_value = "all")]
    page: String,
    /// Initial window size, e.g. 1200x800.
    #[arg(long, value_name = "WxH")]
    size: Option<String>,
    /// Force a colour scheme for this run: dark, light or system.
    #[arg(long)]
    theme: Option<String>,
    /// Ticks (80 ms each) to let a page settle before capturing it.
    #[arg(long, default_value_t = 40)]
    frames: u32,
    /// A sip: or tel: URI (or plain number) to pre-fill the dial field.
    uri: Option<String>,
}

pub fn main() -> iced::Result {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    let demo = cli.demo || cli.screenshot.is_some();
    if demo {
        paths::set_persistence(false);
    }
    let accounts = if demo { demo::accounts() } else { load_accounts() };
    if !demo {
        launcher::ensure_installed();
    }

    let theme = cli.theme.as_deref().and_then(|t| match t {
        "dark" => Some(theme::ThemeSetting::Dark),
        "light" => Some(theme::ThemeSetting::Light),
        "system" => Some(theme::ThemeSetting::System),
        other => {
            eprintln!("unknown --theme {other:?}; expected dark, light or system");
            None
        }
    });
    let size = cli.size.as_deref().and_then(|s| {
        let (w, h) = s.split_once('x')?;
        Some(iced::Size::new(w.parse().ok()?, h.parse().ok()?))
    });
    // iced renders an offscreen screenshot reliably only once per process
    // (later captures come back as stale frames without text), so a request
    // for several pages re-runs this executable once per page.
    if let Some(dir) = &cli.screenshot {
        let pages = screenshot::ShotPage::parse(&cli.page).unwrap_or_else(|| screenshot::ShotPage::ALL.to_vec());
        if pages.len() > 1 {
            let exe = std::env::current_exe().expect("current executable path");
            let mut failures = 0;
            for page in pages {
                let mut cmd = std::process::Command::new(&exe);
                cmd.arg("--screenshot").arg(dir).arg("--page").arg(page.file_name());
                cmd.arg("--frames").arg(cli.frames.to_string());
                if let Some(size) = &cli.size {
                    cmd.arg("--size").arg(size);
                }
                if let Some(theme) = &cli.theme {
                    cmd.arg("--theme").arg(theme);
                }
                match cmd.status() {
                    Ok(status) if status.success() => {}
                    Ok(status) => {
                        eprintln!("screenshot {} failed: {status}", page.file_name());
                        failures += 1;
                    }
                    Err(e) => {
                        eprintln!("could not run {}: {e}", exe.display());
                        failures += 1;
                    }
                }
            }
            std::process::exit(if failures == 0 { 0 } else { 1 });
        }
    }

    let screenshot = cli.screenshot.as_ref().map(|dir| {
        let pages = screenshot::ShotPage::parse(&cli.page).unwrap_or_else(|| {
            eprintln!("unknown --page {:?}; capturing all pages", cli.page);
            screenshot::ShotPage::ALL.to_vec()
        });
        let suffix = match theme {
            Some(theme::ThemeSetting::Light) => "-light".to_string(),
            _ => String::new(),
        };
        screenshot::ShotPlan::new(dir.clone(), pages, suffix, cli.frames.max(5))
    });
    let dial = cli.uri.as_deref().map(number::normalize).filter(|n| !n.is_empty());
    let options = BootOptions {
        demo,
        screenshot,
        theme,
        size: size.or_else(|| cli.screenshot.is_some().then_some(iced::Size::new(1200.0, 800.0))),
        dial,
    };

    // `iced::daemon` needs a `Fn` boot closure; the options are consumed by
    // the single boot that actually happens.
    let options = std::sync::Mutex::new(Some(options));
    iced::daemon(
        move || App::boot(accounts.clone(), options.lock().unwrap().take().unwrap_or_default()),
        App::update,
        App::view,
    )
    .title(App::title)
    .subscription(App::subscription)
    .theme(App::theme)
    // Bundled UI font so every desktop renders identically (Inter, OFL).
    .font(include_bytes!("../assets/fonts/Inter-Regular.ttf").as_slice())
    .font(include_bytes!("../assets/fonts/Inter-Medium.ttf").as_slice())
    .font(include_bytes!("../assets/fonts/Inter-SemiBold.ttf").as_slice())
    .default_font(branding::UI_FONT)
    .run()
}
