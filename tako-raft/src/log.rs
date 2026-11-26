use crate::{
    Types,
    disk::{Disk, META, decode, encode, error, get, set},
};
use openraft::{
    LogState, RaftLogReader,
    alias::{EntryOf, LogIdOf, VoteOf},
    storage::{IOFlushed, RaftLogStorage},
};
use redb::{ReadableTable, TableDefinition};

const ENTRIES: TableDefinition<u64, &[u8]> = TableDefinition::new("entries");
use std::{
    fmt::Debug,
    io,
    ops::{Bound, RangeBounds},
    path::Path,
};

/// Raft's votes, log and truncation markers, independent of state machine snapshots.
#[derive(Clone)]
pub struct LogStore {
    disk: Disk,
    _ownership: std::sync::Arc<std::fs::File>,
}

impl LogStore {
    pub fn open(path: &Path) -> io::Result<Self> {
        let ownership = std::fs::File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path.with_extension("lock"))?;
        ownership.try_lock().map_err(error)?;
        Ok(Self {
            disk: Disk::open(path, |tx| {
                tx.open_table(META).map_err(error)?;
                tx.open_table(ENTRIES).map_err(error)?;
                Ok(())
            })?,
            _ownership: std::sync::Arc::new(ownership),
        })
    }
}

impl RaftLogReader<Types> for LogStore {
    async fn try_get_log_entries<R: RangeBounds<u64> + Clone + Debug + Send>(
        &mut self,
        range: R,
    ) -> io::Result<Vec<EntryOf<Types>>> {
        let start = match range.start_bound() {
            Bound::Included(i) => *i,
            Bound::Excluded(i) => match i.checked_add(1) {
                Some(i) => i,
                None => return Ok(Vec::new()),
            },
            Bound::Unbounded => 0,
        };
        let end = match range.end_bound() {
            Bound::Included(i) => *i,
            Bound::Excluded(i) => match i.checked_sub(1) {
                Some(i) => i,
                None => return Ok(Vec::new()),
            },
            Bound::Unbounded => u64::MAX,
        };
        if start > end {
            return Ok(Vec::new());
        }
        self.disk
            .query(move |tx| {
                let entries = tx.open_table(ENTRIES).map_err(error)?;
                entries
                    .range(start..=end)
                    .map_err(error)?
                    .map(|entry| {
                        let (_, data) = entry.map_err(error)?;
                        decode(data.value())
                    })
                    .collect()
            })
            .await
    }

    async fn read_vote(&mut self) -> io::Result<Option<VoteOf<Types>>> {
        self.disk.query(|db| get(db, "vote")).await
    }
}

impl RaftLogStorage<Types> for LogStore {
    type LogReader = Self;
    async fn get_log_reader(&mut self) -> Self {
        self.clone()
    }

    async fn get_log_state(&mut self) -> io::Result<LogState<Types>> {
        self.disk
            .query(|tx| {
                let purged: Option<LogIdOf<Types>> = get(tx, "purged")?;
                let entries = tx.open_table(ENTRIES).map_err(error)?;
                let last = entries
                    .last()
                    .map_err(error)?
                    .map(|(_, data)| decode::<EntryOf<Types>>(data.value()).map(|e| e.log_id))
                    .transpose()?;
                Ok(LogState {
                    last_log_id: last.or(purged),
                    last_purged_log_id: purged,
                })
            })
            .await
    }

