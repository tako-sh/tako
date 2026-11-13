use crate::{
    FileStore, ReplicaStore, Segment,
    replica::{latest_segment, validate_segment},
    wal::{FRAME_HEADER_SIZE, HEADER_SIZE, PageMap, WalReader},
};
use anyhow::{Context, Result, ensure};
use rusqlite::Connection;
use std::{
    collections::BTreeSet,
    fs::File,
    io::{ErrorKind, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use terse_ltx::{Encoder, Header, NO_CHECKSUM};

#[derive(Clone, Debug)]
pub struct Options {
    pub busy_timeout: Duration,
    pub min_checkpoint_pages: u32,
    pub truncate_pages: u32,
    pub checkpoint_interval: Duration,
    pub max_sync_wal_bytes: u64,
    /// Waits for the first write after opening to capture the base snapshot, so a database
    /// that is only read never copies its pages. Until then the replica has no snapshot.
    pub defer_base_snapshot: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            busy_timeout: Duration::from_secs(1),
            min_checkpoint_pages: 1000,
            truncate_pages: 121359,
            checkpoint_interval: Duration::from_secs(60),
            max_sync_wal_bytes: 64 << 20,
            defer_base_snapshot: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckpointMode {
    Passive,
    Full,
    Restart,
    Truncate,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SyncResult {
    pub txid: u64,
    pub changed: bool,
    pub snapshot: bool,
    pub wal_bytes: u64,
}

impl SyncResult {
    fn merge(&mut self, next: Self) {
        self.txid = next.txid;
        self.changed |= next.changed;
        self.snapshot |= next.snapshot;
        self.wal_bytes += next.wal_bytes;
    }
}

pub struct Database {
    writer: Connection,
    reader: Connection,
    // Closing a separately opened DB descriptor can release SQLite's process locks.
    file: File,
    path: PathBuf,
    local: Arc<dyn ReplicaStore>,
    options: Options,
    page_size: u32,
    position: u64,
    cursor: Option<Cursor>,
    force_snapshot: bool,
    restart_safe: bool,
    schema_version: i64,
    last_checkpoint: Instant,
    dirty: bool,
    fenced: bool,
    /// WAL length once opened; the reader's lock keeps the WAL from restarting, so it grows
    /// exactly when a write commits.
    opened_wal: u64,
}

struct Cursor {
    end: u64,
    salt: [u32; 2],
    frame: Vec<u8>,
    commit: u32,
}

impl Database {
    pub fn open(path: impl AsRef<Path>, options: Options) -> Result<Self> {
        let path = path.as_ref();
        let mut metadata = std::ffi::OsString::from(".");
        metadata.push(path.file_name().context("database filename missing")?);
        metadata.push("-litestream");
        Self::with_store(
            path,
            options,
            Arc::new(FileStore::new(path.with_file_name(metadata))),
        )
    }

    pub fn with_store(
        path: impl AsRef<Path>,
        options: Options,
        local: Arc<dyn ReplicaStore>,
    ) -> Result<Self> {
        let path = path.as_ref();
        let position = recorded_position(local.as_ref())?;
        let writer = open_writer(path, options.busy_timeout)?;
        ensure_wal(&writer, &wal_path(path))?;
        Self::attach(path, options, local, writer, position)
    }

    /// Continues capture of a database that [`restore`](crate::restore) just produced at
    /// `txid`. No snapshot is forced: the next commit is captured as an L0 delta at
    /// `txid + 1` holding only its changed pages, and idle syncs emit nothing.
    ///
    /// The database file must be exactly the restored state at `txid`, unmodified since
    /// restore, and its LTX chain must have no database checksums, like every capture here.
    /// Neither can be verified, so this checks what it can: before opening the database, it
    /// returns an error if the WAL holds any frames, `local` holds captures after `txid`, or
    /// `txid` is zero. It never falls back to a snapshot; callers can use [`Self::with_store`].
    pub fn resume(
        path: impl AsRef<Path>,
        options: Options,
        local: Arc<dyn ReplicaStore>,
        txid: u64,
    ) -> Result<Self> {
        let path = path.as_ref();
        ensure!(txid > 0, "resumed position must be positive");
        ensure!(
            recorded_position(local.as_ref())? <= txid,
            "local capture is ahead of the restored position"
        );
        ensure!(
            wal_size(&wal_path(path))? <= HEADER_SIZE,
            "restored database has WAL frames, so it may not match its restored position"
        );
        let writer = open_writer(path, options.busy_timeout)?;
        let mut database = Self::attach(path, options, local, writer, txid)?;
        database.cursor = Some(Cursor::wal_start(database.page_count()?));
        database.force_snapshot = false;
        Ok(database)
    }

    pub fn position(&self) -> u64 {
        self.position
    }
    pub fn store(&self) -> &dyn ReplicaStore {
        self.local.as_ref()
    }

    pub fn sync(&mut self) -> Result<SyncResult> {
        self.ensure_live()?;
        if self.awaiting_first_write()? {
            return Ok(SyncResult::default());
        }
        if self.resumed_without_writes()? {
            return Ok(SyncResult {
                txid: self.position,
                ..SyncResult::default()
            });
        }
        self.prepare()?;
        let mut result = SyncResult::default();
        loop {
            let (next, limited) = self.capture(self.options.max_sync_wal_bytes)?;
            result.merge(next);
            if !limited {
                break;
            }
        }
        result.merge(self.checkpoint_if_needed()?);
        Ok(result)
    }

    pub fn checkpoint(&mut self, mode: CheckpointMode) -> Result<SyncResult> {
        self.ensure_live()?;
        self.prepare()?;
        let mut result = self.capture(0)?.0;
        let checkpoint = if mode == CheckpointMode::Passive {
            self.passive_checkpoint()?
        } else {
            self.blocking_checkpoint(mode)?
        };
        result.merge(checkpoint);
        self.last_checkpoint = Instant::now();
        self.dirty = false;
        Ok(result)
    }

    pub fn snapshot(&mut self) -> Result<SyncResult> {
        self.ensure_live()?;
        self.prepare()?;
        Ok(self.capture_at_level(0, 9)?.0)
    }

    /// Seeds a restored database above a replica's prior position before capturing new writes.
    pub fn recover_position(&mut self, replica: &dyn ReplicaStore) -> Result<()> {
        self.ensure_live()?;
        let latest = latest_segment(replica)?;
        if let Some(segment) = latest
            && segment.max_txid > self.position
        {
            validate_segment(replica, &segment)?;
            let mut writer = self.local.create(&segment)?;
            std::io::copy(&mut replica.open(&segment)?, &mut writer)?;
            self.publish(writer)?;
            self.position = segment.max_txid;
            self.force_snapshot = true;
            self.cursor = None;
        }
        Ok(())
    }

    pub fn close(mut self) -> Result<u64> {
        Ok(self.sync()?.txid)
    }

    fn attach(
        path: &Path,
        options: Options,
        local: Arc<dyn ReplicaStore>,
        writer: Connection,
        position: u64,
    ) -> Result<Self> {
        let reader = open_connection(path, options.busy_timeout)?;
        acquire_read_lock(&reader)?;
        let page_size = writer.query_row("PRAGMA page_size", [], |row| row.get(0))?;
        let schema_version = writer.query_row("PRAGMA schema_version", [], |row| row.get(0))?;
        let file = File::open(path)?;
        let opened_wal = wal_size(&wal_path(path))?;
        Ok(Self {
            writer,
            reader,
            file,
            path: path.to_path_buf(),
            local,
            options,
            page_size,
            position,
            cursor: None,
            force_snapshot: true,
            restart_safe: false,
            schema_version,
            last_checkpoint: Instant::now(),
            dirty: false,
            fenced: false,
            opened_wal,
        })
    }

    fn page_count(&self) -> Result<u32> {
        Ok(u32::try_from(
            self.file.metadata()?.len() / u64::from(self.page_size),
        )?)
    }

    // Skips `prepare`, whose write to create a missing WAL would itself need capturing.
    fn resumed_without_writes(&self) -> Result<bool> {
        Ok(!self.force_snapshot
            && self.cursor.as_ref().is_some_and(Cursor::at_wal_start)
            && wal_size(&wal_path(&self.path))? < HEADER_SIZE)
    }

    fn awaiting_first_write(&self) -> Result<bool> {
        Ok(self.options.defer_base_snapshot
            && self.position == 0
            && wal_path(&self.path).metadata()?.len() == self.opened_wal)
    }

    fn ensure_live(&self) -> Result<()> {
        ensure!(
            !self.fenced,
            "capture is fenced after an uncertain storage or locking operation; reopen it"
        );
        Ok(())
    }

    fn prepare(&mut self) -> Result<()> {
        let version = self
            .writer
            .query_row("PRAGMA schema_version", [], |row| row.get::<_, i64>(0))?;
        if version != self.schema_version {
            create_control_tables(&self.writer)?;
            self.schema_version = self
                .writer
                .query_row("PRAGMA schema_version", [], |row| row.get(0))?;
        }
        ensure_wal(&self.writer, &wal_path(&self.path))
    }

    fn capture(&mut self, max_bytes: u64) -> Result<(SyncResult, bool)> {
        self.capture_at_level(max_bytes, 0)
    }

    fn capture_at_level(&mut self, max_bytes: u64, level: u8) -> Result<(SyncResult, bool)> {
        let (reader, snapshot, start) = self.read_position()?;
        if !snapshot && level == 0 {
            return self.capture_from(reader, false, start, max_bytes, level);
        }
        self.writer.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| {
            let reader = WalReader::new(File::open(wal_path(&self.path))?)?;
            self.capture_from(reader, true, HEADER_SIZE, 0, level)
        })();
        let rollback = self.writer.execute_batch("ROLLBACK");
        if rollback.is_err() {
            self.fenced = true;
        }
        let result = result?;
        rollback?;
        Ok(result)
    }

    fn read_position(&self) -> Result<(WalReader<File>, bool, u64)> {
        let mut reader = WalReader::new(File::open(wal_path(&self.path))?)?;
        ensure!(reader.page_size == self.page_size, "WAL page size changed");
        if !self.force_snapshot
            && let Some(cursor) = &self.cursor
        {
            // The read lock held since `resume` keeps checkpoints from restarting the WAL,
            // so every frame follows the restored position whatever the salt.
            if cursor.at_wal_start() {
                return Ok((reader, false, HEADER_SIZE));
            }
            if reader.salt == cursor.salt && reader.resume(cursor.end, &cursor.frame)? {
                return Ok((reader, false, cursor.end));
            }
            if reader.salt != cursor.salt && self.restart_safe {
                return Ok((reader, false, HEADER_SIZE));
            }
        }
        Ok((
            WalReader::new(File::open(wal_path(&self.path))?)?,
            true,
            HEADER_SIZE,
        ))
    }

    fn capture_from(
        &mut self,
        mut reader: WalReader<File>,
        snapshot: bool,
        start: u64,
        max_bytes: u64,
        level: u8,
    ) -> Result<(SyncResult, bool)> {
        let map = reader.scan(max_bytes)?;
        if map.end == 0 && !snapshot {
            return Ok((
                SyncResult {
                    txid: self.position,
                    ..SyncResult::default()
                },
                false,
            ));
        }
        let commit = if map.commit > 0 {
            map.commit
        } else {
            self.page_count()?
        };
        let txid = self
            .position
            .checked_add(1)
            .context("transaction overflow")?;
        let wal_bytes = map.end.saturating_sub(start);
        let header = Header {
            flags: NO_CHECKSUM,
            page_size: self.page_size,
            commit,
            min_txid: if level == 9 { 1 } else { txid },
            max_txid: txid,
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)?
                .as_millis()
                .try_into()?,
            wal_offset: start.try_into()?,
            wal_size: wal_bytes.try_into()?,
            wal_salt: reader.salt,
            ..Header::default()
        };
        let segment = Segment::new(level, header.min_txid, txid)?;
        let writer = self.local.create(&segment)?;
        let mut encoder = Encoder::new(writer, header)?;
        self.encode_pages(&mut encoder, &map, snapshot, commit)?;
        let writer = encoder.finish(0)?;
        self.publish(writer)?;
        self.position = txid;
        self.force_snapshot = false;
        self.restart_safe = false;
        self.dirty = true;
        self.cursor = if map.end > HEADER_SIZE {
            Some(Cursor {
                end: map.end,
                salt: reader.salt,
                frame: map.last_frame,
                commit,
            })
        } else {
            None
        };
        Ok((
            SyncResult {
                txid,
                changed: true,
                snapshot,
                wal_bytes,
            },
            map.limited,
        ))
    }

    fn encode_pages(
        &mut self,
        encoder: &mut Encoder<Box<dyn crate::SegmentWriter>>,
        map: &PageMap,
        snapshot: bool,
        commit: u32,
    ) -> Result<()> {
        let previous = self.cursor.as_ref().map_or(0, |cursor| cursor.commit);
        let mut pages: BTreeSet<u32> = map.pages.keys().copied().collect();
        pages.extend(if snapshot {
            1..=commit
        } else {
            previous.saturating_add(1)..=commit
        });
        let mut wal = File::open(wal_path(&self.path))?;
        let mut data = vec![0; self.page_size as usize];
        for number in pages {
            if number == 0x40000000 / self.page_size + 1 {
                continue;
            }
            if let Some(offset) = map.pages.get(&number) {
                wal.seek(SeekFrom::Start(offset + FRAME_HEADER_SIZE))?;
                wal.read_exact(&mut data)?;
            } else {
                self.file.seek(SeekFrom::Start(
                    u64::from(number - 1) * u64::from(self.page_size),
                ))?;
                self.file.read_exact(&mut data)?;
            }
            encoder.write_page(number, &data)?;
        }
        Ok(())
    }

    fn publish(&mut self, writer: Box<dyn crate::SegmentWriter>) -> Result<()> {
        if let Err(error) = writer.commit() {
            self.fenced = true;
            return Err(error);
        }
        Ok(())
    }

    fn passive_checkpoint(&mut self) -> Result<SyncResult> {
        self.writer.execute_batch("BEGIN IMMEDIATE")?;
        let result: Result<SyncResult> = (|| {
            let (reader, snapshot, start) = self.read_position()?;
            let result = self.capture_from(reader, snapshot, start, 0, 0)?.0;
            let (_, log, done) = self.execute_checkpoint(CheckpointMode::Passive)?;
            self.restart_safe = log >= 0 && log == done;
            Ok(result)
        })();
        let rollback = self.writer.execute_batch("ROLLBACK");
        if rollback.is_err() {
            self.fenced = true;
        }
        let result = result?;
        rollback?;
        Ok(result)
    }

    fn blocking_checkpoint(&mut self, mode: CheckpointMode) -> Result<SyncResult> {
        // A truncate can consume commits between the pre-checkpoint sync and its lock.
        self.force_snapshot = true;
        let (busy, _, _) = self.execute_checkpoint(mode)?;
        ensure!(busy == 0, "checkpoint blocked by another SQLite reader");
        ensure_wal(&self.writer, &wal_path(&self.path))?;
        Ok(self.capture(0)?.0)
    }

    fn execute_checkpoint(&mut self, mode: CheckpointMode) -> Result<(i64, i64, i64)> {
        self.reader.execute_batch("ROLLBACK")?;
        let sql = format!(
            "PRAGMA wal_checkpoint({})",
            match mode {
                CheckpointMode::Passive => "PASSIVE",
                CheckpointMode::Full => "FULL",
                CheckpointMode::Restart => "RESTART",
                CheckpointMode::Truncate => "TRUNCATE",
            }
        );
        let result = self
            .reader
            .query_row(&sql, [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)));
        if let Err(error) = acquire_read_lock(&self.reader) {
            self.fenced = true;
            return Err(error);
        }
        Ok(result?)
    }

    fn checkpoint_if_needed(&mut self) -> Result<SyncResult> {
        let unchanged = SyncResult {
            txid: self.position,
            ..SyncResult::default()
        };
        if !self.dirty {
            return Ok(unchanged);
        }
        let frames = self.cursor.as_ref().map_or(0, |cursor| {
            (cursor.end - HEADER_SIZE) / (FRAME_HEADER_SIZE + u64::from(self.page_size))
        });
        let truncate = if self.options.truncate_pages == 0 {
            121359
        } else {
            self.options.truncate_pages
        };
        if frames >= u64::from(truncate) {
            let mut result = self.checkpoint(CheckpointMode::Passive)?;
            if !self.restart_safe {
                result.merge(self.checkpoint(CheckpointMode::Truncate)?);
            }
            Ok(result)
        } else if (self.options.min_checkpoint_pages > 0
            && frames >= u64::from(self.options.min_checkpoint_pages))
            || (!self.options.checkpoint_interval.is_zero()
                && self.last_checkpoint.elapsed() >= self.options.checkpoint_interval)
        {
            self.checkpoint(CheckpointMode::Passive)
        } else {
            Ok(unchanged)
        }
    }
}

impl Cursor {
    /// Positions capture before the WAL's first frame, whatever its salt.
    fn wal_start(commit: u32) -> Self {
        Self {
            end: HEADER_SIZE,
            salt: [0, 0],
            frame: Vec::new(),
            commit,
        }
    }

    fn at_wal_start(&self) -> bool {
        self.end == HEADER_SIZE
    }
}

fn recorded_position(local: &dyn ReplicaStore) -> Result<u64> {
    let Some(segment) = latest_segment(local)? else {
        return Ok(0);
    };
    validate_segment(local, &segment)?;
    Ok(segment.max_txid)
}

fn open_writer(path: &Path, timeout: Duration) -> Result<Connection> {
    let writer = open_connection(path, timeout)?;
    let mode: String = writer.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
    ensure!(mode == "wal", "SQLite did not enable WAL");
    create_control_tables(&writer)?;
    Ok(writer)
}

fn open_connection(path: &Path, timeout: Duration) -> Result<Connection> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(timeout)?;
    connection.execute_batch("PRAGMA wal_autocheckpoint=0; PRAGMA synchronous=FULL")?;
    Ok(connection)
}

