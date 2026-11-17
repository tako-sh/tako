//! Tako-hosted databases: the wire types shared by the CLI, the server, and
//! app SDKs.
//!
//! A database is a SQLite file owned by the server that hosts it (its home
//! server). Its name identifies it: names are unique within a network. Apps
//! reach it through a binding: the server gives each bound app process a URL
//! and token for the SQL over HTTP protocol.

use serde::{Deserialize, Serialize};

use crate::storage::StorageBinding;

/// Default loopback port of the database endpoint for apps.
pub const DATABASE_APP_PORT: u16 = 9845;
/// Default private-network port where home servers accept forwarded requests.
pub const DATABASE_PEER_PORT: u16 = 9847;
/// Bytes of a database file moved per import or export request.
pub const DATABASE_TRANSFER_CHUNK_BYTES: u64 = 512 * 1024;
/// Rows returned by `QueryDatabase` before the result is truncated.
pub const DATABASE_QUERY_MAX_ROWS: usize = 1000;

/// Validates a database generation: 32 lowercase hexadecimal characters,
/// chosen at random when the database is created. It tells a database apart
/// from an earlier one with the same name, e.g. in backup storage.
pub fn validate_database_generation(generation: &str) -> Result<(), String> {
    if generation.len() == 32
        && generation
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Ok(())
    } else {
        Err(format!(
            "database generation `{generation}` must be 32 lowercase hexadecimal characters"
        ))
    }
}

/// Validates a database or binding name: lowercase letters, digits, `-` and
/// `_`, starting with a letter, at most 63 characters.
pub fn validate_database_name(name: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name.len() <= 63
        && name.as_bytes()[0].is_ascii_lowercase()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if valid {
        Ok(())
    } else {
        Err(format!(
            "database name `{name}` must start with a lowercase letter and contain only lowercase letters, digits, `-` and `_` (at most 63 characters)"
        ))
    }
}

/// A database bound to an app environment, sent with `Command::Deploy`.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DatabaseBinding {
    /// The database's name, unique within the network.
    pub name: String,
    /// Device ID of the database's server.
    pub server: String,
    /// Key the database's server issued with its grant to this app
    /// environment. Other servers present it when they forward requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_key: Option<String>,
}

impl DatabaseBinding {
    /// Whether two bindings name the same database the same way, ignoring
    /// the peer key a server fills in.
    pub fn same_target(&self, other: &Self) -> bool {
        self.name == other.name && self.server == other.server
    }
}

impl std::fmt::Debug for DatabaseBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatabaseBinding")
            .field("name", &self.name)
            .field("server", &self.server)
            .field("peer_key", &self.peer_key.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

/// What an app process receives for each binding in its bootstrap envelope.
/// The field names match libSQL client configuration, so the object can be
/// passed to `createClient()` directly.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DatabaseConnection {
    pub url: String,
    #[serde(rename = "authToken")]
    pub auth_token: String,
}

/// The token authorizes SQL as the app, so it is never printed.
impl std::fmt::Debug for DatabaseConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatabaseConnection")
            .field("url", &self.url)
            .field("auth_token", &"[REDACTED]")
            .finish()
    }
}

/// Why a snapshot was taken.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotReason {
    Scheduled,
    Manual,
    /// Before a deploy's release command.
    Release,
    /// Before a restore, import, or reset replaced the database.
    Restore,
    /// Before migrations were applied.
    Migration,
}

/// One SQL migration file, sent with `MigrateDatabase`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DatabaseMigration {
    /// The file name, such as `0001_initial.sql`, which orders it.
    pub name: String,
    pub sql: String,
}

