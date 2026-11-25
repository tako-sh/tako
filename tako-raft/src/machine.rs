use crate::{
    Applied, Commit, Types,
    disk::{Disk, META, RecordReader, Records, decode, encode, error, get, set},
};
use futures_util::{Stream, TryStreamExt};
use openraft::{
    EntryPayload, OptionalSend, RaftSnapshotBuilder,
    alias::{LogIdOf, SnapshotMetaOf, SnapshotOf, StoredMembershipOf},
    storage::{EntryResponder, RaftStateMachine},
};
use parking_lot::Mutex;
use redb::{
    Database, Key, ReadTransaction, ReadableDatabase, ReadableTable, TableDefinition, Value,
    WriteTransaction,
};
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

const PAGES: TableDefinition<u64, &[u8]> = TableDefinition::new("pages");
const STAGED: TableDefinition<u64, &[u8]> = TableDefinition::new("staged_parts");
const GRANTS: Records = TableDefinition::new("grants");
const CHANGES: TableDefinition<(u64, u64), &[u8]> = TableDefinition::new("backup_changes");
const CAPTURED: TableDefinition<u64, i64> = TableDefinition::new("backup_timestamps");

// A pinned source transaction covers every table, including staged commits
// and catalog fences. Installation replaces all tables in one durable commit.
fn copy_table<K: Key, V: Value>(
    table: TableDefinition<K, V>,
    source: Option<&ReadTransaction>,
    dest: &WriteTransaction,
) -> io::Result<()> {
    if let Some(source) = source {
        let input = source.open_table(table).map_err(error)?;
        dest.delete_table(table).map_err(error)?;
        let mut output = dest.open_table(table).map_err(error)?;
        for row in input.iter().map_err(error)? {
            let (key, value) = row.map_err(error)?;
            output.insert(key.value(), value.value()).map_err(error)?;
        }
    } else {
        dest.open_table(table).map_err(error)?;
    }
    Ok(())
}
fn replica_tables(source: Option<&ReadTransaction>, dest: &WriteTransaction) -> io::Result<()> {
    for table in [
        META,
        GRANTS,
        crate::catalog::NAMES,
        crate::catalog::GENERATIONS,
        crate::catalog::RETIREMENTS,
    ] {
        copy_table(table, source, dest)?;
    }
    copy_table(PAGES, source, dest)?;
    copy_table(STAGED, source, dest)?;
    copy_table(CHANGES, source, dest)?;
    copy_table(CAPTURED, source, dest)
}

fn applied(db: &impl RecordReader) -> io::Result<AppliedState> {
    Ok((
        get(db, "applied")?,
        get(db, "membership")?.unwrap_or_default(),
    ))
}

