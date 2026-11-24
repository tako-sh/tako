//! The runtime journal: one redb file that only the native network agent opens.
//! Every runtime operation spanning several rows is one write transaction,
//! durable (fsynced) when its commit returns.
//!
//! Tables, named after the SQLite tables of the legacy `runtime.db`, plus
//! expiry indexes for deletes without a scan:
//! - `runtime_identity`: `()` → (network JSON, server, last_time, auth_window)
//! - `consumed_nonces`, `revocation_nonces`: (signer, nonce) → expires, each
//!   indexed by `*_nonce_expiry`: (expires, signer, nonce) → ()
//! - `observed_records`, `shared_observed`: (kind, id) → payload
//! - `registry_failure`: `()` → () while the registry latch is set
//! - `pending_entitlements`, `invitations`, `shared_publications`: id →
//!   (expires, payload), each indexed by `*_expiry`: (expires, id) → ()
use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
};

use redb::{
    Database, DatabaseError, Durability, ReadableDatabase, ReadableTable, ReadableTableMetadata,
    TableDefinition, WriteTransaction, backends::FileBackend,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::api::{RuntimeError, RuntimeResult};

const CACHE_BYTES: usize = 16 * 1024 * 1024;

pub(super) const IDENTITY: TableDefinition<(), (&str, &str, u64, u64)> =
    TableDefinition::new("runtime_identity");
pub(super) const OBSERVED_RECORDS: TableDefinition<(&str, &str), &str> =
    TableDefinition::new("observed_records");
pub(super) const REGISTRY_FAILURE: TableDefinition<(), ()> =
    TableDefinition::new("registry_failure");
pub(super) const SHARED_OBSERVED: TableDefinition<(&str, &str), &str> =
    TableDefinition::new("shared_observed");
pub(super) const CONSUMED_NONCES: NoncePool = NoncePool(
    TableDefinition::new("consumed_nonces"),
    TableDefinition::new("consumed_nonce_expiry"),
);
pub(super) const REVOCATION_NONCES: NoncePool = NoncePool(
    TableDefinition::new("revocation_nonces"),
    TableDefinition::new("revocation_nonce_expiry"),
);
pub(super) const PENDING_ENTITLEMENTS: Expiring = Expiring(
    TableDefinition::new("pending_entitlements"),
    TableDefinition::new("pending_entitlement_expiry"),
);
pub(super) const INVITATIONS: Expiring = Expiring(
    TableDefinition::new("invitations"),
    TableDefinition::new("invitation_expiry"),
);
pub(super) const SHARED_PUBLICATIONS: Expiring = Expiring(
    TableDefinition::new("shared_publications"),
    TableDefinition::new("shared_publication_expiry"),
);

type NonceRows = TableDefinition<'static, (&'static str, &'static str), u64>;
type NonceExpiry = TableDefinition<'static, (u64, &'static str, &'static str), ()>;
type Rows = TableDefinition<'static, &'static str, (u64, &'static str)>;
type Expiry = TableDefinition<'static, (u64, &'static str), ()>;

/// Nonces by (signer, nonce), indexed by expiry so expired ones are deleted
/// without a scan.
#[derive(Clone, Copy)]
pub(super) struct NoncePool(pub(super) NonceRows, pub(super) NonceExpiry);

impl NoncePool {
    /// Deletes the nonces that expired before `now`.
    pub(super) fn purge_before(self, txn: &WriteTransaction, now: u64) -> RuntimeResult<()> {
        let mut rows = txn.open_table(self.0)?;
        let mut expiry = txn.open_table(self.1)?;
        for entry in expiry.extract_if(..(now, "", ""), |_, _| true)? {
            let (key, _) = entry?;
            let (_, signer, nonce) = key.value();
            rows.remove((signer, nonce))?;
        }
        Ok(())
    }

    pub(super) fn contains(
        self,
        txn: &WriteTransaction,
        signer: &str,
        nonce: &str,
    ) -> RuntimeResult<bool> {
        Ok(txn.open_table(self.0)?.get((signer, nonce))?.is_some())
    }

    /// The pool's size and how many of its nonces `signer` holds.
    pub(super) fn counts(self, txn: &WriteTransaction, signer: &str) -> RuntimeResult<(u64, u64)> {
        let rows = txn.open_table(self.0)?;
        // A signer's keys start at (signer, "") and end before (signer + "\0", "").
        let end = format!("{signer}\0");
        let mut held = 0;
        for entry in rows.range((signer, "")..(end.as_str(), ""))? {
            entry?;
            held += 1;
        }
        Ok((rows.len()?, held))
    }

    pub(super) fn insert(
        self,
        txn: &WriteTransaction,
        signer: &str,
        nonce: &str,
        expires: u64,
    ) -> RuntimeResult<()> {
        let mut rows = txn.open_table(self.0)?;
        let mut expiry = txn.open_table(self.1)?;
        let previous = rows
            .insert((signer, nonce), expires)?
            .map(|old| old.value());
        if let Some(previous) = previous {
            expiry.remove((previous, signer, nonce))?;
        }
        expiry.insert((expires, signer, nonce), ())?;
        Ok(())
    }

    /// Rows and expiry index entries.
    fn len(self, txn: &impl Tables) -> RuntimeResult<(u64, u64)> {
        Ok((txn.rows(self.0)?, txn.rows(self.1)?))
    }
}

/// Payloads by ID with an expiry fixed at insert, indexed by expiry so
/// expired rows are deleted without a scan.
#[derive(Clone, Copy)]
pub(super) struct Expiring(pub(super) Rows, pub(super) Expiry);

impl Expiring {
    /// A row's expiry and payload.
    pub(super) fn get(self, db: &Database, id: &str) -> RuntimeResult<Option<(u64, String)>> {
        let txn = db.begin_read()?;
        let rows = txn.open_table(self.0)?;
        Ok(rows.get(id)?.map(|row| {
            let (expires, payload) = row.value();
            (expires, payload.to_owned())
        }))
    }

    pub(super) fn count(self, txn: &WriteTransaction) -> RuntimeResult<u64> {
        Ok(txn.open_table(self.0)?.len()?)
    }

    /// Inserts a row, or replaces an existing row's payload and keeps its expiry.
    pub(super) fn put(
        self,
        txn: &WriteTransaction,
        id: &str,
        expires: u64,
        payload: &str,
    ) -> RuntimeResult<()> {
        let mut rows = txn.open_table(self.0)?;
        let kept = rows.get(id)?.map(|row| row.value().0);
        rows.insert(id, (kept.unwrap_or(expires), payload))?;
        if kept.is_none() {
            txn.open_table(self.1)?.insert((expires, id), ())?;
        }
        Ok(())
    }

    /// Replaces the payload of an existing row; does nothing when it is absent.
    pub(super) fn update(
        self,
        txn: &WriteTransaction,
        id: &str,
        payload: &str,
    ) -> RuntimeResult<()> {
        let mut rows = txn.open_table(self.0)?;
        let kept = rows.get(id)?.map(|row| row.value().0);
        if let Some(expires) = kept {
            rows.insert(id, (expires, payload))?;
        }
        Ok(())
    }

    /// Deletes the rows that expire at or before `now`.
    pub(super) fn purge_through(self, txn: &WriteTransaction, now: u64) -> RuntimeResult<()> {
        let mut rows = txn.open_table(self.0)?;
        let mut expiry = txn.open_table(self.1)?;
        for entry in expiry.extract_if(..(now + 1, ""), |_, _| true)? {
            let (key, _) = entry?;
            rows.remove(key.value().1)?;
        }
        Ok(())
    }

    fn len(self, txn: &impl Tables) -> RuntimeResult<(u64, u64)> {
        Ok((txn.rows(self.0)?, txn.rows(self.1)?))
    }
}

/// Row counts of read and write transactions alike.
trait Tables {
    fn rows<K: redb::Key + 'static, V: redb::Value + 'static>(
        &self,
        table: TableDefinition<K, V>,
    ) -> RuntimeResult<u64>;
}
impl Tables for redb::ReadTransaction {
    fn rows<K: redb::Key + 'static, V: redb::Value + 'static>(
        &self,
        table: TableDefinition<K, V>,
    ) -> RuntimeResult<u64> {
        Ok(self.open_table(table)?.len()?)
    }
}

/// Starts a write transaction that commits durably and records the allocator
/// state, so a crash never needs a full repair and a read-only open (the
/// controller's verification) still works after one.
pub(super) fn write(db: &Database) -> RuntimeResult<WriteTransaction> {
    let mut txn = db.begin_write()?;
    txn.set_durability(Durability::Immediate)?;
    txn.set_quick_repair(true);
    Ok(txn)
}

fn builder() -> redb::Builder {
    let mut builder = Database::builder();
    // A single process owns the journal; the default exclusive mode locks the file.
    builder.set_cache_size(CACHE_BYTES);
    builder
}

/// Owner-only, and never through a symlink.
pub(super) fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    }
    options
}

