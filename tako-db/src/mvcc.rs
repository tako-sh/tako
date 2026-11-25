//! A file-backed Turso owner. Only its committed image leaves the private directory.
#[path = "mvcc/io.rs"]
mod directory_io;
mod image;
mod local;
pub use local::reset_database_file;
mod storage;

use image::Layout;
use std::{
    fs::{self, File},
    io,
    path::Path,
    sync::Arc,
};
use storage::ReplicatedLog;
use tempfile::TempDir;
use turso_core::{
    ConnectOpts, Database, DatabaseOpts, IO, OpenFlags, OpenOptions, SessionRole,
    mvcc::persistent_storage::{DurableStorage, Storage},
};
use turso_sqlite::SqliteDialect;

pub(crate) struct Owner {
    db: Arc<Database>,
    log: Option<Arc<ReplicatedLog>>,
    _directory: Option<TempDir>,
    _owner_lock: Option<File>,
    slots: Arc<tokio::sync::Semaphore>,
}
fn other(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}
fn text(path: &Path) -> io::Result<&str> {
    path.to_str()
        .ok_or_else(|| io::Error::other("database path must be UTF-8"))
}
impl Owner {
    fn open(source: &Path, writer: Option<tako_raft::Writer>) -> io::Result<Self> {
        let directory = tempfile::Builder::new().prefix("turso-").tempdir_in(
            source
                .parent()
                .ok_or_else(|| other("database path has no parent"))?,
        )?;
        let mut file = File::open(source)?;
        let layout = Layout::inspect(&mut file)?;
        let path = directory.path().join("data.db");
        if let Some(layout) = layout {
            layout.extract(&mut file, directory.path())?
        } else if file.metadata()?.len() > 0 {
            fs::copy(source, &path)?;
        }
        Self::open_database(&path, Some(directory), None, writer, layout)
    }
    fn open_database(
        path: &Path,
        directory: Option<TempDir>,
        owner_lock: Option<File>,
        writer: Option<tako_raft::Writer>,
        layout: Option<Layout>,
    ) -> io::Result<Self> {
        let io: Arc<dyn IO> = Arc::new(directory_io::DirectoryIo::new(path).map_err(other)?);
        let mut opts = OpenOptions::new(Arc::new(SqliteDialect))
            .flags(OpenFlags::Create)
            .db_opts(DatabaseOpts::new().with_views(true).with_mvcc(true));
        let log = if let Some(writer) = writer {
            let file = io
                .open_file(
                    text(&path.with_file_name("data.db-log"))?,
                    OpenFlags::Create,
                    false,
                )
                .map_err(other)?;
            let log = Arc::new(ReplicatedLog::new(
                Storage::new(file, io.clone(), None),
                path.parent()
                    .expect("private database directory")
                    .to_owned(),
                writer,
                layout,
            ));
            opts = opts.durable_storage(Some(log.clone() as Arc<dyn DurableStorage>));
            Some(log)
        } else {
            None
        };
        let db = Database::open(io, text(path)?, opts).map_err(other)?;
        Ok(Self {
            db,
            log,
            _directory: directory,
            _owner_lock: owner_lock,
            slots: Arc::new(tokio::sync::Semaphore::new(4)),
        })
    }
    pub fn primary(writer: tako_raft::Writer) -> io::Result<Self> {
        let source = writer.path().to_owned();
        Self::open(&source, Some(writer))
    }
    pub fn replica(source: &Path) -> io::Result<Self> {
        Self::open(source, None)
    }
    pub fn usable(&self) -> bool {
        self.log.as_ref().is_none_or(|log| log.usable())
    }
    pub fn barrier(&self) -> io::Result<()> {
        if let Some(log) = &self.log {
            log.barrier()
        } else {
            Ok(())
        }
    }
    pub fn writer_slot(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        self.slots.clone().try_acquire_owned().ok()
    }
    pub fn revision(&self) -> io::Result<u64> {
        self.log.as_ref().map_or(Ok(0), |log| log.revision())
    }
    pub fn confirm(&self, before: u64) -> io::Result<()> {
        if let Some(log) = &self.log {
            log.confirm(before)?;
        }
        Ok(())
    }
    pub fn fence(&self) {
        if let Some(log) = &self.log {
            log.fence();
        }
    }
    pub fn connect(&self) -> io::Result<Arc<turso_core::Connection>> {
        self.db
            .connect(ConnectOpts::new(SessionRole::Superuser))
            .map_err(other)
    }
}

