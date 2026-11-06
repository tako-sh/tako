use std::fs::{File, TryLockError};
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tako_network::node_config::{NodeConfig, protected::Directory};

const ACTIVITY_FILE: &str = "agent-management-activity.lock";
const GENERATION_FILE: &str = "agent-management-generation.json";

pub(crate) struct PrimaryGeneration {
    directory: Directory,
    token: String,
}

#[must_use]
pub(crate) struct PrimaryHandoff {
    generation: Arc<PrimaryGeneration>,
    _exclusive: File,
}

#[derive(Debug)]
#[must_use]
pub(crate) struct NativeDispatchGuard {
    _lease: File,
}

impl PrimaryHandoff {
    pub(crate) async fn claim(data_dir: &Path, timeout: Duration) -> io::Result<Self> {
        let config = NodeConfig::load(data_dir).map_err(network_error)?;
        let directory =
            Directory::open(&config.network_dir(data_dir), true).map_err(network_error)?;
        Self::claim_in(directory, timeout).await
    }

    async fn claim_in(directory: Directory, timeout: Duration) -> io::Result<Self> {
        let exclusive = activity_file(&directory)?;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match exclusive.try_lock() {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) => {
                    let now = tokio::time::Instant::now();
                    if now >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "Native management handoff is waiting for admitted work",
                        ));
                    }
                    tokio::time::sleep((deadline - now).min(Duration::from_millis(10))).await;
                }
                Err(TryLockError::Error(error)) => return Err(error),
            }
        }
        let token = nanoid::nanoid!(32);
        directory
            .write(
                GENERATION_FILE,
                &serde_json::to_vec(&token).map_err(io::Error::other)?,
            )
            .map_err(network_error)?;
        Ok(Self {
            generation: Arc::new(PrimaryGeneration { directory, token }),
            _exclusive: exclusive,
        })
    }

    #[cfg(test)]
    pub(crate) fn generation(&self) -> Arc<PrimaryGeneration> {
        self.generation.clone()
    }

    // Restore authoritative state and publish the listener before releasing this lease.
    pub(crate) fn finish(self) -> Arc<PrimaryGeneration> {
        self.generation
    }
}

impl PrimaryGeneration {
    pub(crate) fn admit(&self) -> io::Result<NativeDispatchGuard> {
        let lease = self
            .directory
            .open_file(ACTIVITY_FILE)
            .map_err(network_error)?;
        lease.try_lock_shared().map_err(|error| match error {
            TryLockError::WouldBlock => io::Error::new(
                io::ErrorKind::WouldBlock,
                "Native management primary is changing",
            ),
            TryLockError::Error(error) => error,
        })?;
        let bytes = self
            .directory
            .read(GENERATION_FILE, 128)
            .map_err(network_error)?;
        let current: String = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        if current != self.token {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Native management primary was replaced",
            ));
        }
        Ok(NativeDispatchGuard { _lease: lease })
    }
}

fn activity_file(directory: &Directory) -> io::Result<File> {
    match directory.open_file(ACTIVITY_FILE) {
        Ok(file) => return Ok(file),
        Err(tako_network::Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(network_error(error)),
    }
    if directory.exists(GENERATION_FILE).map_err(network_error)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Native management activity lock is missing",
        ));
    }
    // Creation must preserve the inode even if another process wins this race.
    match directory.lock(ACTIVITY_FILE) {
        Ok(file) => drop(file),
        Err(tako_network::Error::Invalid {
            field: "agent lock",
            ..
        }) => (),
        Err(error) => return Err(network_error(error)),
    }
    directory.open_file(ACTIVITY_FILE).map_err(network_error)
}

fn network_error(error: tako_network::Error) -> io::Error {
    match error {
        tako_network::Error::Io(error) => error,
        error => io::Error::other(error.to_string()),
    }
}

#[cfg(test)]
mod tests;
