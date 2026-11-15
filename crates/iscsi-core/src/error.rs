use thiserror::Error;

/// Errors are separated so frontends can distinguish usage, login and I/O.
#[derive(Debug, Error)]
pub enum Error {
    #[error("transport: {0}")]
    Transport(#[from] std::io::Error),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("login: {0}")]
    Login(String),
    #[error("SCSI status {status:#04x}, sense {sense:02x?}")]
    Scsi { status: u8, sense: Vec<u8> },
    #[error("digest mismatch: {0}")]
    Digest(&'static str),
    #[error("operation timed out")]
    Timeout,
    #[error("configuration: {0}")]
    Config(String),
}
