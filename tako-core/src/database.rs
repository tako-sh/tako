//! Tako-hosted databases: the wire types shared by the CLI, the server, and
//! app SDKs.
//!
//! A database is a file owned by the server that hosts it (its home server):
//! a SQLite database, or a key-value store of JSON values with optional
//! expiry. Its name identifies it: names are unique within a network, across
//! both engines. Apps reach it through a binding: the server gives each bound
//! app process a URL and token for the SQL over HTTP protocol, or a token the
//! SDK sends through the internal socket for a key-value store.

use serde::{Deserialize, Serialize};

use crate::storage::StorageBinding;

/// Server-owned deploy binding; deliberately not a valid application SQL alias.
pub const CHANNEL_REPLAY_BINDING: &str = "@channels";
/// Server-owned deploy binding of the database that holds an app's workflow
/// runs. Like [`CHANNEL_REPLAY_BINDING`], it is not a valid SQL alias.
pub const WORKFLOWS_BINDING: &str = "@workflows";

/// Whether `binding` is one tako-server uses on the app's behalf: channel
/// replay or workflow runs. App processes never receive these tokens.
pub fn is_runtime_binding(binding: &str) -> bool {
    binding == CHANNEL_REPLAY_BINDING || binding == WORKFLOWS_BINDING
}

/// The host scope of an app's runtime binding. Each runtime service has its
/// own grant, so a peer grant for it never authorizes application SQL.
pub fn runtime_grant_scope(binding: &str, app: &str) -> Option<String> {
    match binding {
        CHANNEL_REPLAY_BINDING => Some(channel_replay_grant_scope(app)),
        WORKFLOWS_BINDING => Some(workflows_grant_scope(app)),
        _ => None,
    }
}

/// Peer grants for the replay service never authorize application SQL.
pub fn channel_replay_grant_scope(app: &str) -> String {
    format!("{app}#channels")
}

/// Peer grants for the workflow store never authorize application SQL.
pub fn workflows_grant_scope(app: &str) -> String {
    format!("{app}#workflows")
}

/// Default loopback port of the database endpoint for apps.
pub const DATABASE_APP_PORT: u16 = 9845;
/// Default private-network port where home servers accept forwarded requests.
pub const DATABASE_PEER_PORT: u16 = 9847;
/// Default private-network port where a KV store's server accepts
/// requests forwarded for apps on other servers.
pub const KV_PEER_PORT: u16 = 9848;
/// Private Raft traffic among enrolled servers.
pub const STORAGE_PEER_PORT: u16 = 9849;
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

/// What a hosted database is.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseEngine {
    /// SQLite, served over the libSQL HTTP protocol.
    #[default]
    Sqlite,
    /// A key-value store, served through the internal socket.
    Kv,
}

impl DatabaseEngine {
    pub fn is_sqlite(&self) -> bool {
        *self == Self::Sqlite
    }

    /// How people call a database of this engine in messages.
    pub fn noun(self) -> &'static str {
        match self {
            Self::Sqlite => "database",
            Self::Kv => "KV store",
        }
    }
}

/// A database bound to an app environment, sent with `Command::Deploy`.
/// Bindings of both engines share one map, so a binding name is used once
/// per environment.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DatabaseBinding {
    /// The database's name, unique within the network.
    pub name: String,
    /// Device ID of the database's server.
    pub server: String,
    /// An immutable resource generation when this binding uses Raft.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<String>,
    /// Key the database's server issued with its grant to this app
    /// environment. Other servers present it when they forward requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_key: Option<String>,
    #[serde(default, skip_serializing_if = "DatabaseEngine::is_sqlite")]
    pub engine: DatabaseEngine,
}

impl DatabaseBinding {
    /// Whether two bindings name the same database the same way, ignoring
    /// the peer key a server fills in.
    pub fn same_target(&self, other: &Self) -> bool {
        self.name == other.name
            && self.engine == other.engine
            && self.generation == other.generation
            && (self.generation.is_some() || self.server == other.server)
    }
}

