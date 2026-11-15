//! Manual archive status and retention for existing database-only archives.
pub(super) const DATABASE_SNAPSHOT_RETENTION_SECS: i64 = 48 * 60 * 60;
pub(super) const DISK_RESERVE_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub(crate) struct ManualBackupState {
    pub(super) failure: Option<tako_core::BackupFailure>,
}
impl ManualBackupState {
    pub(super) fn failure(&self) -> Option<&tako_core::BackupFailure> {
        self.failure.as_ref()
    }
}
