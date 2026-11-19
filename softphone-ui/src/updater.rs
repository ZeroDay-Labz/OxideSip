//! In-app updater: asks GitHub for the latest release, downloads the
//! package matching how this copy was installed, checks it against the
//! release's `SHA256SUMS`, and installs it.
//!
//! What "install" means depends on where the running binary came from:
//!
//! * RPM / deb — the package manager is run through `pkexec`, so the desktop
//!   shows its usual authorisation prompt; nothing is installed without it.
//! * tarball — the binary is replaced in place (it must be writable).
//! * Flatpak and development builds — not updated from inside the app; the
//!   release page is opened instead.
//!
//! Trust is HTTPS to GitHub: the checksum file comes from the same release
//! as the package, so it catches a damaged or truncated download, not a
//! compromised release.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::Duration;

const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/ZeroDay-Labz/OxideSip/releases/latest";
/// Every download must come from this repository's release assets.
const DOWNLOAD_PREFIX: &str = "https://github.com/ZeroDay-Labz/OxideSip/releases/download/";
pub const RELEASES_PAGE: &str = "https://github.com/ZeroDay-Labz/OxideSip/releases/latest";
const CHECKSUMS_ASSET: &str = "SHA256SUMS";
const TARBALL_ASSET: &str = "oxidesip-linux-x86_64.tar.gz";
const USER_AGENT: &str = concat!("oxidesip-updater/", env!("CARGO_PKG_VERSION"));
/// Far above any real package; stops a bogus response filling the disk.
const MAX_DOWNLOAD_BYTES: u64 = 300 * 1024 * 1024;

/// How the running copy was installed, which decides how it is updated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallKind {
    Rpm,
    Deb,
    /// A plain binary at this path that we are allowed to replace.
    Tarball(PathBuf),
    Flatpak,
    /// A `cargo` build, a read-only location, or an architecture with no
    /// published package.
    Unmanaged,
}

impl InstallKind {
    pub fn detect() -> Self {
        if Path::new("/.flatpak-info").exists() || std::env::var_os("FLATPAK_ID").is_some() {
            return InstallKind::Flatpak;
        }
        if std::env::consts::ARCH != "x86_64" {
            return InstallKind::Unmanaged;
        }
        let Ok(exe) = std::env::current_exe().and_then(|p| p.canonicalize()) else {
            return InstallKind::Unmanaged;
        };
        let owned_by = |program: &str, flag: &str| {
            std::process::Command::new(program)
                .arg(flag)
                .arg(&exe)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        if owned_by("rpm", "-qf") {
            return InstallKind::Rpm;
        }
        if owned_by("dpkg", "-S") {
            return InstallKind::Deb;
        }
        let text = exe.to_string_lossy();
        if text.contains("/target/debug/") || text.contains("/target/release/") {
            return InstallKind::Unmanaged;
        }
        let writable = exe
            .parent()
            .and_then(|dir| std::fs::metadata(dir).ok())
            .is_some_and(|meta| !meta.permissions().readonly())
            && std::fs::OpenOptions::new().append(true).open(&exe).is_ok();
        if writable {
            InstallKind::Tarball(exe)
        } else {
            InstallKind::Unmanaged
        }
    }

    /// Whether "Update now" can do the whole job from inside the app.
    pub fn can_self_update(&self) -> bool {
        matches!(self, InstallKind::Rpm | InstallKind::Deb | InstallKind::Tarball(_))
    }

    pub fn label(&self) -> &'static str {
        match self {
            InstallKind::Rpm => "RPM package",
            InstallKind::Deb => "Debian package",
            InstallKind::Tarball(_) => "standalone binary",
            InstallKind::Flatpak => "Flatpak",
            InstallKind::Unmanaged => "development or read-only build",
        }
    }

