use crate::error::{CoreError, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SipTransport {
    #[default]
    Udp,
    Tcp,
    Tls,
}

impl SipTransport {
    fn parse_env(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "udp" => Some(Self::Udp),
            "tcp" => Some(Self::Tcp),
            "tls" => Some(Self::Tls),
            _ => None,
        }
    }
}

impl std::fmt::Display for SipTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SipTransport::Udp => "UDP",
            SipTransport::Tcp => "TCP",
            SipTransport::Tls => "TLS",
        })
    }
}

pub const SIP_TRANSPORTS: [SipTransport; 3] = [SipTransport::Udp, SipTransport::Tcp, SipTransport::Tls];

/// A G.711 variant, orderable in `SipAccountConfig::preferred_codecs` to rank
/// which codec wins when both sending an SDP offer (placing a call, or
/// resuming a held one) and answering one (see `sdp::select_payload_type`,
/// used from `dialog.rs` in both directions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PreferredCodec {
    #[default]
    Ulaw,
    Alaw,
}

impl PreferredCodec {
    /// The static RTP payload type for this codec (RFC 3551).
    pub fn payload_type(self) -> u8 {
        match self {
            PreferredCodec::Ulaw => 0,
            PreferredCodec::Alaw => 8,
        }
    }

    fn parse_env(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "ulaw" => Some(Self::Ulaw),
            "alaw" => Some(Self::Alaw),
            _ => None,
        }
    }
}

impl std::fmt::Display for PreferredCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PreferredCodec::Ulaw => "G.711 u-law (PCMU)",
            PreferredCodec::Alaw => "G.711 A-law (PCMA)",
        })
    }
}

pub const PREFERRED_CODECS: [PreferredCodec; 2] = [PreferredCodec::Ulaw, PreferredCodec::Alaw];

#[derive(Debug, Clone, Hash, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SipAccountConfig {
    /// Display name only — never sent over the wire. Distinguishes accounts
    /// in the UI's account switcher/SIP settings sidebar once more than one
    /// is configured (see `AccountsFile`/`load_accounts`).
    pub name: String,
    pub sip_server_host: String,
    pub sip_server_port: u16,
    pub transport: SipTransport,
    pub username: String,
    pub password: String,
    pub register_expires: u32,
    /// Local UDP/TCP bind port. 0 = ephemeral. Unused for TLS, which is
    /// outbound-only.
    pub local_port: u16,
    /// Offer SDES-SRTP instead of plain RTP. Stock FreePBX extensions default
    /// to `media_encryption=no`, so this defaults to false.
    pub srtp: bool,
    /// CA cert PEM path used to verify the registrar's TLS certificate.
    /// Required only when `transport == Tls`: we never fall back to the
    /// system trust store so an internal/self-signed CA must be trusted
    /// explicitly.
    pub ca_cert_path: Option<PathBuf>,
    pub client_cert_path: Option<PathBuf>,
    pub client_key_path: Option<PathBuf>,
    /// Codecs to offer, in priority order — the first entry that the
    /// remote side also supports wins (see `sdp::select_payload_type`).
    /// Always non-empty in practice (`Default`/`RawConfig::into_config`
    /// both guarantee at least `[Ulaw, Alaw]`), but not a `vec1` since
    /// there's no real invariant-violation risk worth the extra type
    /// ceremony here — the UI's codec list editor just always keeps at
    /// least one entry.
    pub preferred_codecs: Vec<PreferredCodec>,
    /// Bumped on every SIP Settings Save (see `app.rs::handle_sip_settings_save`),
    /// even when nothing else changed. Never persisted — its only purpose is
    /// to change this struct's `Hash` so `bridge::subscription`'s
    /// `Subscription::run_with` (keyed off `Vec<SipAccountConfig>`'s hash)
    /// tears down and respawns a fresh `SoftphoneCore` (and thus a fresh
    /// registration attempt) purely because the user hit Save — the
    /// designated recovery path after the registration loop halts on
    /// repeated auth failures (see `registration.rs`).
    #[serde(skip)]
    pub reg_epoch: u64,
}

