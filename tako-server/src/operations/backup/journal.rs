//! Rollback directories of restores. Each holds the originals a restore
//! replaced, a durable intent record written before the first change, and
//! a lock file its restore holds, so the next server start can tell a
//! restore a crash interrupted from one that is still running and put the
//! original data back before any app starts.

use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use rustix::fs::FlockOperation;
use serde::{Deserialize, Serialize};
use tako_core::RestoredDatabases;

use super::snapshot::{SwapError, database_rollback, full_rollback, swap_databases};

/// Rollback directories sit beside the app data directory.
pub(super) const ROLLBACK_DIR_PREFIX: &str = ".data-restore-prev-";
const INTENT_FILE: &str = "restore.json";
const INTENT_TMP_FILE: &str = "restore.json.tmp";
const LOCK_FILE: &str = "restore.lock";
const INTENT_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum IntentKind {
    Databases,
    /// The whole data tree; the original is kept as `data/` in the
    /// rollback directory.
    Full,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum IntentState {
    /// Live data may be partly or wholly replaced and the restart is not
    /// validated: recovery puts the originals back.
    Swapping,
    /// The app runs on the restored data: only the originals' removal is
    /// pending.
    Committed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct IntentDatabase {
    /// Relative to the data root, e.g. `app/data.sqlite`.
    pub(super) relative: PathBuf,
    /// A live database existed at this path before the restore.
    pub(super) existed: bool,
}

/// The plan of a restore. Progress needs no separate record: each step is
/// one link, rename, or unlink whose effect recovery reads back from the
/// file system (see [`database_rollback`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct RestoreIntent {
    pub(super) version: u32,
    pub(super) kind: IntentKind,
    pub(super) state: IntentState,
    pub(super) data_root: PathBuf,
    #[serde(default)]
    pub(super) databases: Vec<IntentDatabase>,
    /// Directories below the data root the restore creates, parents first.
    #[serde(default)]
    pub(super) created_dirs: Vec<PathBuf>,
    /// A full restore found a data tree to replace.
    #[serde(default)]
    pub(super) had_data: bool,
}

impl RestoreIntent {
    pub(super) fn databases(
        data_root: &Path,
        databases: Vec<IntentDatabase>,
        created_dirs: Vec<PathBuf>,
    ) -> Self {
        Self {
            version: INTENT_VERSION,
            kind: IntentKind::Databases,
            state: IntentState::Swapping,
            data_root: data_root.to_path_buf(),
            databases,
            created_dirs,
            had_data: false,
        }
    }

    pub(super) fn full(data_root: &Path, had_data: bool) -> Self {
        Self {
            version: INTENT_VERSION,
            kind: IntentKind::Full,
            state: IntentState::Swapping,
            data_root: data_root.to_path_buf(),
            databases: Vec::new(),
            created_dirs: Vec::new(),
            had_data,
        }
    }
}

/// Write the intent record durably: the file, then its directory entry.
pub(super) fn write_intent(dir: &Path, intent: &RestoreIntent) -> io::Result<()> {
    let tmp = dir.join(INTENT_TMP_FILE);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(&serde_json::to_vec(intent).map_err(io::Error::other)?)?;
    file.sync_all()?;
    std::fs::rename(&tmp, dir.join(INTENT_FILE))?;
    File::open(dir)?.sync_all()
}

pub(super) fn read_intent(dir: &Path) -> Result<Option<RestoreIntent>, String> {
    let path = dir.join(INTENT_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    let intent: RestoreIntent = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    if intent.version != INTENT_VERSION {
        return Err(format!(
            "{} has unknown version {}",
            path.display(),
            intent.version
        ));
    }
    // The record never names data outside the app it sits beside.
    if intent.data_root.parent() != dir.parent() {
        return Err(format!(
            "{} names data outside its app: {}",
            path.display(),
            intent.data_root.display()
        ));
    }
    Ok(Some(intent))
}

/// A new rollback directory beside a data root, locked by this process
/// until dropped. Recovery skips a directory whose lock is held.
#[derive(Debug)]
pub(super) struct RollbackDir {
    path: PathBuf,
    _lock: File,
}

impl RollbackDir {
    pub(super) fn create(data_root: &Path) -> Result<Self, String> {
        let parent = data_root
            .parent()
            .ok_or_else(|| format!("data root has no parent: {}", data_root.display()))?;
        let path = parent.join(format!("{ROLLBACK_DIR_PREFIX}{}", nanoid::nanoid!(8)));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|e| format!("create {}: {e}", path.display()))?;
        let lock = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path.join(LOCK_FILE))
            .map_err(|e| format!("create restore lock in {}: {e}", path.display()))?;
        rustix::fs::flock(&lock, FlockOperation::NonBlockingLockExclusive)
            .map_err(|e| format!("lock {}: {e}", path.display()))?;
        File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(|e| format!("sync {}: {e}", parent.display()))?;
        Ok(Self { path, _lock: lock })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

/// Mutating steps of a restore swap. Tests pass a hook that runs before
/// each step and may stop the swap there, as if the process died.
pub(super) struct Steps<'a> {
    next: usize,
    hook: Option<&'a mut dyn FnMut(usize) -> bool>,
    stopped: bool,
}

impl<'a> Steps<'a> {
    pub(super) fn new(hook: Option<&'a mut dyn FnMut(usize) -> bool>) -> Self {
        Self {
            next: 0,
            hook,
            stopped: false,
        }
    }

    pub(super) fn step(&mut self) -> io::Result<()> {
        let current = self.next;
        self.next += 1;
        if let Some(hook) = self.hook.as_mut()
            && hook(current)
        {
            self.stopped = true;
            return Err(io::Error::other("stopped"));
        }
        Ok(())
    }

    pub(super) fn stopped(&self) -> bool {
        self.stopped
    }
}

/// Undo what the intent in `dir` describes and remove `dir`, or only remove
/// it when the restore was committed or never changed anything. The caller
/// owns the directory: its restore holds the lock, or recovery took it.
pub(super) fn roll_back(dir: &Path) -> Result<(), String> {
    let committed = match read_intent(dir)? {
        // The record is written before the first change.
        None => true,
        Some(intent) if intent.state == IntentState::Committed => true,
        Some(intent) => {
            match intent.kind {
                IntentKind::Databases => database_rollback(&intent, dir)?,
                // Whatever is live is the restored tree this restore put there.
                IntentKind::Full => full_rollback(&intent, dir, true)?,
            }
            false
        }
    };
    std::fs::remove_dir_all(dir).map_err(|error| {
        let what = if committed {
            "previous data"
        } else {
            "rollback copy"
        };
        format!("remove {what} {}: {error}", dir.display())
    })
}

/// Mark the restore in `dir` committed, then remove the originals.
pub(super) fn commit(dir: &Path) -> Result<(), String> {
    if let Some(mut intent) = read_intent(dir)? {
        intent.state = IntentState::Committed;
        write_intent(dir, &intent)
            .map_err(|e| format!("record restore commit in {}: {e}", dir.display()))?;
    }
    std::fs::remove_dir_all(dir)
        .map_err(|error| format!("remove previous data {}: {error}", dir.display()))
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Recovery {
    /// A running restore holds the directory.
    InUse,
    /// The originals are back in place.
    RolledBack,
    /// The restore had finished or never started; the directory is gone.
    Cleaned,
}

/// Recover one rollback directory a previous server process left behind.
pub(super) fn recover_rollback_dir(dir: &Path) -> Result<Recovery, String> {
    let lock = match std::fs::OpenOptions::new()
        .write(true)
        .open(dir.join(LOCK_FILE))
    {
        Ok(lock) => lock,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err("no restore record; left in place for manual recovery".into());
        }
        Err(error) => return Err(format!("open restore lock: {error}")),
    };
    match rustix::fs::flock(&lock, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(rustix::io::Errno::WOULDBLOCK) => return Ok(Recovery::InUse),
        Err(error) => return Err(format!("lock restore record: {error}")),
    }
    let swapping = read_intent(dir)?.is_some_and(|intent| intent.state == IntentState::Swapping);
    roll_back(dir)?;
    Ok(if swapping {
        Recovery::RolledBack
    } else {
        Recovery::Cleaned
    })
}

/// Put back the originals of every restore a crash interrupted. Run at
/// server start, before any app or workflow store opens app data.
pub(crate) fn recover_interrupted_restores(data_dir: &Path) {
    for dir in rollback_dirs(data_dir) {
        match recover_rollback_dir(&dir) {
            Ok(Recovery::InUse) => {}
            Ok(Recovery::RolledBack) => tracing::warn!(
                path = %dir.display(),
                "Rolled back a backup restore that was interrupted; the original data is in place"
            ),
            Ok(Recovery::Cleaned) => tracing::info!(
                path = %dir.display(),
                "Removed the rollback copy of a finished backup restore"
            ),
            Err(error) => tracing::error!(
                path = %dir.display(),
                "Could not recover an interrupted backup restore; the original data remains in this directory: {error}"
            ),
        }
    }
}

/// Rollback directories below `apps/<name>/<environment>/`.
fn rollback_dirs(data_dir: &Path) -> Vec<PathBuf> {
    let children = |dir: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|entry| entry.path())
            .collect()
    };
    let mut found = Vec::new();
    for name in children(&data_dir.join("apps")) {
        for environment in children(&name) {
            found.extend(children(&environment).into_iter().filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(ROLLBACK_DIR_PREFIX))
            }));
        }
    }
    found.sort();
    found
}