impl std::fmt::Debug for DatabaseBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatabaseBinding")
            .field("name", &self.name)
            .field("server", &self.server)
            .field("generation", &self.generation)
            .field("peer_key", &self.peer_key.as_ref().map(|_| "[REDACTED]"))
            .field("engine", &self.engine)
            .finish()
    }
}

/// Bearer credentials for the writable route and the local read-only route.
/// Binding registration stores only their hashes. Knowing the replica token
/// does not give access to the writable route.
pub fn replica_token(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::new()
        .chain_update(b"tako.read-replica.v1\0")
        .chain_update(token)
        .finalize();
    hex::encode(digest)
}

#[derive(Clone, Debug)]
pub struct BindingTokenHashes {
    pub primary: [u8; 32],
    pub replica: [u8; 32],
}

impl BindingTokenHashes {
    pub fn new(token: &str) -> Self {
        use sha2::{Digest, Sha256};
        Self {
            primary: Sha256::digest(token.as_bytes()).into(),
            replica: Sha256::digest(replica_token(token).as_bytes()).into(),
        }
    }
}

/// What an app process receives for each KV binding in its bootstrap
/// envelope: the binding's token, which its SDK sends with each call over
/// the internal socket.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KvConnection {
    pub token: String,
    #[serde(
        rename = "replicaToken",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub replica_token: Option<String>,
}

impl KvConnection {
    pub fn new(token: &str) -> Self {
        Self {
            token: token.to_owned(),
            replica_token: Some(replica_token(token)),
        }
    }
}

/// The token authorizes the binding, so it is never printed.
impl std::fmt::Debug for KvConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KvConnection")
            .field("token", &"[REDACTED]")
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
    #[serde(
        rename = "replicaAuthToken",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub replica_auth_token: Option<String>,
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

/// Why a restore point was recorded.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotReason {
    /// A full image captured on its own: daily, or when capture started.
    Scheduled,
    /// A full image `tako db backup` asked for.
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

/// A restore point of a database: a full image, or a named transaction in
/// its continuous backup.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DatabaseSnapshot {
    /// The transaction it restores, which `RestoreDatabase` accepts.
    pub id: String,
    pub created_at_unix_secs: u64,
    pub reason: SnapshotReason,
    /// A full image, which restores on its own and is kept beyond the
    /// seven days of continuous history.
    #[serde(default)]
    pub full: bool,
    /// Size of the uploaded, encrypted full image.
    #[serde(default)]
    pub stored_bytes: u64,
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
    /// The newest full image this server captured.
    #[serde(default)]
    pub last_snapshot: Option<DatabaseSnapshot>,
    /// When everything captured so far was last confirmed uploaded.
    #[serde(default)]
    pub replicated_at_unix_secs: Option<u64>,
    #[serde(default)]
    pub last_attempt_unix_secs: Option<u64>,
    #[serde(default)]
    pub last_error: Option<String>,
}

/// A database hosted on a server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DatabaseInfo {
    pub name: String,
    #[serde(default)]
    pub engine: DatabaseEngine,
    /// The random ID chosen when the database was created; copies of one
    /// database on different servers share it.
    #[serde(default)]
    pub generation: String,
    pub created_at_unix_secs: u64,
    pub size_bytes: u64,
    /// Binding scopes on this server; `#channels` denotes server-owned replay.
    pub bindings: Vec<String>,
    /// Remote application or replay service scopes granted the database.
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

