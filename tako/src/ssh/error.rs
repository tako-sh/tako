//! SSH error types

use std::path::PathBuf;
use thiserror::Error;

/// Errors that can occur during SSH operations
#[derive(Debug, Error)]
pub enum SshError {
    #[error("{0}")]
    Connection(String),

    #[error("Authentication failed: {0}")]
    Authentication(String),

    #[error("No SSH keys found in {0}")]
    NoKeysFound(PathBuf),

    #[error("Failed to load SSH key {path}: {reason}")]
    KeyLoad { path: PathBuf, reason: String },

    #[error("{0}")]
    CommandFailed(String),

    #[error("Command returned non-zero exit code: {code}")]
    NonZeroExit { code: u32, stderr: String },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Channel error: {0}")]
    Channel(String),

    #[error("Timeout: {0}")]
    Timeout(String),

    #[error("Host key verification failed for {host}")]
    HostKeyVerification { host: String },

    #[error("Not connected")]
    NotConnected,

    #[error("SSH protocol error: {0}")]
    Protocol(String),
}

impl From<russh::Error> for SshError {
    fn from(e: russh::Error) -> Self {
        SshError::Protocol(e.to_string())
    }
}

/// Result type for SSH operations
pub type SshResult<T> = std::result::Result<T, SshError>;