/// Check an import on a private native copy before publishing it to Raft.
pub fn verify_hosted_import(path: &Path) -> Result<(), String> {
    let sql =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| e.to_string())?;
    let without_rowid: Option<String> = rusqlite::OptionalExtension::optional(sql.query_row(
        "SELECT name FROM pragma_table_list WHERE wr = 1 LIMIT 1",
        [],
        |row| row.get(0),
    ))
    .map_err(|e| e.to_string())?;
    if let Some(table) = without_rowid {
        return Err(format!(
            "Turso hosted imports require rowid tables. Recreate {table} with an ordinary primary key before importing."
        ));
    }
    drop(sql);
    let owner = Owner::replica(path).map_err(|e| e.to_string())?;
    owner
        .connect()
        .map_err(|e| e.to_string())?
        .execute("SELECT name FROM sqlite_schema")
        .map_err(|e| format!("Turso cannot open the imported schema: {e}"))
}

/// Convert a restored, quorum-committed image into a portable SQLite file.
/// Conversion operates exclusively on a disposable checkout.
pub(crate) fn export(source: &Path, dest: &Path) -> io::Result<bool> {
    if Layout::inspect(&mut File::open(source)?)?.is_none() {
        return Ok(false);
    }
    let owner = Owner::replica(source)?;
    export_copy(&owner, dest)?;
    Ok(true)
}

pub(crate) fn export_copy(owner: &Owner, dest: &Path) -> io::Result<()> {
    let conn = owner.connect()?;
    conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")
        .map_err(other)?;
    conn.execute("PRAGMA journal_mode=WAL").map_err(other)?;
    conn.close().map_err(other)?;
    drop(conn);
    super::engine::snapshot_database(
        Path::new(&owner.db.path),
        dest,
        std::time::Duration::from_secs(5),
    )
    .map_err(other)
}