/// What a journal belongs to. Neither its owner nor its auth window changes.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Identity {
    pub network: String,
    pub server: String,
    pub auth_window: u64,
}

/// Opens the journal of `state_path` (see [`super::ServerRuntime::open`]),
/// importing a legacy SQLite journal when that is all there is. The caller
/// holds the runtime lock.
pub(super) fn open(state_path: &Path, identity: &Identity) -> RuntimeResult<Database> {
    let path = state_path.with_extension("redb");
    let legacy = state_path.with_extension("db");
    if !present(&path)? && exists(&legacy)? {
        import(&path, &legacy, identity)?;
    } else if exists(&legacy)? {
        // An import that stopped after moving its journal into place: the
        // legacy file is stale, and nothing may open it again.
        retire(&legacy)?;
    }
    let file = private_options().create(true).truncate(false).open(&path)?;
    if !file.metadata()?.is_file() {
        return Err(RuntimeError::Invalid(
            "runtime database is not a regular file",
        ));
    }
    let db = builder().create_with_backend(FileBackend::new(file))?;
    let txn = write(&db)?;
    create_tables(&txn)?;
    {
        let mut table = txn.open_table(IDENTITY)?;
        let matches = table.get(())?.map(|row| {
            let (network, server, _, auth_window) = row.value();
            network == identity.network
                && server == identity.server
                && auth_window == identity.auth_window
        });
        match matches {
            Some(true) => {}
            Some(false) => return Err(RuntimeError::Conflict("local runtime identity")),
            None => {
                table.insert(
                    (),
                    (
                        identity.network.as_str(),
                        identity.server.as_str(),
                        0,
                        identity.auth_window,
                    ),
                )?;
            }
        }
    }
    txn.commit()?;
    Ok(db)
}

