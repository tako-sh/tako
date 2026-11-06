use crate::DeviceId;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid {field}: {reason}")]
    Invalid {
        field: &'static str,
        reason: &'static str,
    },
    #[error("conflicting {0} records")]
    Conflict(&'static str),
    #[error("device {0} is not an effective member")]
    NotEffective(DeviceId),
    #[error("permission denied: {0}")]
    PermissionDenied(&'static str),
    #[error("unknown revocation")]
    UnknownRevocation,
    #[error("profile does not match this device or its server pins")]
    ProfileMismatch,
    #[error("private key file must be a regular file owned by this user with mode 0600")]
    UnsafeKeyFile,
    #[error("secure key persistence is unsupported on this platform")]
    UnsupportedPlatform,
    #[error("I/O operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("SSH operation failed: {0}")]
    Ssh(#[from] ssh_key::Error),
    #[error("cryptographic operation failed: {0}")]
    Crypto(#[from] openssl::error::ErrorStack),
    #[error("invalid JSON data: {0}")]
    Json(#[from] serde_json::Error),
}

pub(crate) fn invalid(field: &'static str, reason: &'static str) -> Error {
    Error::Invalid { field, reason }
}

#[cfg(unix)]
impl From<rustix::io::Errno> for Error {
    fn from(error: rustix::io::Errno) -> Self {
        Self::Io(error.into())
    }
}