    fn asset<'a>(&self, release: &'a Release) -> Option<&'a Asset> {
        let pick = |test: &dyn Fn(&str) -> bool| release.assets.iter().find(|a| test(&a.name));
        match self {
            InstallKind::Rpm => pick(&|n| n.ends_with(".x86_64.rpm")),
            InstallKind::Deb => pick(&|n| n.ends_with("_amd64.deb")),
            InstallKind::Tarball(_) => pick(&|n| n == TARBALL_ASSET),
            InstallKind::Flatpak | InstallKind::Unmanaged => None,
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Asset {
    pub name: String,
    #[serde(rename = "browser_download_url")]
    pub url: String,
    #[serde(default)]
    pub size: u64,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Release {
    #[serde(rename = "tag_name")]
    pub tag: String,
    #[serde(rename = "html_url")]
    pub page: String,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

impl Release {
    /// The tag without its leading `v`.
    pub fn version(&self) -> &str {
        self.tag.trim_start_matches('v')
    }
}

/// Download progress, shared with the UI (which redraws on its own tick).
#[derive(Debug, Default)]
pub struct Progress {
    pub received: AtomicU64,
    pub total: AtomicU64,
    stage: AtomicU8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Downloading,
    Verifying,
    Installing,
}

impl Progress {
    pub fn stage(&self) -> Stage {
        match self.stage.load(Ordering::Relaxed) {
            0 => Stage::Downloading,
            1 => Stage::Verifying,
            _ => Stage::Installing,
        }
    }

    fn set_stage(&self, stage: Stage) {
        self.stage.store(stage as u8, Ordering::Relaxed);
    }

    /// 0.0–1.0, or `None` while the size is unknown.
    pub fn fraction(&self) -> Option<f32> {
        let total = self.total.load(Ordering::Relaxed);
        (total > 0).then(|| (self.received.load(Ordering::Relaxed) as f32 / total as f32).min(1.0))
    }
}

/// What the About page shows.
#[derive(Debug, Clone, Default)]
pub enum UpdateState {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Working(Release, Arc<Progress>),
    /// Installed; the new version runs after a restart.
    Installed(String),
    Failed(String),
}

/// Numeric dotted version, ignoring a leading `v` and any `-suffix`.
fn parse_version(text: &str) -> Option<Vec<u64>> {
    let core = text.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next()?;
    let parts: Option<Vec<u64>> = core.split('.').map(|p| p.parse().ok()).collect();
    parts.filter(|p| !p.is_empty())
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(mut a), Some(mut b)) => {
            let len = a.len().max(b.len());
            a.resize(len, 0);
            b.resize(len, 0);
            a > b
        }
        _ => false,
    }
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .https_only(true)
        .connect_timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| format!("could not set up the connection: {e}"))
}

fn describe(e: reqwest::Error) -> String {
    if e.is_connect() || e.is_timeout() {
        "could not reach GitHub — check the network connection".to_string()
    } else {
        format!("request to GitHub failed: {}", e.without_url())
    }
}

/// The latest published release, whatever its version.
pub async fn fetch_latest() -> Result<Release, String> {
    let response = client()?
        .get(LATEST_RELEASE_URL)
        .header("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(describe)?;
    let status = response.status();
    if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err("GitHub is rate-limiting this network; try again in a while".to_string());
    }
    if !status.is_success() {
        return Err(format!("GitHub answered {status}"));
    }
    let body = response.bytes().await.map_err(describe)?;
    serde_json::from_slice(&body).map_err(|e| format!("unexpected answer from GitHub: {e}"))
}

/// `Some(release)` when a version newer than the running one is published.
pub async fn check() -> Result<Option<Release>, String> {
    let release = fetch_latest().await?;
    Ok(is_newer(&release.tag, env!("CARGO_PKG_VERSION")).then_some(release))
}

