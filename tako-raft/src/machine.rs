use crate::{
    Applied, Commit, Types,
    disk::{Disk, decode, encode, error, get, set},
};
use futures_util::{Stream, TryStreamExt};
use openraft::{
    EntryPayload, OptionalSend, RaftSnapshotBuilder,
    alias::{LogIdOf, SnapshotMetaOf, SnapshotOf, StoredMembershipOf},
    storage::{EntryResponder, RaftStateMachine},
};
use parking_lot::Mutex;
use rusqlite::{Connection, OpenFlags, backup::Backup};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::NamedTempFile;

pub type SnapshotFile = Arc<NamedTempFile>;
type Meta = SnapshotMetaOf<Types>;
type AppliedState = (Option<LogIdOf<Types>>, StoredMembershipOf<Types>);

/// The committed resource bytes and the last applied Raft index share a
/// transaction. A crash cannot advance either one independently of the other.
#[derive(Clone)]
pub struct Replica {
    disk: Disk,
    settings: Arc<parking_lot::RwLock<Option<Vec<u8>>>>,
    grants: Arc<parking_lot::RwLock<std::collections::BTreeMap<String, [u8; 32]>>>,
    dir: Arc<PathBuf>,
    snapshot: Arc<Mutex<Option<(Meta, SnapshotFile)>>>,
}

fn applied(db: &Connection) -> io::Result<AppliedState> {
    Ok((
        get(db, "applied")?,
        get(db, "membership")?.unwrap_or_default(),
    ))
}

