//! The runtime journal: one redb file that only the native network agent opens.
//! Every runtime operation spanning several rows is one write transaction,
//! durable (fsynced) when its commit returns.
//!
//! Tables and expiry indexes:
//! - `runtime_identity`: `()` → (network JSON, server, last_time, auth_window)
//! - `consumed_nonces`, `revocation_nonces`: (signer, nonce) → expires, each
//!   indexed by `*_nonce_expiry`: (expires, signer, nonce) → ()
//! - `observed_records`, `shared_observed`: (kind, id) → payload
//! - `registry_failure`: `()` → () while the registry latch is set
//! - `pending_entitlements`, `invitations`, `shared_publications`: id →
//!   (expires, payload), each indexed by `*_expiry`: (expires, id) → ()
use std::{
    fs::{self, OpenOptions},
    io,
    path::Path,
};

use redb::{
    Database, DatabaseError, Durability, ReadableDatabase, ReadableTable, ReadableTableMetadata,
    TableDefinition, WriteTransaction, backends::FileBackend,
};

use crate::api::{RuntimeError, RuntimeResult};

const CACHE_BYTES: usize = 16 * 1024 * 1024;

pub(super) const IDENTITY: TableDefinition<(), (&str, &str, u64, u64)> =
    TableDefinition::new("runtime_identity");
pub(super) const OBSERVED_RECORDS: TableDefinition<(&str, &str), &str> =
    TableDefinition::new("observed_records");
pub(super) const REGISTRY_FAILURE: TableDefinition<(), ()> =
    TableDefinition::new("registry_failure");
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
/// creating its tables on first bootstrap. The caller
/// holds the runtime lock.
pub(super) fn open(state_path: &Path, identity: &Identity) -> RuntimeResult<Database> {
    let path = state_path.with_extension("redb");
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

/// Row counts read from a journal without opening it for writing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JournalSummary {
    pub consumed_nonces: u64,
    pub revocation_nonces: u64,
}

/// For a node that already completed bootstrap, whose journal must not be
/// recreated: checks that the journal [`super::ServerRuntime::open`] would use
/// for `state_path` holds its identity and nonce tables. Creates and imports
/// nothing. Call
/// it only while no runtime has the journal open.
pub fn verify_journal(state_path: &Path) -> RuntimeResult<JournalSummary> {
    let path = state_path.with_extension("redb");
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
