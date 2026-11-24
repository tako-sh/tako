use crate::{
    Applied, Commit, Types,
    disk::{Disk, error, get, set},
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
    dir: Arc<PathBuf>,
    snapshot: Arc<Mutex<Option<(Meta, SnapshotFile)>>>,
}

fn applied(db: &Connection) -> io::Result<AppliedState> {
    Ok((
        get(db, "applied")?,
        get(db, "membership")?.unwrap_or_default(),
    ))
}

impl Replica {
    pub fn open(directory: &Path) -> io::Result<Self> {
        let disk = Disk::open(
            &directory.join("replica.sqlite"),
            &format!(
                "CREATE TABLE IF NOT EXISTS pages(page INTEGER PRIMARY KEY, bytes BLOB NOT NULL); {}",
                crate::catalog::SCHEMA
            ),
        )?;
        Ok(Self {
            disk,
            dir: Arc::new(directory.to_owned()),
            snapshot: Default::default(),
        })
    }

    pub async fn revision(&self) -> io::Result<u64> {
        self.disk
            .run(|db| Ok(get(db, "revision")?.unwrap_or(0)))
            .await
    }

    /// Reconstruct a private writer or local reader from one committed
    /// revision. A failed copy discards the partial file with its owner.
    pub async fn checkout(&self, dir: &Path) -> io::Result<(NamedTempFile, u64)> {
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
                Ok((file, revision))
            })
            .await
    }
}

fn apply_commit(db: &Connection, commit: Commit, revision: u64) -> io::Result<Applied> {
    let current_revision = get(db, "revision")?.unwrap_or(0);
    if commit.base_revision != current_revision {
        return Ok(Applied::Stale { current_revision });
    }
    let page_size = u64::from(commit.page_size);
    let existing: Option<u32> = get(db, "page_size")?;
    if !(512..=65536).contains(&page_size)
        || !page_size.is_power_of_two()
        || existing.is_some_and(|size| size != commit.page_size)
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
            let result = self
                .disk
                .run(move |db| {
                    let tx = db.transaction().map_err(error)?;
                    let id = entry.log_id;
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
                        EntryPayload::Normal(crate::Proposal::Claim(candidate)) => {
                            crate::catalog::claim(&tx, candidate)?
                        }
                    };
                    set(&tx, "applied", &id)?;
                    tx.commit().map_err(error)?;
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
                let backup = Backup::new(&conn, db).map_err(error)?;
                backup
                    .run_to_completion(128, std::time::Duration::ZERO, None)
                    .map_err(error)?;
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