fn create_tables(txn: &WriteTransaction) -> RuntimeResult<()> {
    txn.open_table(IDENTITY)?;
    txn.open_table(OBSERVED_RECORDS)?;
    txn.open_table(REGISTRY_FAILURE)?;
    txn.open_table(SHARED_OBSERVED)?;
    for pool in [CONSUMED_NONCES, REVOCATION_NONCES] {
        txn.open_table(pool.0)?;
        txn.open_table(pool.1)?;
    }
    for table in [PENDING_ENTITLEMENTS, INVITATIONS, SHARED_PUBLICATIONS] {
        txn.open_table(table.0)?;
        txn.open_table(table.1)?;
    }
    Ok(())
}

/// An empty file is the placeholder the controller creates for a first
/// bootstrap, never a journal.
fn present(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(!meta.is_file() || meta.len() > 0),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(suffix);
    name.into()
}

fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    File::open(parent)?.sync_all()
}

fn legacy_time(time: i64) -> RuntimeResult<u64> {
    u64::try_from(time).map_err(|_| RuntimeError::Invalid("negative legacy journal time"))
}

/// Rows copied from each legacy table.
#[derive(Default)]
struct Copied {
    consumed_nonces: u64,
    revocation_nonces: u64,
    observed_records: u64,
    registry_failure: u64,
    pending_entitlements: u64,
    invitations: u64,
    shared_observed: u64,
    shared_publications: u64,
}