impl Default for SipAccountConfig {
    /// Blank starting point for first-run boot (no config file/env vars yet)
    /// so the UI can land on a Settings screen instead of hard-failing.
    fn default() -> Self {
        SipAccountConfig {
            name: String::new(),
            sip_server_host: String::new(),
            sip_server_port: 5060,
            transport: SipTransport::Udp,
            username: String::new(),
            password: String::new(),
            register_expires: 3600,
            local_port: 0,
            srtp: false,
            ca_cert_path: None,
            client_cert_path: None,
            client_key_path: None,
            preferred_codecs: vec![PreferredCodec::Ulaw, PreferredCodec::Alaw],
            reg_epoch: 0,
        }
    }
}

#[derive(Debug, Default, serde::Deserialize)]
struct RawConfig {
    name: Option<String>,
    sip_server_host: Option<String>,
    sip_server_port: Option<u16>,
    transport: Option<SipTransport>,
    username: Option<String>,
    password: Option<String>,
    register_expires: Option<u32>,
    local_port: Option<u16>,
    srtp: Option<bool>,
    ca_cert_path: Option<PathBuf>,
    client_cert_path: Option<PathBuf>,
    client_key_path: Option<PathBuf>,
    preferred_codecs: Option<Vec<PreferredCodec>>,
}

impl RawConfig {
    fn apply_env_overrides(&mut self) {
        if let Ok(v) = std::env::var("OXIDESIP_SIP_SERVER_HOST") {
            self.sip_server_host = Some(v);
        }
        if let Ok(v) = std::env::var("OXIDESIP_SIP_SERVER_PORT")
            && let Ok(p) = v.parse()
        {
            self.sip_server_port = Some(p);
        }
        if let Ok(v) = std::env::var("OXIDESIP_TRANSPORT")
            && let Some(t) = SipTransport::parse_env(&v)
        {
            self.transport = Some(t);
        }
        if let Ok(v) = std::env::var("OXIDESIP_USERNAME") {
            self.username = Some(v);
        }
        if let Ok(v) = std::env::var("OXIDESIP_PASSWORD") {
            self.password = Some(v);
        }
        if let Ok(v) = std::env::var("OXIDESIP_REGISTER_EXPIRES")
            && let Ok(p) = v.parse()
        {
            self.register_expires = Some(p);
        }
        if let Ok(v) = std::env::var("OXIDESIP_LOCAL_PORT")
            && let Ok(p) = v.parse()
        {
            self.local_port = Some(p);
        }
        if let Ok(v) = std::env::var("OXIDESIP_SRTP")
            && let Ok(b) = v.parse()
        {
            self.srtp = Some(b);
        }
        if let Ok(v) = std::env::var("OXIDESIP_CA_CERT_PATH") {
            self.ca_cert_path = Some(v.into());
        }
        if let Ok(v) = std::env::var("OXIDESIP_CLIENT_CERT_PATH") {
            self.client_cert_path = Some(v.into());
        }
        if let Ok(v) = std::env::var("OXIDESIP_CLIENT_KEY_PATH") {
            self.client_key_path = Some(v.into());
        }
        if let Ok(v) = std::env::var("OXIDESIP_PREFERRED_CODECS") {
            let codecs: Vec<PreferredCodec> =
                v.split(',').filter_map(|s| PreferredCodec::parse_env(s.trim())).collect();
            if !codecs.is_empty() {
                self.preferred_codecs = Some(codecs);
            }
        }
    }

    fn into_config(self) -> Result<SipAccountConfig> {
        let transport = self.transport.unwrap_or_default();
        let sip_server_port = self.sip_server_port.unwrap_or(match transport {
            SipTransport::Tls => 5061,
            SipTransport::Udp | SipTransport::Tcp => 5060,
        });
        let ca_cert_path = match transport {
            SipTransport::Tls => Some(self.ca_cert_path.ok_or_else(|| {
                CoreError::Config("missing ca_cert_path (required for transport = \"tls\")".into())
            })?),
            SipTransport::Udp | SipTransport::Tcp => self.ca_cert_path,
        };

        Ok(SipAccountConfig {
            name: self.name.unwrap_or_default(),
            sip_server_host: self
                .sip_server_host
                .ok_or_else(|| CoreError::Config("missing sip_server_host".into()))?,
            sip_server_port,
            transport,
            username: self
                .username
                .ok_or_else(|| CoreError::Config("missing username".into()))?,
            password: self
                .password
                .ok_or_else(|| CoreError::Config("missing password".into()))?,
            register_expires: self.register_expires.unwrap_or(3600),
            local_port: self.local_port.unwrap_or(0),
            srtp: self.srtp.unwrap_or(false),
            ca_cert_path,
            client_cert_path: self.client_cert_path,
            client_key_path: self.client_key_path,
            preferred_codecs: self
                .preferred_codecs
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| vec![PreferredCodec::Ulaw, PreferredCodec::Alaw]),
            reg_epoch: 0,
        })
    }
}