/// Remove backup and restore work directories a previous server process
/// left behind. Call only when no other server process can be using them.
pub(crate) fn sweep_backup_work_dirs(data_dir: &Path) {
    for kind in ["backups", "restore"] {
        let root = data_dir.join("tmp").join(kind);
        for entry in std::fs::read_dir(&root).into_iter().flatten().flatten() {
            let path = entry.path();
            let removed = if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            match removed {
                Ok(()) => {
                    tracing::info!(path = %path.display(), "Removed a stale backup work directory")
                }
                Err(error) => tracing::warn!(
                    path = %path.display(),
                    "Failed to remove a stale backup work directory: {error}"
                ),
            }
        }
    }
}

/// Live databases replaced by a database snapshot restore. The originals and
/// their sidecars stay in the rollback directory until [`DatabaseSwap::commit`]
/// or [`DatabaseSwap::rollback`]; until then a crash is undone at the next
/// server start.
#[derive(Debug)]
#[must_use = "commit the validated restore or roll it back after stopping the app"]
pub(super) struct DatabaseSwap {
    dir: RollbackDir,
    summary: RestoredDatabases,
    /// The process that swapped the files.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) worker_pid: u32,
}

impl DatabaseSwap {
    pub(super) fn summary(&self) -> &RestoredDatabases {
        &self.summary
    }

