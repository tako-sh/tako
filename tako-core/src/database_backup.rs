//! Continuous SQLite backup configuration and recovery records.
use std::{
    collections::BTreeSet,
    path::{Component, Path},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const DATABASE_BACKUP_RETENTION_DAYS: u16 = 7;
pub const DATABASE_BACKUP_INTERVAL_SECS: u64 = 60;

/// Stable across consumer removal, redeployment, and backup key rotation.
pub fn database_replica_id(app: &str, database: &str, shared: bool) -> String {
    hex::encode(Sha256::digest(format!(
        "{}\0{database}",
        if shared { "shared" } else { app }
    )))
}

/// Credentials can rotate without changing the location of a replica.
pub fn same_backup_destination(a: &crate::StorageBinding, b: &crate::StorageBinding) -> bool {
    a.provider == b.provider
        && a.bucket == b.bucket
        && a.endpoint == b.endpoint
        && a.region == b.region
        && a.force_path_style == b.force_path_style
}

/// Paths are relative to TAKO_DATA_DIR or TAKO_SHARED_DATA_DIR. Requiring
/// normalized names gives a database one registration and prevents traversal.
pub fn validate_database_paths(paths: &[String]) -> Result<(), String> {
    if paths.len() > 64 {
        return Err("At most 64 databases can be configured in each backup scope.".into());
    }
    let mut unique = BTreeSet::new();
    for value in paths {
        let path = Path::new(value);
        if value.is_empty()
            || value.len() > 1024
            || value
                .chars()
                .any(|c| c.is_control() || matches!(c, '\\' | ':'))
            || value
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
            || path
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
            || ["-wal", "-shm", "-journal"]
                .iter()
                .any(|suffix| value.ends_with(suffix))
        {
            return Err(format!(
                "Invalid database path {value:?}: use a normalized relative SQLite filename, not a sidecar."
            ));
        }
        if !unique.insert(value) {
            return Err(format!(
                "Database path {value:?} is configured more than once."
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DatabaseBackupStatus {
    pub database: String,
    pub shared: bool,
    pub replica_id: String,
    pub consumers: Vec<String>,
    pub active: bool,
    pub captured_txid: Option<u64>,
    pub uploaded_txid: Option<u64>,
    pub uploaded_at_unix_secs: Option<i64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DatabaseRecoveryPoint {
    pub txid: u64,
    /// Remote publication time. This is not the SQLite transaction time.
    pub uploaded_at_unix_secs: i64,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DatabaseBackupsResponse {
    pub database: String,
    pub shared: bool,
    pub replica_id: String,
    pub points: Vec<DatabaseRecoveryPoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DatabaseRestoreResponse {
    pub database: String,
    pub shared: bool,
    pub replica_id: String,
    pub txid: u64,
    pub affected_apps: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_paths_reject_aliases_and_sidecars() {
        for path in [
            "",
            "/app.db",
            "../app.db",
            "a/../app.db",
            "./app.db",
            "a//app.db",
            "a/",
            "file:app.db",
            "a\\app.db",
            "app.db-wal",
        ] {
            assert!(validate_database_paths(&[path.into()]).is_err(), "{path}");
        }
        assert!(validate_database_paths(&["db.sqlite".into(), "db.sqlite".into()]).is_err());
        validate_database_paths(&["app.sqlite".into(), "billing/orders.db".into()]).unwrap();
    }
}