/// Load config from a TOML file, then apply `OXIDESIP_*` env var overrides.
pub fn load_config(path: &Path) -> Result<SipAccountConfig> {
    let text = std::fs::read_to_string(path)?;
    let mut raw: RawConfig =
        toml::from_str(&text).map_err(|e| CoreError::Config(e.to_string()))?;
    raw.apply_env_overrides();
    raw.into_config()
}

/// Load config purely from `OXIDESIP_*` env vars (no TOML file).
pub fn load_config_from_env() -> Result<SipAccountConfig> {
    let mut raw = RawConfig::default();
    raw.apply_env_overrides();
    raw.into_config()
}

/// Persist `config` to `path` as TOML, overwriting whatever was there
/// (including any hand-written comments) — acceptable since the file
/// becomes app-managed once saved from the Settings screen.
pub fn save_config(path: &Path, config: &SipAccountConfig) -> Result<()> {
    let text = toml::to_string_pretty(config).map_err(|e| CoreError::Config(e.to_string()))?;
    std::fs::write(path, text)?;
    Ok(())
}

/// Multiple registered accounts, persisted as one TOML file (`[[accounts]]`
/// array-of-tables) — separate from `load_config`/`save_config`'s single-
/// account `SipAccountConfig` file so existing single-account setups keep
/// working untouched; `softphone-ui` migrates a legacy single-account file
/// into a one-entry `AccountsFile` the first time it finds no accounts file
/// (see `main.rs`).
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct AccountsFile {
    #[serde(default)]
    pub accounts: Vec<SipAccountConfig>,
}

/// Loads every configured account from `path`. Returns an empty `Vec` (not
/// an error) if the file doesn't exist yet — the normal first-run state
/// before any account has been added or migrated.
pub fn load_accounts(path: &Path) -> Result<Vec<SipAccountConfig>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let file: AccountsFile = toml::from_str(&text).map_err(|e| CoreError::Config(e.to_string()))?;
    Ok(file.accounts)
}

pub fn save_accounts(path: &Path, accounts: &[SipAccountConfig]) -> Result<()> {
    let file = AccountsFile {
        accounts: accounts.to_vec(),
    };
    let text = toml::to_string_pretty(&file).map_err(|e| CoreError::Config(e.to_string()))?;
    std::fs::write(path, text)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_required_field_errors() {
        let raw = RawConfig::default();
        assert!(raw.into_config().is_err());
    }

    #[test]
    fn defaults_applied() {
        let raw = RawConfig {
            sip_server_host: Some("pbx.example.com".into()),
            username: Some("1001".into()),
            password: Some("secret".into()),
            ..Default::default()
        };
        let cfg = raw.into_config().unwrap();
        assert_eq!(cfg.transport, SipTransport::Udp);
        assert_eq!(cfg.sip_server_port, 5060);
        assert_eq!(cfg.register_expires, 3600);
        assert_eq!(cfg.local_port, 0);
        assert!(!cfg.srtp);
        assert!(cfg.ca_cert_path.is_none());
    }

    #[test]
    fn tls_transport_defaults_and_requires_ca_cert() {
        let raw = RawConfig {
            sip_server_host: Some("pbx.example.com".into()),
            username: Some("1001".into()),
            password: Some("secret".into()),
            transport: Some(SipTransport::Tls),
            ..Default::default()
        };
        assert!(raw.into_config().is_err());

        let raw = RawConfig {
            sip_server_host: Some("pbx.example.com".into()),
            username: Some("1001".into()),
            password: Some("secret".into()),
            transport: Some(SipTransport::Tls),
            ca_cert_path: Some("./ca.pem".into()),
            ..Default::default()
        };
        let cfg = raw.into_config().unwrap();
        assert_eq!(cfg.sip_server_port, 5061);
    }
}