/// A freshly downloaded recovery image or private Raft checkout can be
/// replaced atomically with its verified, portable database.
pub fn materialize_recovery_image(path: &Path) -> Result<(), String> {
    let dir = tempfile::tempdir_in(path.parent().ok_or("database has no parent directory")?)
        .map_err(|e| e.to_string())?;
    let portable = dir.path().join("portable.sqlite");
    if export(path, &portable).map_err(|e| e.to_string())? {
        fs::rename(portable, path).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::BTreeMap, time::Duration};
    use tako_raft::{Config, Peer, Resource};
    #[test]
    fn hosted_import_checks_the_native_schema_before_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("upload.sqlite");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE items(k TEXT PRIMARY KEY) STRICT")
            .unwrap();
        verify_hosted_import(&path).unwrap();
        conn.execute_batch("CREATE TABLE unsupported(k TEXT PRIMARY KEY) WITHOUT ROWID")
            .unwrap();
        assert!(
            verify_hosted_import(&path)
                .unwrap_err()
                .contains("unsupported")
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM items", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn live_commit_is_recoverable_at_quorum_and_rollback_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let peer = Peer::default();
        let (node, replica) = peer
            .open(
                "db",
                [4; 32],
                1,
                &dir.path().join("raft"),
                Config::default(),
            )
            .await
            .unwrap();
        node.initialize(BTreeMap::from([(1, "127.0.0.1:1".parse().unwrap())]))
            .await
            .unwrap();
        node.wait(Some(Duration::from_secs(5)))
            .current_leader(1, "leader")
            .await
            .unwrap();
        let resource = Resource::new(node, replica, dir.path());
        resource.set_backup_settings(true, vec![1]).await.unwrap();
        tokio::task::spawn_blocking(move || {
            let owner = Arc::new(Owner::primary(resource.writer().unwrap()).unwrap());
            let conn = owner.connect().unwrap();
            conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY)")
                .unwrap();
            conn.execute("BEGIN CONCURRENT; INSERT INTO t VALUES(1); COMMIT")
                .unwrap();
            let spool_dir = dir.path().join("backup");
            let spool = tako_backup::CommittedSpool::open(&spool_dir).unwrap();
            let (base, point) = resource.backup_image().unwrap();
            spool
                .snapshot(
                    base.reopen().unwrap(),
                    point.txid,
                    point.captured_at_ms,
                    point.page_size,
                )
                .unwrap();
            let a = resource.local_file().unwrap();
            let tx = resource.transaction_id().unwrap();
            conn.execute("BEGIN CONCURRENT; INSERT INTO t VALUES(9); ROLLBACK")
                .unwrap();
            assert_eq!(resource.transaction_id().unwrap(), tx);
            conn.execute("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
            conn.execute("INSERT INTO t VALUES(2)").unwrap();
            let tip = resource.transaction_id().unwrap();
            assert_eq!(tip, point.txid + 1);
            let change = resource.backup_delta(tip).unwrap().unwrap();
            spool
                .delta(
                    tip,
                    change.captured_at_ms,
                    change.file_size,
                    change.page_size,
                    &change.pages,
                )
                .unwrap();
            for (index, txid) in [point.txid, tip].into_iter().enumerate() {
                let dest = dir.path().join(format!("restored-{txid}.sqlite"));
                tokio::runtime::Handle::current()
                    .block_on(tako_backup::Replica::apply(&spool_dir, txid, &dest))
                    .unwrap();
                materialize_recovery_image(&dest).unwrap();
                let restored = rusqlite::Connection::open(dest).unwrap();
                assert_eq!(
                    restored
                        .query_row("SELECT count(*) FROM t", [], |row| row.get::<_, i64>(0))
                        .unwrap(),
                    index as i64 + 1
                );
            }
            let b = resource.local_file().unwrap();
            for (i, file) in [a, b].iter().enumerate() {
                let data = Owner::replica(file.path()).unwrap();
                let rd = data.connect().unwrap();
                let rows = rd
                    .prepare("SELECT id FROM t ORDER BY id")
                    .unwrap()
                    .run_collect_rows()
                    .unwrap();
                assert_eq!(rows.len(), i + 1);
                assert_eq!(rows[i][0].as_int(), Some((i + 1) as i64));
                let portable = dir.path().join(format!("point-{i}.sqlite"));
                assert!(export(file.path(), &portable).unwrap());
                let sql = rusqlite::Connection::open(portable).unwrap();
                assert_eq!(
                    sql.query_row("SELECT count(*) FROM t", [], |r| r.get::<_, i64>(0))
                        .unwrap(),
                    (i + 1) as i64
                );
            }
            use crate::engine::{Args, Budget, Connection, EngineConfig};
            let config = EngineConfig {
                max_value_bytes: 1024,
                ..Default::default()
            };
            let buffered = Arc::new(tokio::sync::Semaphore::new(1 << 20));
            // During a handoff another owner may advance the same group's
            // revision without a new term. Stale handles must then reopen.
            let stale = Owner::primary(resource.writer().unwrap()).unwrap();
            conn.execute("INSERT INTO t VALUES(3)").unwrap();
            assert!(stale.barrier().is_err());
            assert!(!stale.usable());
            let replacement = Owner::primary(resource.writer().unwrap()).unwrap();
            assert_eq!(
                replacement
                    .connect()
                    .unwrap()
                    .prepare("SELECT count(*) FROM t")
                    .unwrap()
                    .run_collect_rows()
                    .unwrap()[0][0]
                    .as_int(),
                Some(3)
            );
            let mut clients = (0..4)
                .map(|_| {
                    Connection::hosted(owner.clone(), &config, false, buffered.clone()).unwrap()
                })
                .collect::<Vec<_>>();
            assert!(Connection::hosted(owner.clone(), &config, false, buffered.clone()).is_err());
            let execute = |c: &mut Connection, sql| {
                c.execute(sql, &Args::Positional(&[]), true, &mut Budget::new(16384))
            };
            assert_eq!(
                execute(&mut clients[0], "SELECT length(zeroblob(2048))")
                    .unwrap_err()
                    .code,
                "SQLITE_TOOBIG"
            );
            execute(
                &mut clients[0],
                "CREATE TABLE inventory(id INTEGER PRIMARY KEY, n INTEGER)",
            )
            .unwrap();
            execute(&mut clients[0], "INSERT INTO inventory VALUES (1,0),(2,0)").unwrap();
            for c in clients.iter_mut().take(2) {
                execute(c, "BEGIN CONCURRENT").unwrap();
            }
            execute(&mut clients[0], "UPDATE inventory SET n=1 WHERE id=1").unwrap();
            execute(&mut clients[1], "UPDATE inventory SET n=2 WHERE id=2").unwrap();
            for c in clients.iter_mut().take(2) {
                execute(c, "COMMIT").unwrap();
            }
            assert_eq!(
                execute(&mut clients[2], "SELECT sum(n) FROM inventory")
                    .unwrap()
                    .rows[0][0],
                crate::Value::Integer(3)
            );
            assert_eq!(
                execute(
                    &mut clients[2],
                    "INSERT INTO inventory VALUES(1,3) ON CONFLICT(id) DO UPDATE SET n=3"
                )
                .unwrap()
                .last_insert_rowid,
                None
            );
        })
        .await
        .unwrap();
        peer.close("db").await.unwrap();
    }
}
