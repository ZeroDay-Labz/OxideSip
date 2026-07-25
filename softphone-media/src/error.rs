#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("pipewire error: {0}")]
    PipeWire(String),

    #[error("rtp packet too short")]
    RtpTooShort,

    #[error("unsupported rtp version {0}")]
    RtpBadVersion(u8),
}
