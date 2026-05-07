use std::io::Write;
use std::path::{Path, PathBuf};

use thiserror::Error;

pub(crate) const IDENTITY_KEY_FILE: &str = "identity.key";
pub(crate) const IDENTITY_PUBLIC_FILE: &str = "identity.pub";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServerIdentity {
    pub(crate) fingerprint: String,
}

#[derive(Debug, Error)]
pub(crate) enum IdentityError {
    #[error("read server identity {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("write server identity {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("invalid server identity {path}: {message}")]
    Invalid { path: PathBuf, message: String },
}

pub(crate) fn load_or_create_server_identity(
    data_dir: &Path,
) -> Result<ServerIdentity, IdentityError> {
    let key_path = data_dir.join(IDENTITY_KEY_FILE);
    let public_path = data_dir.join(IDENTITY_PUBLIC_FILE);

    let key = if key_path.exists() {
        load_private_key(&key_path)?
    } else {
        create_private_key(&key_path)?
    };

    write_public_key(&public_path, &key)?;

    Ok(ServerIdentity {
        fingerprint: key.fingerprint(Default::default()).to_string(),
    })
}

fn load_private_key(path: &Path) -> Result<ssh_key::PrivateKey, IdentityError> {
    let raw = std::fs::read_to_string(path).map_err(|source| IdentityError::Read {
        path: path.to_path_buf(),
        source,
    })?;

    ssh_key::PrivateKey::from_openssh(&raw).map_err(|error| IdentityError::Invalid {
        path: path.to_path_buf(),
        message: error.to_string(),
    })
}

fn create_private_key(path: &Path) -> Result<ssh_key::PrivateKey, IdentityError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|source| IdentityError::Write {
        path: parent.to_path_buf(),
        source,
    })?;

    let mut key =
        ssh_key::PrivateKey::random(&mut ssh_key::rand_core::OsRng, ssh_key::Algorithm::Ed25519)
            .map_err(|error| IdentityError::Invalid {
                path: path.to_path_buf(),
                message: error.to_string(),
            })?;
    key.set_comment("tako-server identity");

    let encoded =
        key.to_openssh(ssh_key::LineEnding::LF)
            .map_err(|error| IdentityError::Invalid {
                path: path.to_path_buf(),
                message: error.to_string(),
            })?;
    write_private_key(path, encoded.as_bytes())?;
    Ok(key)
}

#[cfg(unix)]
fn write_private_key(path: &Path, bytes: &[u8]) -> Result<(), IdentityError> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|source| IdentityError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    file.write_all(bytes)
        .map_err(|source| IdentityError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|source| {
        IdentityError::Write {
            path: path.to_path_buf(),
            source,
        }
    })?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private_key(path: &Path, bytes: &[u8]) -> Result<(), IdentityError> {
    std::fs::write(path, bytes).map_err(|source| IdentityError::Write {
        path: path.to_path_buf(),
        source,
    })
}

fn write_public_key(path: &Path, key: &ssh_key::PrivateKey) -> Result<(), IdentityError> {
    let public = key
        .public_key()
        .to_openssh()
        .map_err(|error| IdentityError::Invalid {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
    std::fs::write(path, format!("{public}\n")).map_err(|source| IdentityError::Write {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn load_or_create_server_identity_creates_keypair_files() {
        let temp = tempfile::tempdir().expect("tempdir");

        let identity = load_or_create_server_identity(temp.path()).expect("identity");

        assert!(
            identity.fingerprint.starts_with("SHA256:"),
            "fingerprint should be OpenSSH-style SHA256: {}",
            identity.fingerprint
        );
        assert!(temp.path().join(IDENTITY_KEY_FILE).is_file());
        assert!(temp.path().join(IDENTITY_PUBLIC_FILE).is_file());

        #[cfg(unix)]
        {
            let mode = std::fs::metadata(temp.path().join(IDENTITY_KEY_FILE))
                .expect("private key metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn load_or_create_server_identity_reuses_existing_key() {
        let temp = tempfile::tempdir().expect("tempdir");

        let first = load_or_create_server_identity(temp.path()).expect("first identity");
        let second = load_or_create_server_identity(temp.path()).expect("second identity");

        assert_eq!(first.fingerprint, second.fingerprint);
    }

    #[test]
    fn load_or_create_server_identity_rejects_invalid_existing_key() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join(IDENTITY_KEY_FILE), "not a key")
            .expect("write invalid key");

        let err = load_or_create_server_identity(temp.path()).unwrap_err();

        assert!(
            matches!(err, IdentityError::Invalid { .. }),
            "expected invalid identity error, got {err:?}"
        );
    }
}
