#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error(transparent)]
    Sip(#[from] rsipstack::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("config error: {0}")]
    Config(String),

    #[error("sdp error: {0}")]
    Sdp(String),
}

pub type Result<T> = std::result::Result<T, CoreError>;