/// The reply to `MigrateDatabase`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DatabaseMigrationReport {
    /// Migrations applied, in order.
    #[serde(default)]
    pub applied: Vec<String>,
    /// Migrations a dry run would apply, in order.
    #[serde(default)]
    pub pending: Vec<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// One uploaded snapshot of a database.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DatabaseSnapshot {
    pub id: String,
    pub created_at_unix_secs: u64,
    pub reason: SnapshotReason,
    /// Size of the database file the snapshot restores.
    pub database_bytes: u64,
    /// Size of the uploaded, compressed and encrypted object.
    pub stored_bytes: u64,
    /// SHA-256 of the database file, hex encoded.
    pub sha256: String,
}

/// Backup state of one database.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DatabaseBackupStatus {
    pub bucket: String,
    pub endpoint: String,
    /// The full location, which a move matches against the environments'
    /// storages to read the database's snapshots.
    #[serde(default)]
    pub destination: Option<BackupDestination>,
    #[serde(default)]
    pub last_snapshot: Option<DatabaseSnapshot>,
    #[serde(default)]
    pub last_attempt_unix_secs: Option<u64>,
    #[serde(default)]
    pub last_error: Option<String>,
}

/// A database hosted on a server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DatabaseInfo {
    pub name: String,
    /// The random ID chosen when the database was created; copies of one
    /// database on different servers share it.
    #[serde(default)]
    pub generation: String,
    pub created_at_unix_secs: u64,
    pub size_bytes: u64,
    /// App environments on this server bound to the database.
    pub bindings: Vec<String>,
    /// App environments on other servers granted the database.
    pub remote_grants: Vec<String>,
    #[serde(default)]
    pub backup: Option<DatabaseBackupStatus>,
}

/// The result of `QueryDatabase`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DatabaseQueryResult {
    pub columns: Vec<String>,
    /// Values as JSON: null, numbers, strings, and blobs as `{"base64": ...}`.
    pub rows: Vec<Vec<serde_json::Value>>,
    pub affected_row_count: u64,
    pub truncated: bool,
}

/// The result of starting an export.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DatabaseExport {
    pub export_id: String,
    pub size_bytes: u64,
    pub sha256: String,
}

/// Where a storage binding's objects live: every field that addresses the
/// bucket, without the credentials, which can rotate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BackupDestination {
    pub provider: crate::storage::StorageProvider,
    pub bucket: Option<String>,
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub force_path_style: bool,
}

impl BackupDestination {
    pub fn of(binding: &StorageBinding) -> Self {
        Self {
            provider: binding.provider,
            bucket: binding.bucket.clone(),
            endpoint: binding.endpoint.clone(),
            region: binding.region.clone(),
            force_path_style: binding.force_path_style,
        }
    }
}

/// Whether two storage bindings name the same location.
pub fn same_backup_destination(a: &StorageBinding, b: &StorageBinding) -> bool {
    BackupDestination::of(a) == BackupDestination::of(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_generations_are_validated() {
        assert!(validate_database_generation("0123456789abcdef0123456789abcdef").is_ok());
        assert!(validate_database_generation("0123456789ABCDEF0123456789ABCDEF").is_err());
        assert!(validate_database_generation("short").is_err());
        assert!(validate_database_name("main").is_ok());
        assert!(validate_database_name("orders_2").is_ok());
        assert!(validate_database_name("Main").is_err());
        assert!(validate_database_name("2main").is_err());
        assert!(validate_database_name("").is_err());
    }

    #[test]
    fn connections_never_print_their_token() {
        let connection = DatabaseConnection {
            url: "http://127.0.0.1:9845".into(),
            auth_token: "secret-token".into(),
        };
        let printed = format!("{connection:?}");
        assert!(!printed.contains("secret-token"), "{printed}");
        assert!(printed.contains("127.0.0.1:9845"));
    }

    #[test]
    fn connections_use_libsql_field_names() {
        let value = serde_json::to_value(DatabaseConnection {
            url: "http://127.0.0.1:9845".into(),
            auth_token: "t".into(),
        })
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({"url": "http://127.0.0.1:9845", "authToken": "t"})
        );
    }
}
