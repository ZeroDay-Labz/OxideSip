//! Where every `*.toml`/`*.json` persistence file actually lives on disk.
//!
//! Every other module used to hardcode a `./foo.toml`-style path, resolved
//! against the process's current working directory. That's fine for
//! `cargo run` from a repo checkout (CWD is always the repo root), but an
//! installed binary (RPM, tarball, or a desktop launcher entry) has no such
//! guarantee — CWD is typically `$HOME` or `/`, often not writable, and not
//! necessarily the same directory from one launch to the next. The
//! practical symptom was every setting (most visibly, SIP credentials)
//! silently failing to persist once installed, since the write just went to
//! a non-writable or throwaway location.
//!
//! `config_file` resolves a stable, XDG-correct location instead
//! (`$XDG_CONFIG_HOME/oxidesip/`, falling back to `~/.config/oxidesip/`),
//! and transparently migrates a pre-existing CWD-relative file the first
//! time it's asked for — so a dev checkout's existing `./accounts.toml` (or
//! anyone's from before this fix) isn't silently orphaned.

use std::path::PathBuf;

fn config_dir() -> PathBuf {
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("oxidesip");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// The real path to use for `filename` (e.g. `"accounts.toml"`): under the
/// XDG config directory, migrating a same-named file from the current
/// working directory into place on first use if the XDG path doesn't exist
/// yet but a CWD-relative one does.
pub fn config_file(filename: &str) -> PathBuf {
    let new_path = config_dir().join(filename);
    if !new_path.exists() {
        let old_path = PathBuf::from(".").join(filename);
        if old_path.exists()
            && let Ok(contents) = std::fs::read(&old_path)
        {
            let _ = std::fs::write(&new_path, contents);
        }
    }
    new_path
}