/// The hex digest listed for `name` in a `sha256sum`-format file.
fn checksum_for(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hash, file) = line.split_once(char::is_whitespace)?;
        let file = file.trim().trim_start_matches('*');
        (file == name && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| hash.to_ascii_lowercase())
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn updates_dir() -> Result<PathBuf, String> {
    let dir = dirs::cache_dir()
        .ok_or("no cache directory to download into")?
        .join("oxidesip")
        .join("updates");
    // Start clean: nothing from an earlier attempt is ever installed.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Downloads `asset` into `dir` and returns its path once its SHA-256
/// matches the one the release's `SHA256SUMS` lists for it.
pub async fn download_verified(
    release: &Release,
    asset: &Asset,
    dir: &Path,
    progress: &Progress,
) -> Result<PathBuf, String> {
    use tokio::io::AsyncWriteExt;

    let sums_asset = release
        .assets
        .iter()
        .find(|a| a.name == CHECKSUMS_ASSET)
        .ok_or("this release has no SHA256SUMS file, so the download cannot be verified")?;
    for a in [asset, sums_asset] {
        if !a.url.starts_with(DOWNLOAD_PREFIX) {
            return Err(format!("refusing to download from an unexpected address: {}", a.url));
        }
    }
    // Asset names come from the network; never let one pick the directory.
    let file_name = Path::new(&asset.name)
        .file_name()
        .filter(|n| *n == asset.name.as_str())
        .ok_or("the release lists a file with an unusable name")?;

    let client = client()?;
    let sums = client
        .get(&sums_asset.url)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(describe)?
        .text()
        .await
        .map_err(describe)?;
    let expected = checksum_for(&sums, &asset.name)
        .ok_or_else(|| format!("SHA256SUMS does not list {}, so it cannot be verified", asset.name))?;

    progress.set_stage(Stage::Downloading);
    progress.received.store(0, Ordering::Relaxed);
    progress.total.store(asset.size, Ordering::Relaxed);
    let mut response = client
        .get(&asset.url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(describe)?;
    if let Some(length) = response.content_length() {
        progress.total.store(length, Ordering::Relaxed);
    }

    let path = dir.join(file_name);
    let mut file = tokio::fs::File::create(&path)
        .await
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut received = 0u64;
    while let Some(chunk) = response.chunk().await.map_err(describe)? {
        received += chunk.len() as u64;
        if received > MAX_DOWNLOAD_BYTES {
            return Err("the download is far larger than any real package; stopped".to_string());
        }
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("could not write {}: {e}", path.display()))?;
        progress.received.store(received, Ordering::Relaxed);
    }
    file.flush().await.map_err(|e| e.to_string())?;
    drop(file);

    progress.set_stage(Stage::Verifying);
    let actual = hex(&hasher.finalize());
    if actual != expected {
        let _ = std::fs::remove_file(&path);
        return Err(format!(
            "the download does not match its published checksum (expected {expected}, got {actual}); nothing was installed"
        ));
    }
    Ok(path)
}

async fn run(program: &str, args: &[&std::ffi::OsStr]) -> Result<std::process::Output, String> {
    tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| format!("could not run {program}: {e}"))
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

/// Runs a package manager as root through polkit's `pkexec`.
async fn run_privileged(command: &[&std::ffi::OsStr]) -> Result<(), String> {
    if !on_path("pkexec") {
        return Err("`pkexec` (polkit) is needed to install system packages and was not found".to_string());
    }
    let output = run("pkexec", command).await?;
    match output.status.code() {
        Some(0) => Ok(()),
        // pkexec's own codes: 126 = dialog dismissed, 127 = not authorised.
        Some(126) | Some(127) => Err("the update was not authorised, so nothing was installed".to_string()),
        _ => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let last = stderr
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("no details");
            Err(format!("the package manager failed: {}", last.trim()))
        }
    }
}

async fn install_package(kind: &InstallKind, package: &Path) -> Result<(), String> {
    let package = package.as_os_str();
    let arg = std::ffi::OsStr::new;
    match kind {
        InstallKind::Rpm if on_path("dnf") => run_privileged(&[arg("dnf"), arg("install"), arg("-y"), package]).await,
        InstallKind::Rpm => run_privileged(&[arg("rpm"), arg("-U"), package]).await,
        InstallKind::Deb if on_path("apt-get") => {
            run_privileged(&[arg("apt-get"), arg("install"), arg("-y"), package]).await
        }
        InstallKind::Deb => run_privileged(&[arg("dpkg"), arg("-i"), package]).await,
        InstallKind::Tarball(exe) => replace_binary(exe, Path::new(package)).await,
        InstallKind::Flatpak | InstallKind::Unmanaged => Err("this copy cannot update itself".to_string()),
    }
}