fn read_grants(db: &Connection) -> io::Result<std::collections::BTreeMap<String, [u8; 32]>> {
    let mut rows = db
        .prepare("SELECT scope,key_hash FROM resource_grants")
        .map_err(error)?;
    rows.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(error)?
        .collect::<rusqlite::Result<_>>()
        .map_err(error)
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BackupPosition {
    pub revision: u64,
    pub txid: u64,
    pub captured_at_ms: i64,
    pub page_size: u32,
}

impl Replica {
    pub fn open(directory: &Path) -> io::Result<Self> {
        let disk = Disk::open(
            &directory.join("replica.sqlite"),
            &format!(
                "CREATE TABLE IF NOT EXISTS staged_parts(part INTEGER PRIMARY KEY, nonce BLOB NOT NULL, bytes BLOB NOT NULL); CREATE TABLE IF NOT EXISTS pages(page INTEGER PRIMARY KEY, bytes BLOB NOT NULL); CREATE TABLE IF NOT EXISTS backup_changes(txid INTEGER NOT NULL,part INTEGER NOT NULL,captured INTEGER NOT NULL,change BLOB NOT NULL,PRIMARY KEY(txid,part)) WITHOUT ROWID; CREATE INDEX IF NOT EXISTS backup_expiry ON backup_changes(captured); CREATE TABLE IF NOT EXISTS resource_grants(scope TEXT PRIMARY KEY,key_hash BLOB NOT NULL) WITHOUT ROWID; {}",
                crate::catalog::SCHEMA
            ),
        )?;
        let settings = Arc::new(parking_lot::RwLock::new(
            disk.read(|db| get(db, "backup_settings"))?,
        ));
        let grants = Arc::new(parking_lot::RwLock::new(disk.read(read_grants)?));
        Ok(Self {
            grants,
            settings,
            disk,
            dir: Arc::new(directory.to_owned()),
            snapshot: Default::default(),
        })
    }

    pub(crate) fn has_grant(&self, scope: &str) -> bool {
        self.grants.read().contains_key(scope)
    }

    pub(crate) fn grant_matches(&self, scope: &str, hash: &[u8; 32]) -> bool {
        use subtle::ConstantTimeEq;
        self.grants
            .read()
            .get(scope)
            .is_some_and(|expected| bool::from(expected.ct_eq(hash)))
    }

    pub async fn retirement(
        &self,
        generation: String,
    ) -> io::Result<Option<std::collections::BTreeSet<String>>> {
        self.disk
            .run(move |db| crate::catalog::retirement(db, &generation))
            .await
    }

    pub async fn placement(&self) -> io::Result<tako_core::ResourcePlacement> {
        self.disk.run(|db| crate::placement::load(db)).await
    }

    pub async fn revision(&self) -> io::Result<u64> {
        self.disk
            .run(|db| Ok(get(db, "revision")?.unwrap_or(0)))
            .await
    }

    /// Reconstruct a private writer or local reader from one committed
    /// revision. A failed copy discards the partial file with its owner.
    pub async fn checkout(&self, dir: &Path) -> io::Result<(NamedTempFile, u64)> {
        let (file, pos) = self.checkout_position(dir).await?;
        Ok((file, pos.revision))
    }
    pub(crate) async fn transaction_id(&self) -> io::Result<u64> {
        self.disk.run(|db| Ok(get(db, "txid")?.unwrap_or(0))).await
    }
    pub(crate) fn backup_settings(&self) -> Option<Vec<u8>> {
        self.settings.read().clone()
    }

    pub(crate) async fn backup_delta(&self, txid: u64) -> io::Result<Option<Commit>> {
        self.disk
            .run(move |db| {
                let mut encoded = Vec::new();
                let mut statement = db
                    .prepare("SELECT change FROM backup_changes WHERE txid=?1 ORDER BY part")
                    .map_err(error)?;
                let mut rows = statement
                    .query([i64::try_from(txid).map_err(error)?])
                    .map_err(error)?;
                while let Some(row) = rows.next().map_err(error)? {
                    encoded.extend_from_slice(
                        row.get_ref(0).map_err(error)?.as_blob().map_err(error)?,
                    );
                }
                if encoded.is_empty() {
                    Ok(None)
                } else {
                    decode(&encoded).map(Some)
                }
            })
            .await
    }

    pub(crate) async fn checkout_position(
        &self,
        dir: &Path,
    ) -> io::Result<(NamedTempFile, BackupPosition)> {
        let mut file = NamedTempFile::new_in(dir)?;
        let path = self.dir.join("replica.sqlite");
        // A read transaction on a separate WAL connection pins this revision
        // while the Raft apply connection continues to accept new writes.
        tokio::task::spawn_blocking(move || {
            let mut reader =
                Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .map_err(error)?;
            let tx = reader.transaction().map_err(error)?;
            let revision = get(&tx, "revision")?.unwrap_or(0);
            let size = get(&tx, "file_size")?.unwrap_or(0);
            let page_size: u32 = get(&tx, "page_size")?.unwrap_or(0);
            file.as_file().set_len(size)?;
            let mut stmt = tx
                .prepare("SELECT page,bytes FROM pages ORDER BY page")
                .map_err(error)?;
            let mut rows = stmt.query([]).map_err(error)?;
            while let Some(row) = rows.next().map_err(error)? {
                let page = u64::try_from(row.get::<_, i64>(0).map_err(error)?).map_err(error)?;
                file.seek(SeekFrom::Start(page * u64::from(page_size)))?;
                file.write_all(row.get_ref(1).map_err(error)?.as_blob().map_err(error)?)?;
            }
            file.rewind()?;
            Ok((
                file,
                BackupPosition {
                    revision,
                    page_size,
                    txid: get(&tx, "txid")?.unwrap_or(0),
                    captured_at_ms: get(&tx, "captured_at_ms")?.unwrap_or(0),
                },
            ))
        })
        .await
        .map_err(error)?
    }
}

// Only the final log entry publishes a transaction. A new writer can supersede an
// interrupted upload at the same revision. A displaced writer cannot finish it.
fn apply_part(
    db: &Connection,
    base: u64,
    nonce: &[u8; 16],
    index: u64,
    last: bool,
    bytes: Vec<u8>,
    revision: u64,
) -> io::Result<Applied> {
    let current_revision = get(db, "revision")?.unwrap_or(0);
    if current_revision != base {
        return Ok(Applied::Stale { current_revision });
    }
    let Ok(index) = i64::try_from(index) else {
        return Ok(Applied::Invalid);
    };
    if index == 0 {
        db.execute("DELETE FROM staged_parts", []).map_err(error)?;
    } else {
        let next: Option<i64> = db
            .query_row(
                "SELECT max(part)+1 FROM staged_parts WHERE nonce=?1",
                [nonce],
                |row| row.get(0),
            )
            .map_err(error)?;
        if next != Some(index) {
            return Ok(Applied::Invalid);
        }
    }
    db.execute(
        "INSERT INTO staged_parts VALUES(?1,?2,?3)",
        (index, nonce, bytes),
    )
    .map_err(error)?;
    if !last {
        return Ok(Applied::NoChange);
    }
    let mut encoded = Vec::new();
    {
        let mut stmt = db
            .prepare("SELECT bytes FROM staged_parts ORDER BY part")
            .map_err(error)?;
        let mut rows = stmt.query([]).map_err(error)?;
        while let Some(row) = rows.next().map_err(error)? {
            encoded.extend_from_slice(row.get_ref(0).map_err(error)?.as_blob().map_err(error)?);
        }
    }
    let Ok(commit) = postcard::from_bytes::<Commit>(&encoded) else {
        return Ok(Applied::Invalid);
    };
    if commit.base_revision != base {
        return Ok(Applied::Invalid);
    }
    apply_commit(db, commit, revision)
}

fn apply_commit(db: &Connection, mut commit: Commit, revision: u64) -> io::Result<Applied> {
    let current_revision = get(db, "revision")?.unwrap_or(0);
    if commit.base_revision != current_revision {
        return Ok(Applied::Stale { current_revision });
    }
    let page_size = u64::from(commit.page_size);
    let existing: Option<u32> = get(db, "page_size")?;
    if !(512..=65536).contains(&page_size)
        || !page_size.is_power_of_two()
        || (!commit.replace && existing.is_some_and(|size| size != commit.page_size))
        || commit.file_size > i64::MAX as u64
        || !commit.file_size.is_multiple_of(page_size)
    {
        return Ok(Applied::Invalid);
    }
    let page_count = commit.file_size / page_size;
    if commit
        .pages
        .iter()
        .any(|(&index, bytes)| index >= page_count || bytes.len() != page_size as usize)
    {
        return Ok(Applied::Invalid);
    }
    db.execute("DELETE FROM staged_parts", []).map_err(error)?;
    // Raft log indices also include elections and configuration changes.
    // The backup format needs a consecutive transaction number for page commits.
    let txid = get::<u64>(db, "txid")?
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| error("transaction overflow"))?;
    commit.captured_at_ms = commit
        .captured_at_ms
        .max(get::<i64>(db, "captured_at_ms")?.unwrap_or(0));
    set(db, "txid", &txid)?;
    set(db, "captured_at_ms", &commit.captured_at_ms)?;
    if get::<bool>(db, "track_backups")?.unwrap_or(false) {
        let txid = i64::try_from(txid).map_err(error)?;
        let encoded = encode(&commit)?;
        let mut save = db
            .prepare_cached("INSERT INTO backup_changes VALUES (?1,?2,?3,?4)")
            .map_err(error)?;
        for (part, bytes) in encoded.chunks(8 * 1024 * 1024).enumerate() {
            save.execute((txid, part as i64, commit.captured_at_ms, bytes))
                .map_err(error)?;
        }
        let expiry = commit
            .captured_at_ms
            .saturating_sub(7 * 24 * 60 * 60 * 1000);
        db.execute("DELETE FROM backup_changes WHERE captured < ?1", [expiry])
            .map_err(error)?;
    }
    if commit.replace {
        db.execute("DELETE FROM pages", []).map_err(error)?;
    }
    let mut stmt = db
        .prepare_cached("INSERT OR REPLACE INTO pages VALUES (?1,?2)")
        .map_err(error)?;
    for (page, bytes) in commit.pages {
        stmt.execute((page as i64, bytes)).map_err(error)?;
    }
    db.execute("DELETE FROM pages WHERE page >= ?1", [page_count as i64])
        .map_err(error)?;
    set(db, "file_size", &commit.file_size)?;
    set(db, "page_size", &commit.page_size)?;
    set(db, "revision", &revision)?;
    Ok(Applied::Committed(revision))
}