/// One-time upgrade from `runtime.db`: the identity anchors the monotonic
/// clock and consumed nonces must stay replays, so nothing may be dropped.
/// Every row is copied in one write transaction into a temporary file, which
/// is synced and renamed into place only once it is complete and checked. The
/// legacy files are then kept as `runtime.db.migrated*`.
fn import(path: &Path, legacy: &Path, identity: &Identity) -> RuntimeResult<()> {
    if !fs::symlink_metadata(legacy)?.is_file() {
        return Err(RuntimeError::Invalid(
            "legacy runtime database is not a regular file",
        ));
    }
    let source = Connection::open_with_flags(
        legacy,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    // The identity row precedes every other row, so a journal without any row
    // was left by a first bootstrap that never finished: nothing to keep.
    if blank(&source)? {
        drop(source);
        return retire(legacy);
    }
    let temporary = path.with_extension("redb-import");
    // Left by a crash during an earlier import; it never replaced anything.
    remove_if_exists(&temporary)?;
    let file = private_options().create_new(true).open(&temporary)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    let result = (|| -> RuntimeResult<()> {
        let db = builder().create_with_backend(FileBackend::new(file.try_clone()?))?;
        let txn = write(&db)?;
        create_tables(&txn)?;
        let (stored, copied) = copy_legacy(&source, &txn, identity)?;
        txn.commit()?;
        check_import(&db, &stored, &copied)?;
        db.close()?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        sync_parent(path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    drop(source);
    retire(legacy)
}

/// Keeps the legacy files for an operator, under names nothing opens.
fn retire(legacy: &Path) -> RuntimeResult<()> {
    // Sidecars first: `runtime.db.migrated` only appears with its WAL beside it.
    let migrated = legacy.with_extension("db.migrated");
    for suffix in ["-wal", "-shm"] {
        let from = with_suffix(legacy, suffix);
        let to = with_suffix(&migrated, suffix);
        remove_if_exists(&to)?;
        if exists(&from)? {
            fs::rename(&from, &to)?;
        }
    }
    fs::rename(legacy, &migrated)?;
    sync_parent(legacy)?;
    Ok(())
}

fn blank(source: &Connection) -> RuntimeResult<bool> {
    let tables: Vec<String> = source
        .prepare("SELECT name FROM sqlite_master WHERE type='table'")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    for table in tables {
        let quoted = table.replace('"', "\"\"");
        let rows: bool = source.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM \"{quoted}\")"),
            [],
            |row| row.get(0),
        )?;
        if rows {
            return Ok(false);
        }
    }
    Ok(true)
}

type StoredIdentity = (String, String, u64, u64);

/// Copies every row of a legacy table. Tables an older agent never created
/// are empty; only the identity is required.
fn each_row(
    source: &Connection,
    tables: &BTreeSet<String>,
    name: &str,
    columns: &str,
    mut copy: impl FnMut(&rusqlite::Row<'_>) -> RuntimeResult<()>,
) -> RuntimeResult<u64> {
    if !tables.contains(name) {
        return Ok(0);
    }
    let mut statement = source.prepare(&format!("SELECT {columns} FROM {name}"))?;
    let mut rows = statement.query([])?;
    let mut count = 0;
    while let Some(row) = rows.next()? {
        copy(row)?;
        count += 1;
    }
    Ok(count)
}

fn copy_legacy(
    source: &Connection,
    txn: &WriteTransaction,
    identity: &Identity,
) -> RuntimeResult<(StoredIdentity, Copied)> {
    let tables: BTreeSet<String> = source
        .prepare("SELECT name FROM sqlite_master WHERE type='table'")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    let (network, server, last_time, auth_window): (String, String, i64, i64) = source
        .query_row(
            "SELECT network, server, last_time, auth_window FROM runtime_identity WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?
        .ok_or(RuntimeError::Invalid("legacy runtime identity missing"))?;
    let stored = (
        network,
        server,
        legacy_time(last_time)?,
        legacy_time(auth_window)?,
    );
    if stored.0 != identity.network
        || stored.1 != identity.server
        || stored.3 != identity.auth_window
    {
        return Err(RuntimeError::Conflict("local runtime identity"));
    }
    txn.open_table(IDENTITY)?.insert(
        (),
        (stored.0.as_str(), stored.1.as_str(), stored.2, stored.3),
    )?;
    let mut copied = Copied::default();
    for (name, pool, count) in [
        (
            "consumed_nonces",
            CONSUMED_NONCES,
            &mut copied.consumed_nonces,
        ),
        (
            "revocation_nonces",
            REVOCATION_NONCES,
            &mut copied.revocation_nonces,
        ),
    ] {
        *count = each_row(
            source,
            &tables,
            name,
            "signer, nonce, expires",
            |row: &rusqlite::Row<'_>| {
                let signer: String = row.get(0)?;
                let nonce: String = row.get(1)?;
                pool.insert(txn, &signer, &nonce, legacy_time(row.get(2)?)?)
            },
        )?;
    }
    for (name, definition, count) in [
        (
            "observed_records",
            OBSERVED_RECORDS,
            &mut copied.observed_records,
        ),
        (
            "shared_observed",
            SHARED_OBSERVED,
            &mut copied.shared_observed,
        ),
    ] {
        let mut table = txn.open_table(definition)?;
        *count = each_row(
            source,
            &tables,
            name,
            "kind, id, payload",
            |row: &rusqlite::Row<'_>| {
                let (kind, id, payload): (String, String, String) =
                    (row.get(0)?, row.get(1)?, row.get(2)?);
                table.insert((kind.as_str(), id.as_str()), payload.as_str())?;
                Ok(())
            },
        )?;
    }
    copied.registry_failure = each_row(
        source,
        &tables,
        "registry_failure",
        "singleton",
        |_: &rusqlite::Row<'_>| {
            txn.open_table(REGISTRY_FAILURE)?.insert((), ())?;
            Ok(())
        },
    )?
    .min(1);
    for (name, rows, count) in [
        (
            "pending_entitlements",
            PENDING_ENTITLEMENTS,
            &mut copied.pending_entitlements,
        ),
        ("invitations", INVITATIONS, &mut copied.invitations),
        (
            "shared_publications",
            SHARED_PUBLICATIONS,
            &mut copied.shared_publications,
        ),
    ] {
        *count = each_row(
            source,
            &tables,
            name,
            "id, expires, payload",
            |row: &rusqlite::Row<'_>| {
                let (id, expires, payload): (String, i64, String) =
                    (row.get(0)?, row.get(1)?, row.get(2)?);
                rows.put(txn, &id, legacy_time(expires)?, &payload)
            },
        )?;
    }
    Ok((stored, copied))
}