/// The reply to `QueryKv`, as JSON: strings (or `{"base64": ...}` for
/// bytes that are not UTF-8), integers, `null`, arrays, `{"error": ...}`,
/// and maps as arrays of `[key, value]` pairs.
pub type KvQueryResult = serde_json::Value;

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
    fn kv_connections_redact_their_token() {
        let connection = KvConnection::new("secret-token");
        assert_eq!(connection.token, "secret-token");
        assert!(!format!("{connection:?}").contains("secret-token"));
    }

    #[test]
    fn sqlite_bindings_serialize_without_an_engine() {
        let binding = DatabaseBinding {
            name: "main".into(),
            generation: None,
            server: "s".into(),
            peer_key: None,
            engine: DatabaseEngine::Sqlite,
        };
        let json = serde_json::to_value(&binding).unwrap();
        assert_eq!(json, serde_json::json!({"name": "main", "server": "s"}));
        let kv = DatabaseBinding {
            engine: DatabaseEngine::Kv,
            ..binding.clone()
        };
        let json = serde_json::to_value(&kv).unwrap();
        assert_eq!(json["engine"], "kv");
        assert!(!binding.same_target(&kv));
        assert_eq!(serde_json::from_value::<DatabaseBinding>(json).unwrap(), kv);
    }

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
        assert!(validate_database_name(CHANNEL_REPLAY_BINDING).is_err());
        assert!(validate_database_name(WORKFLOWS_BINDING).is_err());
    }

    #[test]
    fn runtime_bindings_have_their_own_grant_scopes() {
        assert!(is_runtime_binding(CHANNEL_REPLAY_BINDING));
        assert!(is_runtime_binding(WORKFLOWS_BINDING));
        assert!(!is_runtime_binding("main"));
        assert_eq!(
            runtime_grant_scope(CHANNEL_REPLAY_BINDING, "shop/production").as_deref(),
            Some("shop/production#channels")
        );
        assert_eq!(
            runtime_grant_scope(WORKFLOWS_BINDING, "shop/production").as_deref(),
            Some("shop/production#workflows")
        );
        assert_eq!(runtime_grant_scope("main", "shop/production"), None);
    }

    #[test]
    fn connections_never_print_their_token() {
        let connection = DatabaseConnection {
            url: "http://127.0.0.1:9845".into(),
            replica_auth_token: None,
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
            replica_auth_token: None,
            auth_token: "t".into(),
        })
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({"url": "http://127.0.0.1:9845", "authToken": "t"})
        );
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageClaim {
    pub name: String,
    pub engine: DatabaseEngine,
    pub generation: String,
    pub created_at_unix_secs: u64,
    pub channel_owner: Option<String>,
    pub workflows_owner: Option<String>,
    /// Immutable device IDs in the initial voting membership. A deployment
    /// joining later goes through membership change on this resource.
    pub founders: std::collections::BTreeSet<String>,
}

/// Full-client storage coordination on the network management channel.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum StorageOperation {
    /// The first enrolled server can mint the catalog credential. Other
    /// participants receive the same credential through their management channel.
    SetGrant {
        generation: String,
        scope: String,
        enabled: bool,
    },
    OpenCatalog {
        key: Option<String>,
    },
    Claim {
        candidate: StorageClaim,
    },
    OpenReplica {
        claim: StorageClaim,
        backup_key: String,
    },
    SeedReplica {
        name: String,
        generation: String,
    },
    Readiness {
        group: String,
        target: String,
        barrier: Option<u64>,
    },
    AddMembers {
        group: String,
        targets: Vec<String>,
    },
    Progress {
        group: String,
    },
}

// The operation can contain a catalog credential.
impl std::fmt::Debug for StorageOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SetGrant { scope, .. } => f.debug_tuple("SetGrant").field(scope).finish(),
            Self::OpenCatalog { .. } => f.write_str("OpenCatalog"),
            Self::Claim { candidate } => f.debug_tuple("Claim").field(&candidate.name).finish(),
            Self::SeedReplica { name, .. } => f.debug_tuple("SeedReplica").field(name).finish(),
            Self::Readiness { group, target, .. } => f
                .debug_tuple("Readiness")
                .field(group)
                .field(target)
                .finish(),
            Self::OpenReplica { claim, .. } => {
                f.debug_tuple("OpenReplica").field(&claim.name).finish()
            }
            Self::AddMembers { group, targets } => f
                .debug_tuple("AddMembers")
                .field(group)
                .field(targets)
                .finish(),
            Self::Progress { group } => f.debug_tuple("Progress").field(group).finish(),
        }
    }
}