/// Unpacks the release tarball's `oxidesip` and swaps it in for `exe`.
/// The swap is a rename within one directory, so a running copy keeps its
/// old file and a failure part-way leaves the old binary in place.
async fn replace_binary(exe: &Path, tarball: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let arg = std::ffi::OsStr::new;
    let unpack = tarball.parent().ok_or("bad download path")?.join("unpacked");
    std::fs::create_dir_all(&unpack).map_err(|e| e.to_string())?;
    // Only the one named member is extracted, so nothing else in the
    // archive can land anywhere.
    let output = run(
        "tar",
        &[
            arg("-xzf"),
            tarball.as_os_str(),
            arg("-C"),
            unpack.as_os_str(),
            arg("oxidesip"),
        ],
    )
    .await?;
    if !output.status.success() {
        return Err(format!(
            "could not unpack the download: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let fresh = unpack.join("oxidesip");
    let meta = std::fs::symlink_metadata(&fresh).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("the download does not contain the program".to_string());
    }
    let dir = exe.parent().ok_or("bad install path")?;
    let staged = dir.join(".oxidesip.update");
    std::fs::copy(&fresh, &staged).map_err(|e| format!("could not write to {}: {e}", dir.display()))?;
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    std::fs::rename(&staged, exe).map_err(|e| {
        let _ = std::fs::remove_file(&staged);
        format!("could not replace {}: {e}", exe.display())
    })
}

/// The whole update: download, verify, install. Returns the new version.
pub async fn download_and_install(
    release: Release,
    kind: InstallKind,
    progress: Arc<Progress>,
) -> Result<String, String> {
    let asset = kind
        .asset(&release)
        .ok_or_else(|| format!("release {} has no package for a {}", release.tag, kind.label()))?
        .clone();
    let dir = updates_dir()?;
    let result = async {
        let package = download_verified(&release, &asset, &dir, &progress).await?;
        progress.set_stage(Stage::Installing);
        install_package(&kind, &package).await
    }
    .await;
    let _ = std::fs::remove_dir_all(&dir);
    result.map(|()| release.version().to_string())
}

/// Starts the (now updated) program again. `exe` must have been captured
/// before the update: afterwards `current_exe` names a deleted file.
pub fn relaunch(exe: &Path) -> std::io::Result<()> {
    std::process::Command::new(exe)
        .stdin(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison() {
        assert!(is_newer("v0.4.1", "0.4.0"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("1.0", "0.99.99"));
        assert!(is_newer("v0.4.0.1", "0.4.0"));
        assert!(!is_newer("v0.4.0", "0.4.0"));
        assert!(!is_newer("v0.3.9", "0.4.0"));
        assert!(!is_newer("0.4", "0.4.0"));
        assert!(!is_newer("nightly", "0.4.0"));
        assert!(!is_newer("", "0.4.0"));
        // A pre-release suffix is ignored rather than treated as newer.
        assert!(!is_newer("v0.4.0-rc1", "0.4.0"));
    }

    #[test]
    fn checksum_lookup() {
        let a = "a".repeat(64);
        let b = "B".repeat(64);
        let sums = format!("{a}  oxidesip-linux-x86_64.tar.gz\n{b} *oxidesip-0.4.0-1.x86_64.rpm\nshort  x.deb\n");
        assert_eq!(checksum_for(&sums, "oxidesip-linux-x86_64.tar.gz"), Some(a));
        assert_eq!(
            checksum_for(&sums, "oxidesip-0.4.0-1.x86_64.rpm"),
            Some(b.to_ascii_lowercase())
        );
        assert_eq!(checksum_for(&sums, "x.deb"), None);
        assert_eq!(checksum_for(&sums, "missing.rpm"), None);
        // A name that merely ends with the wanted one must not match.
        assert_eq!(checksum_for(&sums, "x86_64.tar.gz"), None);
    }

    fn release(names: &[&str]) -> Release {
        Release {
            tag: "v9.9.9".into(),
            page: String::new(),
            assets: names
                .iter()
                .map(|n| Asset {
                    name: n.to_string(),
                    url: format!("{DOWNLOAD_PREFIX}v9.9.9/{n}"),
                    size: 1,
                })
                .collect(),
        }
    }

    #[test]
    fn picks_the_package_for_the_install_kind() {
        let r = release(&[
            "oxidesip-9.9.9-1.x86_64.rpm",
            "oxidesip-linux-x86_64.tar.gz",
            "oxidesip-x86_64.flatpak",
            "oxidesip_9.9.9-1_amd64.deb",
            "SHA256SUMS",
        ]);
        assert_eq!(InstallKind::Rpm.asset(&r).unwrap().name, "oxidesip-9.9.9-1.x86_64.rpm");
        assert_eq!(InstallKind::Deb.asset(&r).unwrap().name, "oxidesip_9.9.9-1_amd64.deb");
        assert_eq!(
            InstallKind::Tarball("/opt/oxidesip".into()).asset(&r).unwrap().name,
            TARBALL_ASSET
        );
        assert!(InstallKind::Flatpak.asset(&r).is_none());
        assert!(InstallKind::Unmanaged.asset(&r).is_none());
        assert_eq!(r.version(), "9.9.9");
    }

    #[tokio::test]
    async fn refuses_downloads_from_elsewhere() {
        let mut r = release(&["oxidesip-9.9.9-1.x86_64.rpm", "SHA256SUMS"]);
        r.assets[0].url = "https://example.com/oxidesip-9.9.9-1.x86_64.rpm".into();
        let asset = r.assets[0].clone();
        let dir = std::env::temp_dir();
        let err = download_verified(&r, &asset, &dir, &Progress::default())
            .await
            .unwrap_err();
        assert!(err.contains("unexpected address"), "{err}");

        let mut r = release(&["../evil.rpm", "SHA256SUMS"]);
        r.assets[0].url = format!("{DOWNLOAD_PREFIX}v9.9.9/evil.rpm");
        let asset = r.assets[0].clone();
        let err = download_verified(&r, &asset, &dir, &Progress::default())
            .await
            .unwrap_err();
        assert!(err.contains("unusable name"), "{err}");

        let r = release(&["oxidesip-9.9.9-1.x86_64.rpm"]);
        let asset = r.assets[0].clone();
        let err = download_verified(&r, &asset, &dir, &Progress::default())
            .await
            .unwrap_err();
        assert!(err.contains("SHA256SUMS"), "{err}");
    }

    /// Talks to GitHub: `cargo test -p softphone-ui -- --ignored updater`.
    #[tokio::test]
    #[ignore]
    async fn live_download_matches_published_checksum() {
        let release = fetch_latest().await.expect("latest release");
        assert!(parse_version(&release.tag).is_some(), "tag {}", release.tag);
        let dir = std::env::temp_dir().join(format!("oxidesip-updater-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for kind in [
            InstallKind::Rpm,
            InstallKind::Deb,
            InstallKind::Tarball(dir.join("oxidesip")),
        ] {
            let asset = kind.asset(&release).expect("asset for kind").clone();
            let progress = Progress::default();
            let path = download_verified(&release, &asset, &dir, &progress)
                .await
                .expect("verified download");
            assert_eq!(std::fs::metadata(&path).unwrap().len(), asset.size);
            assert_eq!(progress.fraction(), Some(1.0));
            if let InstallKind::Tarball(exe) = &kind {
                std::fs::write(exe, b"old").unwrap();
                replace_binary(exe, &path).await.expect("binary replaced");
                assert!(std::fs::metadata(exe).unwrap().len() > 1_000_000);
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
