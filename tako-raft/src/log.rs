use crate::{
    Types,
    disk::{Disk, decode, encode, error, get, set},
};
use openraft::{
    LogState, RaftLogReader,
    alias::{EntryOf, LogIdOf, VoteOf},
    storage::{IOFlushed, RaftLogStorage},
};
use rusqlite::OptionalExtension;
use std::{
    fmt::Debug,
    io,
    ops::{Bound, RangeBounds},
    path::Path,
};

/// Raft's votes, log and truncation markers, independent of state machine snapshots.
#[derive(Clone)]
pub struct LogStore(Disk);

impl LogStore {
    pub fn open(path: &Path) -> io::Result<Self> {
        Ok(Self(Disk::open(
            path,
            "CREATE TABLE IF NOT EXISTS entries(idx BLOB PRIMARY KEY, data BLOB NOT NULL) WITHOUT ROWID;",
        )?))
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
        self.0
            .run(move |db| {
                let mut stmt = db
                    .prepare("SELECT data FROM entries WHERE idx BETWEEN ?1 AND ?2 ORDER BY idx")
                    .map_err(error)?;
                let mut rows = stmt
                    .query((start.to_be_bytes(), end.to_be_bytes()))
                    .map_err(error)?;
                let mut entries = Vec::new();
                while let Some(row) = rows.next().map_err(error)? {
                    entries.push(decode(
                        row.get_ref(0).map_err(error)?.as_blob().map_err(error)?,
                    )?);
                }
                Ok(entries)
            })
            .await
    }

    async fn read_vote(&mut self) -> io::Result<Option<VoteOf<Types>>> {
        self.0.run(|db| get(db, "vote")).await
    }
}

impl RaftLogStorage<Types> for LogStore {
    type LogReader = Self;
    async fn get_log_reader(&mut self) -> Self {
        self.clone()
    }

    async fn get_log_state(&mut self) -> io::Result<LogState<Types>> {
        self.0
            .run(|db| {
                let purged: Option<LogIdOf<Types>> = get(db, "purged")?;
                let last: Option<Vec<u8>> = db
                    .query_row(
                        "SELECT data FROM entries ORDER BY idx DESC LIMIT 1",
                        [],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(error)?;
                let last = last
                    .map(|data| decode::<EntryOf<Types>>(&data).map(|e| e.log_id))
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
        self.0.run(move |db| set(db, "vote", &vote)).await
    }
    async fn read_committed(&mut self) -> io::Result<Option<LogIdOf<Types>>> {
        self.0.run(|db| Ok(get(db, "committed")?.flatten())).await
    }
    async fn save_committed(&mut self, committed: Option<LogIdOf<Types>>) -> io::Result<()> {
        self.0.run(move |db| set(db, "committed", &committed)).await
    }
    async fn append<I>(&mut self, entries: I, callback: IOFlushed<Types>) -> io::Result<()>
    where
        I: IntoIterator<Item = EntryOf<Types>> + Send,
        I::IntoIter: Send,
    {
        let entries: Vec<_> = entries.into_iter().collect();
        let result = self
            .0
            .run(move |db| {
                let tx = db.transaction().map_err(error)?;
                {
                    let mut insert = tx
                        .prepare_cached("INSERT INTO entries VALUES (?1,?2)")
                        .map_err(error)?;
                    for entry in entries {
                        insert
                            .execute((entry.log_id.index().to_be_bytes(), encode(&entry)?))
                            .map_err(error)?;
                    }
                }
                tx.commit().map_err(error)
            })
            .await;
        callback.io_completed(result.as_ref().map(|_| ()).map_err(error));
        result
    }
    async fn truncate_after(&mut self, last: Option<LogIdOf<Types>>) -> io::Result<()> {
        self.0
            .run(move |db| {
                if let Some(last) = last {
                    db.execute(
                        "DELETE FROM entries WHERE idx > ?1",
                        [last.index().to_be_bytes()],
                    )
                    .map_err(error)?;
                } else {
                    db.execute("DELETE FROM entries", []).map_err(error)?;
                }
                Ok(())
            })
            .await
    }
    async fn purge(&mut self, log: LogIdOf<Types>) -> io::Result<()> {
        self.0
            .run(move |db| {
                let tx = db.transaction().map_err(error)?;
                set(&tx, "purged", &log)?;
                tx.execute(
                    "DELETE FROM entries WHERE idx <= ?1",
                    [log.index().to_be_bytes()],
                )
                .map_err(error)?;
                tx.commit().map_err(error)
            })
            .await
    }
}
