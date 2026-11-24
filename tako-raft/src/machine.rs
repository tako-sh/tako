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
                "CREATE TABLE IF NOT EXISTS pages(page INTEGER PRIMARY KEY, bytes BLOB NOT NULL); CREATE TABLE IF NOT EXISTS backup_changes(txid INTEGER PRIMARY KEY,captured INTEGER NOT NULL,change BLOB NOT NULL); CREATE INDEX IF NOT EXISTS backup_expiry ON backup_changes(captured); CREATE TABLE IF NOT EXISTS resource_grants(scope TEXT PRIMARY KEY,key_hash BLOB NOT NULL) WITHOUT ROWID; {}",
                crate::catalog::SCHEMA
            ),
        )?;
        let grants = Arc::new(parking_lot::RwLock::new(disk.read(read_grants)?));
        Ok(Self {
            grants,
            disk,
            dir: Arc::new(directory.to_owned()),
            snapshot: Default::default(),
        })
    }

    pub(crate) fn grant_matches(&self, scope: &str, hash: &[u8; 32]) -> bool {
        use subtle::ConstantTimeEq;
        self.grants
            .read()
            .get(scope)
            .is_some_and(|expected| bool::from(expected.ct_eq(hash)))
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
    pub(crate) async fn tracking(&self) -> io::Result<bool> {
        self.disk
            .run(|db| Ok(get(db, "track_backups")?.unwrap_or(false)))
            .await
    }

    pub(crate) async fn backup_delta(&self, txid: u64) -> io::Result<Option<Commit>> {
        self.disk
            .run(move |db| {
                use rusqlite::OptionalExtension;
                let data: Option<Vec<u8>> = db
                    .query_row(
                        "SELECT change FROM backup_changes WHERE txid=?1",
                        [i64::try_from(txid).map_err(error)?],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(error)?;
                data.map(|d| decode(&d)).transpose()
            })
            .await
    }

    pub(crate) async fn checkout_position(
        &self,
        dir: &Path,
    ) -> io::Result<(NamedTempFile, BackupPosition)> {
        let mut file = NamedTempFile::new_in(dir)?;
        self.disk
            .run(move |db| {
                let tx = db.transaction().map_err(error)?;
                let revision = get(&tx, "revision")?.unwrap_or(0);
                let size = get(&tx, "file_size")?.unwrap_or(0);
                let page_size: u32 = get(&tx, "page_size")?.unwrap_or(0);
                file.as_file().set_len(size)?;
                let mut stmt = tx
                    .prepare("SELECT page,bytes FROM pages ORDER BY page")
                    .map_err(error)?;
                let mut rows = stmt.query([]).map_err(error)?;
                while let Some(row) = rows.next().map_err(error)? {
                    let page =
                        u64::try_from(row.get::<_, i64>(0).map_err(error)?).map_err(error)?;
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
    }
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
        db.execute(
            "INSERT INTO backup_changes VALUES (?1,?2,?3)",
            (
                i64::try_from(txid).map_err(error)?,
                commit.captured_at_ms,
                encode(&commit)?,
            ),
        )
        .map_err(error)?;
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
            let result = self
                .disk
                .run(move |db| {
                    let tx = db.transaction().map_err(error)?;
                    let id = entry.log_id;
                    let mut grant_change = None;
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
                        EntryPayload::Normal(crate::Proposal::Pages(change)) => {
                            apply_commit(&tx, change, id.index())?
                        }
                        EntryPayload::Normal(crate::Proposal::TrackBackups(enabled)) => {
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
                let backup = Backup::new(&conn, db).map_err(error)?;
                backup
                    .run_to_completion(128, std::time::Duration::ZERO, None)
                    .map_err(error)?;
                *grants.write() = restored_grants;
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
        let meta = self
            .disk
            .run(move |db| {
                let path = target
                    .path()
                    .to_str()
                    .ok_or_else(|| error("non UTF-8 snapshot path"))?;
                db.execute("VACUUM INTO ?1", [path]).map_err(error)?;
                let (last_log_id, last_membership) = applied(db)?;
                Ok(Meta {
                    last_log_id,
                    last_membership,
                })
            })
            .await?;
        *self.snapshot.lock() = Some((meta.clone(), file.clone()));
        Ok(openraft::Snapshot {
            meta,
            snapshot: file,
        })
    }
}