impl RaftStateMachine<Types> for Replica {
    type SnapshotData = SnapshotFile;
    type SnapshotBuilder = Self;

    async fn applied_state(&mut self) -> io::Result<AppliedState> {
        self.disk.run(|db| applied(db)).await
    }
    async fn apply<S>(&mut self, mut entries: S) -> io::Result<()>
    where
        S: Stream<Item = io::Result<EntryResponder<Types>>> + Unpin + OptionalSend,
    {
        while let Some((entry, responder)) = entries.try_next().await? {
            let grants = self.grants.clone();
            let settings = self.settings.clone();
            let result = self
                .disk
                .run(move |db| {
                    let tx = db.transaction().map_err(error)?;
                    let id = entry.log_id;
                    let mut grant_change = None;
                    let mut revoked = Vec::new();
                    let mut new_settings = None;
                    let reply = match entry.payload {
                        EntryPayload::Blank => Applied::NoChange,
                        EntryPayload::Membership(membership) => {
                            set(
                                &tx,
                                "membership",
                                &StoredMembershipOf::<Types>::new(Some(id), membership),
                            )?;
                            Applied::NoChange
                        }
                        EntryPayload::Normal(crate::Proposal::Placement(change)) => {
                            let released_app = match &change {
                                tako_core::PlacementChange::Release { app, .. } => {
                                    Some(app.clone())
                                }
                                _ => None,
                            };
                            let result = crate::placement::apply(&tx, change, id.index())?;
                            if let Applied::Placement(placement) = &result
                                && let Some(ref app) = released_app
                                && !placement.uses.values().any(|apps| apps.contains_key(app))
                            {
                                for scope in [
                                    app.to_owned(),
                                    tako_core::channel_replay_grant_scope(app),
                                    tako_core::workflows_grant_scope(app),
                                ] {
                                    tx.execute(
                                        "DELETE FROM resource_grants WHERE scope=?1",
                                        [&scope],
                                    )
                                    .map_err(error)?;
                                    revoked.push(scope);
                                }
                            }
                            if let Applied::Placement(placement) = &result
                                && placement.retired
                            {
                                set(&tx, "retired", &true)?;
                            }
                            result
                        }
                        EntryPayload::Normal(crate::Proposal::Retirement {
                            generation,
                            targets,
                        }) => crate::catalog::begin_retirement(&tx, &generation, targets)?,
                        EntryPayload::Normal(crate::Proposal::RetireName { name, generation }) => {
                            crate::catalog::retire(&tx, &name, &generation)?
                        }
                        EntryPayload::Normal(_)
                            if get::<bool>(&tx, "retired")?.unwrap_or(false) =>
                        {
                            Applied::Invalid
                        }
                        EntryPayload::Normal(crate::Proposal::CommitPart {
                            base,
                            nonce,
                            index,
                            last,
                            bytes,
                        }) => apply_part(&tx, base, &nonce, index, last, bytes, id.index())?,
                        EntryPayload::Normal(crate::Proposal::Pages(change)) => {
                            apply_commit(&tx, change, id.index())?
                        }
                        EntryPayload::Normal(crate::Proposal::BackupSettings {
                            track: enabled,
                            sealed,
                        }) => {
                            set(&tx, "backup_settings", &sealed)?;
                            new_settings = Some(sealed);
                            set(&tx, "track_backups", &enabled)?;
                            if !enabled {
                                tx.execute("DELETE FROM backup_changes", [])
                                    .map_err(error)?;
                            }
                            Applied::NoChange
                        }
                        EntryPayload::Normal(crate::Proposal::Grant { scope, key_hash }) => {
                            if let Some(hash) = &key_hash {
                                tx.execute(
                                    "INSERT OR REPLACE INTO resource_grants VALUES(?1,?2)",
                                    (&scope, hash),
                                )
                                .map_err(error)?;
                            } else {
                                tx.execute("DELETE FROM resource_grants WHERE scope=?1", [&scope])
                                    .map_err(error)?;
                            }
                            grant_change = Some((scope, key_hash));
                            Applied::NoChange
                        }
                        EntryPayload::Normal(crate::Proposal::Claim(candidate)) => {
                            crate::catalog::claim(&tx, candidate)?
                        }
                    };
                    set(&tx, "applied", &id)?;
                    tx.commit().map_err(error)?;
                    if !revoked.is_empty() {
                        let mut current = grants.write();
                        for scope in revoked {
                            current.remove(&scope);
                        }
                    }
                    if let Some(sealed) = new_settings {
                        *settings.write() = Some(sealed);
                    }
                    if let Some((scope, hash)) = grant_change {
                        let mut current = grants.write();
                        match hash {
                            Some(value) => current.insert(scope, value),
                            None => current.remove(&scope),
                        };
                    }
                    Ok(reply)
                })
                .await?;
            if let Some(responder) = responder {
                responder.send(result);
            }
        }
        Ok(())
    }
    async fn get_snapshot_builder(&mut self) -> Self {
        self.clone()
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> io::Result<Option<SnapshotOf<Types, SnapshotFile>>> {
        if let Some((meta, snapshot)) = self.snapshot.lock().clone() {
            return Ok(Some(openraft::Snapshot { meta, snapshot }));
        }
        if self.applied_state().await?.0.is_none() {
            return Ok(None);
        }
        self.build_snapshot().await.map(Some)
    }

    async fn install_snapshot(&mut self, meta: &Meta, file: SnapshotFile) -> io::Result<()> {
        let expected = meta.clone();
        let source = file.clone();
        let settings = self.settings.clone();
        let grants = self.grants.clone();
        self.disk
            .run(move |db| {
                let conn =
                    Connection::open_with_flags(source.path(), OpenFlags::SQLITE_OPEN_READ_ONLY)
                        .map_err(error)?;
                let (id, membership) = applied(&conn)?;
                if id != expected.last_log_id || membership != expected.last_membership {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "snapshot position does not match Raft metadata",
                    ));
                }
                let restored_grants = read_grants(&conn)?;
                let restored_settings = get(&conn, "backup_settings")?;
                let backup = Backup::new(&conn, db).map_err(error)?;
                backup
                    .run_to_completion(128, std::time::Duration::ZERO, None)
                    .map_err(error)?;
                *grants.write() = restored_grants;
                *settings.write() = restored_settings;
                Ok(())
            })
            .await?;
        *self.snapshot.lock() = Some((meta.clone(), file));
        Ok(())
    }
}

impl RaftSnapshotBuilder<Types> for Replica {
    type SnapshotData = SnapshotFile;

    async fn build_snapshot(&mut self) -> io::Result<SnapshotOf<Types, SnapshotFile>> {
        let file = Arc::new(NamedTempFile::new_in(self.dir.as_ref())?);
        let target = file.clone();
        let source = self.dir.join("replica.sqlite");
        let meta = tokio::task::spawn_blocking(move || {
            let mut reader =
                Connection::open_with_flags(source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .map_err(error)?;
            let tx = reader.transaction().map_err(error)?;
            let (last_log_id, last_membership) = applied(&tx)?;
            let mut dest = Connection::open(target.path()).map_err(error)?;
            let backup = Backup::new(&tx, &mut dest).map_err(error)?;
            backup
                .run_to_completion(128, std::time::Duration::ZERO, None)
                .map_err(error)?;
            Ok::<_, io::Error>(Meta {
                last_log_id,
                last_membership,
            })
        })
        .await
        .map_err(error)??;
        *self.snapshot.lock() = Some((meta.clone(), file.clone()));
        Ok(openraft::Snapshot {
            meta,
            snapshot: file,
        })
    }
}