    async fn save_vote(&mut self, vote: &VoteOf<Types>) -> io::Result<()> {
        let vote = *vote;
        self.disk.update(move |tx| set(tx, "vote", &vote)).await
    }
    async fn read_committed(&mut self) -> io::Result<Option<LogIdOf<Types>>> {
        self.disk
            .query(|db| Ok(get(db, "committed")?.flatten()))
            .await
    }
    async fn save_committed(&mut self, committed: Option<LogIdOf<Types>>) -> io::Result<()> {
        self.disk
            .update(move |tx| set(tx, "committed", &committed))
            .await
    }
    async fn append<I>(&mut self, entries: I, callback: IOFlushed<Types>) -> io::Result<()>
    where
        I: IntoIterator<Item = EntryOf<Types>> + Send,
        I::IntoIter: Send,
    {
        let entries: Vec<_> = entries.into_iter().collect();
        let result = self
            .disk
            .update(move |tx| {
                let mut table = tx.open_table(ENTRIES).map_err(error)?;
                for entry in entries {
                    if table
                        .insert(entry.log_id.index(), encode(&entry)?.as_slice())
                        .map_err(error)?
                        .is_some()
                    {
                        return Err(error("duplicate Raft log index"));
                    }
                }
                Ok(())
            })
            .await;
        callback.io_completed(result.as_ref().map(|_| ()).map_err(error));
        result
    }
    async fn truncate_after(&mut self, last: Option<LogIdOf<Types>>) -> io::Result<()> {
        self.disk
            .update(move |tx| {
                let mut entries = tx.open_table(ENTRIES).map_err(error)?;
                if let Some(last) = last {
                    entries
                        .retain_in((Bound::Excluded(last.index()), Bound::Unbounded), |_, _| {
                            false
                        })
                        .map_err(error)?;
                } else {
                    entries.retain(|_, _| false).map_err(error)?;
                }
                Ok(())
            })
            .await
    }
    async fn purge(&mut self, log: LogIdOf<Types>) -> io::Result<()> {
        self.disk
            .update(move |tx| {
                set(tx, "purged", &log)?;
                tx.open_table(ENTRIES)
                    .map_err(error)?
                    .retain_in(..=log.index(), |_, _| false)
                    .map_err(error)
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openraft::testing::{blank_ent, log_id};

    #[tokio::test]
    async fn durable_markers_and_replaced_tail_survive_process_exit() {
        const CHILD_PATH: &str = "TAKO_TEST_REDB_RAFT_CRASH_PATH";
        let vote = VoteOf::<Types>::new_committed(2, 1);
        let first = log_id::<Types>(1, 1, 1);
        let last = log_id::<Types>(2, 1, 3);
        if let Some(path) = std::env::var_os(CHILD_PATH) {
            let mut log = LogStore::open(Path::new(&path)).unwrap();
            log.save_vote(&vote).await.unwrap();
            log.append(
                (1..=3).map(|i| blank_ent::<Types>(1, 1, i)),
                IOFlushed::noop(),
            )
            .await
            .unwrap();
            log.truncate_after(Some(log_id::<Types>(1, 1, 2)))
                .await
                .unwrap();
            log.append([blank_ent::<Types>(2, 1, 3)], IOFlushed::noop())
                .await
                .unwrap();
            log.purge(first).await.unwrap();
            log.save_committed(Some(last)).await.unwrap();
            let _: io::Result<()> = log.disk.write(|tx| {
                tx.open_table(ENTRIES)
                    .map_err(error)?
                    .remove(3)
                    .map_err(error)?;
                set(tx, "purged", &last)?;
                // Skip all destructors with this write transaction still open.
                std::process::exit(37);
            });
            unreachable!();
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raft.redb");
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "log::tests::durable_markers_and_replaced_tail_survive_process_exit",
            ])
            .env(CHILD_PATH, &path)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(37));
        let mut log = LogStore::open(&path).unwrap();
        assert_eq!(log.read_vote().await.unwrap(), Some(vote));
        assert_eq!(log.read_committed().await.unwrap(), Some(last));
        let state = log.get_log_state().await.unwrap();
        assert_eq!(state.last_purged_log_id, Some(first));
        assert_eq!(state.last_log_id, Some(last));
        let entries = log.try_get_log_entries(..).await.unwrap();
        assert_eq!(
            entries.iter().map(|e| e.log_id).collect::<Vec<_>>(),
            [log_id::<Types>(1, 1, 2), last]
        );
    }
}