/// Reads the committed import back before it may replace anything.
fn check_import(db: &Database, stored: &StoredIdentity, copied: &Copied) -> RuntimeResult<()> {
    let txn = db.begin_read()?;
    let identity = txn.open_table(IDENTITY)?.get(())?.map(|row| {
        let (network, server, last_time, auth_window) = row.value();
        (
            network.to_owned(),
            server.to_owned(),
            last_time,
            auth_window,
        )
    });
    let both = |count: u64| (count, count);
    if identity.as_ref() != Some(stored)
        || CONSUMED_NONCES.len(&txn)? != both(copied.consumed_nonces)
        || REVOCATION_NONCES.len(&txn)? != both(copied.revocation_nonces)
        || txn.rows(OBSERVED_RECORDS)? != copied.observed_records
        || txn.rows(REGISTRY_FAILURE)? != copied.registry_failure
        || PENDING_ENTITLEMENTS.len(&txn)? != both(copied.pending_entitlements)
        || INVITATIONS.len(&txn)? != both(copied.invitations)
        || txn.rows(SHARED_OBSERVED)? != copied.shared_observed
        || SHARED_PUBLICATIONS.len(&txn)? != both(copied.shared_publications)
    {
        return Err(RuntimeError::Invalid("runtime journal import incomplete"));
    }
    Ok(())
}