fn create_control_tables(connection: &Connection) -> Result<()> {
    connection.execute_batch("CREATE TABLE IF NOT EXISTS _litestream_seq(id INTEGER PRIMARY KEY, seq INTEGER); CREATE TABLE IF NOT EXISTS _litestream_lock(id INTEGER)")?;
    Ok(())
}

fn ensure_wal(connection: &Connection, path: &Path) -> Result<()> {
    if wal_size(path)? >= HEADER_SIZE {
        return Ok(());
    }
    connection.execute(
        "INSERT INTO _litestream_seq(id,seq) VALUES(1,1) ON CONFLICT(id) DO UPDATE SET seq=seq+1",
        [],
    )?;
    Ok(())
}

fn wal_size(path: &Path) -> Result<u64> {
    match path.metadata() {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

fn acquire_read_lock(connection: &Connection) -> Result<()> {
    connection.execute_batch("BEGIN")?;
    if let Err(error) = connection.query_row("SELECT count(*) FROM _litestream_seq", [], |row| {
        row.get::<_, i64>(0)
    }) {
        let _ = connection.execute_batch("ROLLBACK");
        return Err(error.into());
    }
    Ok(())
}

fn wal_path(path: &Path) -> PathBuf {
    let mut path = path.as_os_str().to_os_string();
    path.push("-wal");
    PathBuf::from(path)
}