    /// Discard the originals once the app runs on the restored databases.
    pub(super) fn commit(self) -> Result<(), String> {
        commit(self.dir.path())
    }

    /// Put the original databases and sidecars back. Call only after
    /// stopping all processes that may open these databases.
    pub(super) fn rollback(self) -> Result<(), String> {
        run_restore_request(&RestoreRequest::RollBack {
            dir: self.dir.path().to_path_buf(),
        })
        .map(drop)
        .map_err(|error| error.message)
    }
}

/// Swap the staged databases into the live tree in a worker process. Its
/// checks open the live databases, and closing a descriptor to a SQLite
/// file drops every POSIX lock the closing process holds on it.
pub(super) fn swap_databases_in_worker(
    staged_root: &Path,
    databases: &[PathBuf],
    data_root: &Path,
) -> Result<DatabaseSwap, SwapError> {
    let dir = RollbackDir::create(data_root).map_err(SwapError::intact)?;
    let request = RestoreRequest::SwapDatabases {
        staged_root: staged_root.to_path_buf(),
        databases: databases.to_vec(),
        data_root: data_root.to_path_buf(),
        dir: dir.path().to_path_buf(),
    };
    match run_restore_request(&request) {
        Ok(reply) => Ok(DatabaseSwap {
            dir,
            summary: reply.summary.unwrap_or_default(),
            worker_pid: reply.pid,
        }),
        Err(error) if error.reported => Err(SwapError {
            message: error.message,
            originals_intact: error.originals_intact,
        }),
        // The worker died: the intent record says what to undo.
        Err(error) => {
            let rolled_back = run_restore_request(&RestoreRequest::RollBack {
                dir: dir.path().to_path_buf(),
            });
            Err(match rolled_back {
                Ok(_) => SwapError::intact(error.message),
                Err(rollback) => SwapError {
                    message: format!("{}; {}", error.message, rollback.message),
                    originals_intact: false,
                },
            })
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub(super) enum RestoreRequest {
    SwapDatabases {
        staged_root: PathBuf,
        databases: Vec<PathBuf>,
        data_root: PathBuf,
        dir: PathBuf,
    },
    RollBack {
        dir: PathBuf,
    },
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub(super) struct RestoreReply {
    /// The worker's process id, which tests compare with the server's.
    pub(super) pid: u32,
    pub(super) summary: Option<RestoredDatabases>,
    pub(super) error: Option<String>,
    #[serde(default)]
    pub(super) originals_intact: bool,
}

#[derive(Debug)]
pub(super) struct RequestError {
    pub(super) message: String,
    pub(super) originals_intact: bool,
    /// The worker ran to the end and reported this error itself.
    pub(super) reported: bool,
}

fn run_restore_request(request: &RestoreRequest) -> Result<RestoreReply, RequestError> {
    let crashed = |message: String| RequestError {
        message,
        originals_intact: false,
        reported: false,
    };
    let request = serde_json::to_string(request).map_err(|e| crashed(e.to_string()))?;
    let stdout = super::worker::run_worker(&["--restore-worker".as_ref(), request.as_ref()])
        .map_err(crashed)?;
    let reply: RestoreReply = serde_json::from_slice(&stdout)
        .map_err(|e| crashed(format!("restore worker sent an invalid reply: {e}")))?;
    match reply.error {
        Some(message) => Err(RequestError {
            message,
            originals_intact: reply.originals_intact,
            reported: true,
        }),
        None => Ok(reply),
    }
}

/// Entry point of `tako-server --restore-worker <request>`: run one restore
/// step and return the JSON reply for stdout.
pub(crate) fn run_restore_worker(request: &str) -> Result<String, String> {
    let request: RestoreRequest =
        serde_json::from_str(request).map_err(|e| format!("invalid restore request: {e}"))?;
    let mut reply = RestoreReply {
        pid: std::process::id(),
        ..RestoreReply::default()
    };
    match request {
        RestoreRequest::SwapDatabases {
            staged_root,
            databases,
            data_root,
            dir,
        } => match swap_databases(&staged_root, &databases, &data_root, &dir) {
            Ok(summary) => reply.summary = Some(summary),
            Err(error) => {
                reply.error = Some(error.message);
                reply.originals_intact = error.originals_intact;
            }
        },
        RestoreRequest::RollBack { dir } => {
            if let Err(error) = roll_back(&dir) {
                reply.error = Some(format!("{error}; originals remain at {}", dir.display()));
            }
        }
    }
    serde_json::to_string(&reply).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_recovery_finds_rollback_dirs_and_sweeps_work_dirs() {
        let temp = tempfile::tempdir().unwrap();
        let data_root = temp.path().join("apps/demo/production/data");
        std::fs::create_dir_all(&data_root).unwrap();
        // A restore that died before its first change.
        let unused = RollbackDir::create(&data_root)
            .unwrap()
            .path()
            .to_path_buf();
        // A directory from before intent records: never guessed at.
        let legacy = data_root.with_file_name(format!("{ROLLBACK_DIR_PREFIX}legacy"));
        std::fs::create_dir(&legacy).unwrap();
        std::fs::write(legacy.join("kept"), b"original").unwrap();
        // A restore still running in this process.
        let running = RollbackDir::create(&data_root).unwrap();
        for work in ["tmp/backups/b1-abc/snapshot", "tmp/restore/restore-abc"] {
            std::fs::create_dir_all(temp.path().join(work)).unwrap();
        }

        assert_eq!(
            rollback_dirs(temp.path()).len(),
            3,
            "{:?}",
            rollback_dirs(temp.path())
        );
        recover_interrupted_restores(temp.path());
        sweep_backup_work_dirs(temp.path());
        assert!(!unused.exists());
        assert_eq!(std::fs::read(legacy.join("kept")).unwrap(), b"original");
        assert!(running.path().exists());
        for kind in ["backups", "restore"] {
            let root = temp.path().join("tmp").join(kind);
            assert_eq!(std::fs::read_dir(root).unwrap().count(), 0, "{kind}");
        }
    }

    #[test]
    fn an_intent_naming_another_data_root_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let data_root = temp.path().join("apps/demo/production/data");
        std::fs::create_dir_all(&data_root).unwrap();
        let dir = RollbackDir::create(&data_root).unwrap();
        let intent =
            RestoreIntent::databases(&temp.path().join("elsewhere/data"), Vec::new(), Vec::new());
        write_intent(dir.path(), &intent).unwrap();
        let error = read_intent(dir.path()).unwrap_err();
        assert!(error.contains("names data outside its app"), "{error}");
        assert!(roll_back(dir.path()).is_err());
        assert!(dir.path().exists());
    }
}