/// Row counts read from a journal without opening it for writing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JournalSummary {
    pub consumed_nonces: u64,
    pub revocation_nonces: u64,
}

/// For a node that already completed bootstrap, whose journal must not be
/// recreated: checks that the journal [`super::ServerRuntime::open`] would use
/// for `state_path` holds its identity and nonce tables. Creates and imports
/// nothing. A legacy SQLite journal not yet imported is checked in place. Call
/// it only while no runtime has the journal open.
pub fn verify_journal(state_path: &Path) -> RuntimeResult<JournalSummary> {
    let path = state_path.with_extension("redb");
    let legacy = state_path.with_extension("db");
    if present(&path)? {
        return match builder().open_read_only(&path) {
            Ok(db) => summarize(&db),
            // Every journal commit records the allocator state, so only a
            // journal interrupted before its first commit lands here. A
            // writable open repairs it; it never initializes a file.
            Err(DatabaseError::RepairAborted) => summarize(&builder().open(&path)?),
            Err(error) => Err(error.into()),
        };
    }
    if exists(&legacy)? {
        return verify_legacy(&legacy);
    }
    Err(RuntimeError::Invalid("runtime journal missing"))
}

fn summarize(db: &impl ReadableDatabase) -> RuntimeResult<JournalSummary> {
    let txn = db.begin_read()?;
    let identity = match txn.open_table(IDENTITY) {
        Ok(table) => table.get(())?.is_some(),
        Err(redb::TableError::TableDoesNotExist(_)) => false,
        Err(error) => return Err(error.into()),
    };
    if !identity {
        return Err(RuntimeError::Invalid("runtime identity missing"));
    }
    let count = |pool: NoncePool| -> RuntimeResult<u64> {
        match txn.open_table(pool.0) {
            Ok(table) => Ok(table.len()?),
            Err(redb::TableError::TableDoesNotExist(_)) => {
                Err(RuntimeError::Invalid("nonce journal missing"))
            }
            Err(error) => Err(error.into()),
        }
    };
    Ok(JournalSummary {
        consumed_nonces: count(CONSUMED_NONCES)?,
        revocation_nonces: count(REVOCATION_NONCES)?,
    })
}

fn verify_legacy(path: &Path) -> RuntimeResult<JournalSummary> {
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    let identity: i64 = db
        .query_row(
            "SELECT count(*) FROM runtime_identity WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .map_err(|_| RuntimeError::Invalid("runtime identity missing"))?;
    if identity != 1 {
        return Err(RuntimeError::Invalid("runtime identity missing"));
    }
    let count = |table: &str| -> RuntimeResult<u64> {
        let count: i64 = db
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .map_err(|_| RuntimeError::Invalid("nonce journal missing"))?;
        Ok(count.max(0) as u64)
    };
    let revocations: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='revocation_nonces')",
        [],
        |row| row.get(0),
    )?;
    Ok(JournalSummary {
        consumed_nonces: count("consumed_nonces")?,
        revocation_nonces: if revocations {
            count("revocation_nonces")?
        } else {
            0
        },
    })
}