fn read_grants(db: &ReadTransaction) -> io::Result<std::collections::BTreeMap<String, [u8; 32]>> {
    db.open_table(GRANTS)
        .map_err(error)?
        .iter()
        .map_err(error)?
        .map(|row| {
            let (scope, hash) = row.map_err(error)?;
            Ok((
                scope.value().to_owned(),
                hash.value().try_into().map_err(error)?,
            ))
        })
        .collect()
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
        let disk = Disk::open(&directory.join("replica.redb"), |tx| {
            replica_tables(None, tx)
        })?;
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

    pub(crate) fn grant_scopes(&self) -> Vec<String> {
        self.grants.read().keys().cloned().collect()
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
            .query(move |db| crate::catalog::retirement(db, &generation))
            .await
    }

    pub async fn placement(&self) -> io::Result<tako_core::ResourcePlacement> {
        self.disk.query(|db| crate::placement::load(db)).await
    }

    pub(crate) fn file_size(&self) -> io::Result<u64> {
        self.disk.read(|db| Ok(get(db, "file_size")?.unwrap_or(0)))
    }
    pub async fn revision(&self) -> io::Result<u64> {
        self.disk
            .query(|db| Ok(get(db, "revision")?.unwrap_or(0)))
            .await
    }

    /// Reconstruct a private writer or local reader from one committed
    /// revision. A failed copy discards the partial file with its owner.
    pub async fn checkout(&self, dir: &Path) -> io::Result<(NamedTempFile, u64)> {
        let (file, pos) = self.checkout_position(dir).await?;
        Ok((file, pos.revision))
    }
    pub(crate) async fn transaction_id(&self) -> io::Result<u64> {
        self.disk
            .query(|db| Ok(get(db, "txid")?.unwrap_or(0)))
            .await
    }
    pub(crate) fn backup_settings(&self) -> Option<Vec<u8>> {
        self.settings.read().clone()
    }

    pub(crate) async fn backup_delta(&self, txid: u64) -> io::Result<Option<Commit>> {
        self.disk
            .query(move |tx| {
                let mut encoded = Vec::new();
                let changes = tx.open_table(CHANGES).map_err(error)?;
                for row in changes.range((txid, 0)..=(txid, u64::MAX)).map_err(error)? {
                    let (_, part) = row.map_err(error)?;
                    encoded.extend_from_slice(part.value());
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
        self.disk
            .query(move |tx| {
                let revision = get(tx, "revision")?.unwrap_or(0);
                let size = get(tx, "file_size")?.unwrap_or(0);
                let page_size: u32 = get(tx, "page_size")?.unwrap_or(0);
                file.as_file().set_len(size)?;
                let pages = tx.open_table(PAGES).map_err(error)?;
                for row in pages.iter().map_err(error)? {
                    let (page, bytes) = row.map_err(error)?;
                    file.seek(SeekFrom::Start(page.value() * u64::from(page_size)))?;
                    file.write_all(bytes.value())?;
                }
                file.rewind()?;
                Ok((
                    file,
                    BackupPosition {
                        revision,
                        page_size,
                        txid: get(tx, "txid")?.unwrap_or(0),
                        captured_at_ms: get(tx, "captured_at_ms")?.unwrap_or(0),
                    },
                ))
            })
            .await
    }
}

// Only the final log entry publishes a transaction. A new writer can supersede an
// interrupted upload at the same revision. A displaced writer cannot finish it.
fn apply_part(
    db: &WriteTransaction,
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
    let mut staged = db.open_table(STAGED).map_err(error)?;
    if index == 0 {
        staged.retain(|_, _| false).map_err(error)?;
        set(db, "stage_nonce", nonce)?;
    } else {
        let next = staged
            .last()
            .map_err(error)?
            .and_then(|(key, _)| key.value().checked_add(1));
        if next != Some(index) || get::<[u8; 16]>(db, "stage_nonce")?.as_ref() != Some(nonce) {
            return Ok(Applied::Invalid);
        }
    }
    staged.insert(index, bytes.as_slice()).map_err(error)?;
    if !last {
        return Ok(Applied::NoChange);
    }
    let mut encoded = Vec::new();
    for row in staged.iter().map_err(error)? {
        let (_, bytes) = row.map_err(error)?;
        encoded.extend_from_slice(bytes.value());
    }
    drop(staged);
    let Ok(commit) = postcard::from_bytes::<Commit>(&encoded) else {
        return Ok(Applied::Invalid);
    };
    if commit.base_revision != base {
        return Ok(Applied::Invalid);
    }
    apply_commit(db, commit, revision)
}

fn apply_commit(db: &WriteTransaction, mut commit: Commit, revision: u64) -> io::Result<Applied> {
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
    db.open_table(STAGED)
        .map_err(error)?
        .retain(|_, _| false)
        .map_err(error)?;
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
        let encoded = encode(&commit)?;
        let mut changes = db.open_table(CHANGES).map_err(error)?;
        let mut captured = db.open_table(CAPTURED).map_err(error)?;
        captured
            .insert(txid, commit.captured_at_ms)
            .map_err(error)?;
        for (part, bytes) in encoded.chunks(8 * 1024 * 1024).enumerate() {
            changes.insert((txid, part as u64), bytes).map_err(error)?;
        }
        let expiry = commit
            .captured_at_ms
            .saturating_sub(7 * 24 * 60 * 60 * 1000);
        // Capture times are monotonic in txid order.
        loop {
            let old = captured
                .first()
                .map_err(error)?
                .and_then(|(id, time)| (time.value() < expiry).then(|| id.value()));
            let Some(old) = old else { break };
            changes
                .retain_in((old, 0)..=(old, u64::MAX), |_, _| false)
                .map_err(error)?;
            captured.remove(old).map_err(error)?;
        }
    }
    let mut pages = db.open_table(PAGES).map_err(error)?;
    if commit.replace {
        pages.retain(|_, _| false).map_err(error)?;
    }
    for (page, bytes) in commit.pages {
        pages.insert(page, bytes.as_slice()).map_err(error)?;
    }
    pages.retain_in(page_count.., |_, _| false).map_err(error)?;
    set(db, "file_size", &commit.file_size)?;
    set(db, "page_size", &commit.page_size)?;
    set(db, "revision", &revision)?;
    Ok(Applied::Committed(revision))
}

impl RaftStateMachine<Types> for Replica {
    type SnapshotData = SnapshotFile;
    type SnapshotBuilder = Self;

    async fn applied_state(&mut self) -> io::Result<AppliedState> {
        self.disk.query(|db| applied(db)).await
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
                .run(move |disk| {
                    let (reply, revoked, new_settings, grant_change) = disk.write(|tx| {
                        let id = entry.log_id;
                        let mut grant_change = None;
                        let mut revoked = Vec::new();
                        let mut new_settings = None;
                        let reply = match entry.payload {
                            EntryPayload::Blank => Applied::NoChange,
                            EntryPayload::Membership(membership) => {
                                set(
                                    tx,
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
                                let result = crate::placement::apply(tx, change, id.index())?;
                                if let Applied::Placement(placement) = &result
                                    && let Some(ref app) = released_app
                                    && !placement.uses.values().any(|apps| apps.contains_key(app))
                                {
                                    for scope in [
                                        app.to_owned(),
                                        tako_core::channel_replay_grant_scope(app),
                                        tako_core::workflows_grant_scope(app),
                                    ] {
                                        tx.open_table(GRANTS)
                                            .map_err(error)?
                                            .remove(scope.as_str())
                                            .map_err(error)?;
                                        revoked.push(scope);
                                    }
                                }
                                if let Applied::Placement(placement) = &result
                                    && placement.retired
                                {
                                    set(tx, "retired", &true)?;
                                }
                                result
                            }
                            EntryPayload::Normal(crate::Proposal::Retirement {
                                generation,
                                targets,
                            }) => crate::catalog::begin_retirement(tx, &generation, targets)?,
                            EntryPayload::Normal(crate::Proposal::RetireName {
                                name,
                                generation,
                            }) => crate::catalog::retire(tx, &name, &generation)?,
                            EntryPayload::Normal(_)
                                if get::<bool>(tx, "retired")?.unwrap_or(false) =>
                            {
                                Applied::Invalid
                            }
                            EntryPayload::Normal(crate::Proposal::CommitPart {
                                base,
                                nonce,
                                index,
                                last,
                                bytes,
                            }) => apply_part(tx, base, &nonce, index, last, bytes, id.index())?,
                            EntryPayload::Normal(crate::Proposal::Pages(change)) => {
                                apply_commit(tx, change, id.index())?
                            }
                            EntryPayload::Normal(crate::Proposal::BackupSettings {
                                track: enabled,
                                sealed,
                            }) => {
                                set(tx, "backup_settings", &sealed)?;
                                new_settings = Some(sealed);
                                set(tx, "track_backups", &enabled)?;
                                if !enabled {
                                    tx.open_table(CHANGES)
                                        .map_err(error)?
                                        .retain(|_, _| false)
                                        .map_err(error)?;
                                    tx.open_table(CAPTURED)
                                        .map_err(error)?
                                        .retain(|_, _| false)
                                        .map_err(error)?;
                                }
                                Applied::NoChange
                            }
                            EntryPayload::Normal(crate::Proposal::Grant { scope, key_hash }) => {
                                let mut grants = tx.open_table(GRANTS).map_err(error)?;
                                if let Some(hash) = &key_hash {
                                    grants
                                        .insert(scope.as_str(), hash.as_slice())
                                        .map_err(error)?;
                                } else {
                                    grants.remove(scope.as_str()).map_err(error)?;
                                }
                                grant_change = Some((scope, key_hash));
                                Applied::NoChange
                            }
                            EntryPayload::Normal(crate::Proposal::Claim(candidate)) => {
                                crate::catalog::claim(tx, candidate)?
                            }
                        };
                        set(tx, "applied", &id)?;
                        Ok((reply, revoked, new_settings, grant_change))
                    })?;
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
            .run(move |disk| {
                let input = Database::builder()
                    .set_cache_size(crate::disk::CACHE_BYTES)
                    .open_read_only(source.path())
                    .map_err(error)?;
                let read = input.begin_read().map_err(error)?;
                let (id, membership) = applied(&read)?;
                if id != expected.last_log_id || membership != expected.last_membership {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "snapshot position does not match Raft metadata",
                    ));
                }
                let restored_grants = read_grants(&read)?;
                let restored_settings = get(&read, "backup_settings")?;
                disk.write(|tx| replica_tables(Some(&read), tx))?;
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
        let meta = self
            .disk
            .query(move |tx| {
                let (last_log_id, last_membership) = applied(tx)?;
                Disk::open(target.path(), |dest| replica_tables(Some(tx), dest))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn incomplete_snapshot_preserves_live_state_and_cached_grants_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let mut live = Replica::open(dir.path()).unwrap();
        live.disk
            .write(|tx| {
                set(tx, "revision", &7_u64)?;
                tx.open_table(GRANTS)
                    .map_err(error)?
                    .insert("orders", &[7; 32][..])
                    .map_err(error)?;
                Ok(())
            })
            .unwrap();
        drop(live);
        live = Replica::open(dir.path()).unwrap();

        let scratch = tempfile::tempdir().unwrap();
        let mut leader = Replica::open(scratch.path()).unwrap();
        leader.disk.write(|tx| set(tx, "revision", &8_u64)).unwrap();
        let snap = leader.build_snapshot().await.unwrap();
        let broken = Database::open(snap.snapshot.path()).unwrap();
        let write = broken.begin_write().unwrap();
        write.delete_table(CAPTURED).unwrap();
        write.commit().unwrap();
        drop(broken);

        assert!(
            live.install_snapshot(&snap.meta, snap.snapshot)
                .await
                .is_err()
        );
        assert_eq!(live.revision().await.unwrap(), 7);
        assert!(live.grant_matches("orders", &[7; 32]));
        drop(live);
        let reopened = Replica::open(dir.path()).unwrap();
        assert_eq!(reopened.revision().await.unwrap(), 7);
        assert!(reopened.grant_matches("orders", &[7; 32]));
    }
}
