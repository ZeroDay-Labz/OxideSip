//! Makes sure the desktop knows about us when no package installed the
//! launcher: on Wayland the task-bar / alt-tab icon comes *only* from a
//! `.desktop` file whose name matches the window's app id, so a `cargo run`
//! or tarball binary would otherwise show a blank icon. Writes the entry
//! and the icons into `~/.local/share` once (and rewrites the entry when the
//! executable has moved, e.g. a new build directory). Never runs in demo
//! mode and never touches anything a system package put under `/usr`.

use crate::branding::APP_ID;
use std::path::{Path, PathBuf};

const ICONS: &[(u32, &[u8])] = &[
    (16, include_bytes!("../../packaging/icons/16.png")),
    (32, include_bytes!("../../packaging/icons/32.png")),
    (48, include_bytes!("../../packaging/icons/48.png")),
    (64, include_bytes!("../../packaging/icons/64.png")),
    (128, include_bytes!("../../packaging/icons/128.png")),
    (256, include_bytes!("../../packaging/icons/256.png")),
    (512, include_bytes!("../../packaging/icons/512.png")),
];
const ICON_SVG: &[u8] = include_bytes!("../../packaging/io.github.zerodaylabz.OxideSip.svg");

/// Installs the icons for this user (always — a user-level hicolor entry
/// overrides whatever an older package put under `/usr/share`, so a running
/// build shows *its* icon), and the launcher entry only when no system or
/// Flatpak package provides one.
pub fn ensure_installed() {
    let Some(data_home) = dirs::data_dir() else {
        return;
    };
    let desktop_name = format!("{APP_ID}.desktop");
    let mut changed = false;

    for (size, png) in ICONS {
        let path = data_home
            .join("icons/hicolor")
            .join(format!("{size}x{size}/apps"))
            .join(format!("{APP_ID}.png"));
        changed |= write_if_different(&path, png);
    }
    let svg_path = data_home
        .join("icons/hicolor/scalable/apps")
        .join(format!("{APP_ID}.svg"));
    changed |= write_if_different(&svg_path, ICON_SVG);

    // A system or Flatpak package already ships the entry: leave it alone.
    let system_dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    let packaged = system_dirs
        .split(':')
        .filter(|d| !d.is_empty())
        .any(|d| Path::new(d).join("applications").join(&desktop_name).exists());
    if !packaged && let Ok(exe) = std::env::current_exe() {
        let exe = exe.to_string_lossy().into_owned();
        let entry = format!(
            "[Desktop Entry]\nType=Application\nName=OxideSip\nGenericName=SIP Softphone\n\
             Comment=Native Linux SIP softphone with PipeWire audio\nExec={exe} %u\nIcon={APP_ID}\n\
             Terminal=false\nCategories=Network;Telephony;\n\
             Keywords=sip;voip;phone;softphone;pbx;asterisk;freepbx;call;\n\
             MimeType=x-scheme-handler/sip;x-scheme-handler/tel;\nStartupNotify=true\nStartupWMClass={APP_ID}\n\
             X-OxideSip-Managed=true\n"
        );
        let desktop_path = data_home.join("applications").join(&desktop_name);
        let current = std::fs::read_to_string(&desktop_path).unwrap_or_default();
        // Hand-edited by the user? Don't fight them.
        if current.is_empty() || current.contains("X-OxideSip-Managed=true") {
            changed |= write_if_different(&desktop_path, entry.as_bytes());
        }
    }
    if changed {
        tracing::info!(dir = %data_home.display(), "installed launcher icons for this user");
        refresh_caches(data_home);
    }
}

fn write_if_different(path: &PathBuf, bytes: &[u8]) -> bool {
    if std::fs::read(path).is_ok_and(|existing| existing == bytes) {
        return false;
    }
    write(path, bytes)
}

fn write(path: &PathBuf, bytes: &[u8]) -> bool {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(path, bytes) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(%e, path = %path.display(), "could not install launcher file");
            false
        }
    }
}

/// Best effort, in the background: tell KDE / the icon cache about the new
/// files so the running window picks up its icon without a re-login.
fn refresh_caches(data_home: PathBuf) {
    std::thread::spawn(move || {
        let quiet = |mut cmd: std::process::Command| {
            let _ = cmd
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        };
        let mut udd = std::process::Command::new("update-desktop-database");
        udd.arg(data_home.join("applications"));
        quiet(udd);
        for sycoca in ["kbuildsycoca6", "kbuildsycoca5"] {
            if std::process::Command::new(sycoca)
                .arg("--noincremental")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok()
            {
                break;
            }
        }
    });
}
