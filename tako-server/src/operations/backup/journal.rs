//! Rollback directories of restores. Each holds the originals a restore
//! replaced, a durable intent record written before the first change, and
//! a lock file its restore holds, so the next server start can tell a
//! restore a crash interrupted from one that is still running and put the
//! original data back before any app starts.
//!
//! The name of a rollback directory says what recovery may do with it:
//!
//! - [`ROLLBACK_DIR_PREFIX`]: a restore in progress, or one whose process
//!   died. Recovery rolls it back only at a normal start, only when its
//!   lock is free, and only when no server start came after the process
//!   that wrote it (see [`RecoveryMode`]); otherwise it reports and leaves it.
//! - [`ABANDONED_DIR_PREFIX`]: a server gave up on the restore and kept
//!   running. Recovery only reports it.
//! - [`FINISHED_DIR_PREFIX`]: committed or rolled back; only the removal
//!   of the directory is pending.

use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::Duration;

use rustix::fs::FlockOperation;
use serde::{Deserialize, Serialize};
use tako_core::RestoredDatabases;

use super::snapshot::{SwapError, database_rollback, full_rollback, swap_databases};

/// Rollback directories sit beside the app data directory.
pub(super) const ROLLBACK_DIR_PREFIX: &str = ".data-restore-prev-";
pub(super) const ABANDONED_DIR_PREFIX: &str = ".data-restore-abandoned-";
pub(super) const FINISHED_DIR_PREFIX: &str = ".data-restore-done-";
const INTENT_FILE: &str = "restore.json";
const INTENT_TMP_FILE: &str = "restore.json.tmp";
const LOCK_FILE: &str = "restore.lock";
const INTENT_VERSION: u32 = 2;
/// Names the server generation: a new one at every start that serves apps.
const GENERATION_FILE: &str = "restore-generation";
const GENERATION_TMP_FILE: &str = "restore-generation.tmp";
/// How long a normal start waits for a restore worker that outlived its
/// server; it is killed when its server dies.
const ORPHAN_LOCK_WAIT: Duration = if cfg!(test) {
    Duration::from_millis(100)
} else {
    Duration::from_secs(10)
};

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
    /// validated. A finished restore removes the record instead of
    /// changing its state, which needs no free disk space.
    Swapping,
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
    /// The server generation of the process that ran the restore.
    pub(super) generation: String,
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
            generation: current_generation(),
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
            generation: current_generation(),
            data_root: data_root.to_path_buf(),
            databases: Vec::new(),
            created_dirs: Vec::new(),
            had_data,
        }
    }
}

/// Write `bytes` to `dir/name` durably: a temporary file, then the rename,
/// then the directory entry.
fn write_durably(dir: &Path, tmp_name: &str, name: &str, bytes: &[u8]) -> io::Result<()> {
    let tmp = dir.join(tmp_name);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&tmp, dir.join(name))?;
    File::open(dir)?.sync_all()
}

/// Write the intent record durably: the file, then its directory entry.
pub(super) fn write_intent(dir: &Path, intent: &RestoreIntent) -> io::Result<()> {
    let bytes = serde_json::to_vec(intent).map_err(io::Error::other)?;
    write_durably(dir, INTENT_TMP_FILE, INTENT_FILE, &bytes)
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

/// The record is the only thing that makes a rollback directory roll back:
/// removing it, which needs no free disk space, is the point after which
/// recovery never touches live data for this restore.
pub(super) fn retire(dir: &Path) -> Result<(), String> {
    let path = dir.join(INTENT_FILE);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("remove {}: {error}", path.display())),
    }
    sync_dir(dir)
}

fn sync_dir(dir: &Path) -> Result<(), String> {
    File::open(dir)
        .and_then(|dir| dir.sync_all())
        .map_err(|e| format!("sync {}: {e}", dir.display()))
}

fn renamed(dir: &Path, prefix: &str) -> PathBuf {
    let name = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let id = [
        ROLLBACK_DIR_PREFIX,
        ABANDONED_DIR_PREFIX,
        FINISHED_DIR_PREFIX,
    ]
    .iter()
    .find_map(|known| name.strip_prefix(known))
    .unwrap_or(&name);
    dir.with_file_name(format!("{prefix}{id}"))
}

/// Remove a retired rollback directory. Renamed first, so a removal that
/// stops part way (the lock file may go before the rest) leaves a
/// directory recovery knows it may finish removing.
fn discard(dir: &Path) -> Result<(), String> {
    let finished = renamed(dir, FINISHED_DIR_PREFIX);
    if dir != finished {
        std::fs::rename(dir, &finished)
            .map_err(|e| format!("mark {} finished: {e}", dir.display()))?;
        if let Some(parent) = finished.parent() {
            sync_dir(parent)?;
        }
    }
    super::mounts::remove_tree(&finished)
        .map_err(|error| format!("remove previous data {}: {error}", finished.display()))
}

/// Retire and remove a rollback directory whose live data needs nothing
/// more: the restore was committed, rolled back, or never changed anything.
pub(super) fn finish(dir: &Path) -> Result<(), String> {
    retire(dir)?;
    discard(dir)
}

/// Put back the originals the intent in `dir` describes. Repeating it after
/// it stopped part way is safe. The caller owns the directory: its restore
/// holds the lock, or recovery took it.
fn undo(dir: &Path) -> Result<(), String> {
    match read_intent(dir)? {
        // The record is written before the first change.
        None => Ok(()),
        Some(intent) => match intent.kind {
            IntentKind::Databases => database_rollback(&intent, dir),
            // Whatever is live is the restored tree this restore put there.
            IntentKind::Full => full_rollback(&intent, dir, true),
        },
    }
}

/// Undo what the intent in `dir` describes and retire it. A failed removal
/// of the directory afterwards leaves it for the next start to remove.
pub(super) fn roll_back(dir: &Path) -> Result<(), String> {
    undo(dir)?;
    retire(dir)?;
    if let Err(error) = discard(dir) {
        tracing::warn!("Restore rolled back but its rollback copy remains: {error}");
    }
    Ok(())
}

/// Mark the restore in `dir` committed, then remove the originals.
#[cfg(test)]
pub(super) fn commit(dir: &Path) -> Result<(), String> {
    finish(dir)
}

/// A new rollback directory beside a data root, locked by this process
/// until dropped. Recovery skips a directory whose lock is held.
///
/// A restore ends with [`Self::finish`], [`Self::roll_back`], or
/// [`Self::abandon`]. Dropped without one while it still exists (an early
/// return, a cancelled restore, a panic), the directory is abandoned: this
/// process keeps running, so the data may change after the restore, and no
/// later start may roll it back.
#[derive(Debug)]
pub(super) struct RollbackDir {
    path: PathBuf,
    _lock: File,
    ended: bool,
}

impl RollbackDir {
    /// Refuses while an earlier restore of the same app left a rollback
    /// directory that is not finished.
    pub(super) fn create(data_root: &Path) -> Result<Self, String> {
        refuse_unfinished_restores(data_root)?;
        Self::create_unchecked(data_root)
    }

    fn create_unchecked(data_root: &Path) -> Result<Self, String> {
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
        Ok(Self {
            path,
            _lock: lock,
            ended: false,
        })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    fn exists(&self) -> bool {
        std::fs::symlink_metadata(&self.path).is_ok()
    }

    /// The live data needs nothing more from this directory: the restore
    /// was committed or never changed anything. If even retiring the record
    /// fails, the directory is abandoned.
    pub(super) fn finish(mut self) -> Result<(), String> {
        self.ended = true;
        if !self.exists() {
            return Ok(());
        }
        if let Err(error) = retire(&self.path) {
            let kept = self.abandon(&format!(
                "could not record the restore as finished: {error}"
            ));
            return Err(format!(
                "could not record the restore as finished: {error}; it is left in {} and never rolled back",
                kept.display()
            ));
        }
        discard(&self.path)
    }

    /// Put the originals back. On failure the directory is abandoned and
    /// the error says where the originals are.
    pub(super) fn roll_back(mut self) -> Result<(), String> {
        self.ended = true;
        if let Err(error) = undo(&self.path).and_then(|()| retire(&self.path)) {
            let kept = self.abandon(&format!("rollback failed: {error}"));
            return Err(format!(
                "{error}; original data remains in {}",
                kept.display()
            ));
        }
        if let Err(error) = discard(&self.path) {
            tracing::warn!("Restore rolled back but its rollback copy remains: {error}");
        }
        Ok(())
    }

    /// Give up on the restore while this process keeps running: whatever is
    /// live stays live, the originals stay in the directory, and recovery
    /// only reports it. Returns where the directory is now.
    ///
    /// Renaming needs no free disk space. If it fails, removing the lock
    /// file also makes recovery leave the directory alone; if that fails
    /// too, starting a new server generation does, unless the disk refuses
    /// that write as well.
    pub(super) fn abandon(&mut self, reason: &str) -> PathBuf {
        self.ended = true;
        let abandoned = renamed(&self.path, ABANDONED_DIR_PREFIX);
        let marked = match std::fs::rename(&self.path, &abandoned) {
            Ok(()) => {
                self.path = abandoned;
                if let Some(parent) = self.path.parent() {
                    let _ = sync_dir(parent);
                }
                Ok(())
            }
            Err(rename) => std::fs::remove_file(self.path.join(LOCK_FILE))
                .map_err(|unlink| format!("rename: {rename}; remove lock: {unlink}"))
                .and_then(|()| sync_dir(&self.path))
                .or_else(|error| {
                    // Rollback directories sit at <data dir>/apps/<name>/<environment>/.
                    start_generation_in(self.path.ancestors().nth(4))
                        .map_err(|generation| format!("{error}; {generation}"))
                }),
        };
        match marked {
            Ok(()) => tracing::error!(
                path = %self.path.display(),
                "Gave up on a backup restore: {reason}. Whatever is live stays as it is and the original data remains in this directory; recovery never applies it. Check the app data, keep what you need from the directory, then remove it."
            ),
            Err(error) => tracing::error!(
                path = %self.path.display(),
                "Gave up on a backup restore: {reason}. Could not mark it abandoned ({error}); the next server start may roll it back. Move this directory out of the app directory before restarting the server."
            ),
        }
        self.path.clone()
    }

    /// Release the lock and leave the directory as it is, as if the process
    /// died here: for test hooks that simulate a crash.
    pub(super) fn leave_for_recovery(mut self) {
        self.ended = true;
    }
}

impl Drop for RollbackDir {
    fn drop(&mut self) {
        // Before the lock is released with the fields.
        if !self.ended && self.exists() {
            self.abandon("the restore stopped before it was committed or rolled back");
        }
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

/// Refuse a new restore beside `data_root` while an earlier one left a
/// rollback directory that is not finished. Finished ones are removed.
pub(super) fn refuse_unfinished_restores(data_root: &Path) -> Result<(), String> {
    let Some(parent) = data_root.parent() else {
        return Ok(());
    };
    for dir in restore_dirs_in(parent) {
        if has_prefix(&dir, FINISHED_DIR_PREFIX) {
            if let Err(error) = super::mounts::remove_tree(&dir) {
                tracing::warn!(path = %dir.display(), "Failed to remove a finished restore: {error}");
            }
            continue;
        }
        if has_prefix(&dir, ROLLBACK_DIR_PREFIX) && lock_is_held(&dir) {
            return Err(format!(
                "A restore of this app is in progress in another server process ({}).",
                dir.display()
            ));
        }
        return Err(format!(
            "A previous restore of this app did not finish and left {}. Tako does not start another restore over it: check the app data, keep what you need from that directory, then remove it and retry.",
            dir.display()
        ));
    }
    Ok(())
}

fn lock_is_held(dir: &Path) -> bool {
    std::fs::OpenOptions::new()
        .write(true)
        .open(dir.join(LOCK_FILE))
        .is_ok_and(|lock| {
            rustix::fs::flock(&lock, FlockOperation::NonBlockingLockShared)
                == Err(rustix::io::Errno::WOULDBLOCK)
        })
}

/// The generation this process writes into restore records. Startup sets
/// it; until then (and in tests) it is empty.
static GENERATION: RwLock<String> = RwLock::new(String::new());

fn current_generation() -> String {
    GENERATION
        .read()
        .map(|generation| generation.clone())
        .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
}

fn set_generation(generation: String) {
    *GENERATION
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = generation;
}

fn read_generation(data_dir: &Path) -> Result<Option<String>, String> {
    let path = data_dir.join(GENERATION_FILE);
    match std::fs::read_to_string(&path) {
        Ok(generation) if !generation.trim().is_empty() => Ok(Some(generation.trim().to_string())),
        Ok(_) => Err(format!("{} is empty", path.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("read {}: {error}", path.display())),
    }
}

/// Start a new server generation and record it, so that every restore
/// record written before is only reported from now on.
fn start_generation_in(data_dir: Option<&Path>) -> Result<(), String> {
    let generation = nanoid::nanoid!(16);
    set_generation(generation.clone());
    let data_dir = data_dir.ok_or("no server data directory")?;
    write_durably(
        data_dir,
        GENERATION_TMP_FILE,
        GENERATION_FILE,
        generation.as_bytes(),
    )
    .map_err(|e| format!("record server generation in {}: {e}", data_dir.display()))
}

/// How the starting server relates to other server processes on the same
/// data directory, which decides what startup recovery may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryMode {
    /// A normal start: no other server process serves this data directory.
    /// Rolls back records of the previous generation whose lock is free.
    Primary,
    /// An upgrade reload or a native promotion child, which takes over from
    /// a server that may still be running. Changes nothing and starts a new
    /// generation, so records of the server it replaces are only reported
    /// at later starts.
    Successor,
    /// A standby beside a running primary. Changes nothing, and its own
    /// restores are never rolled back automatically.
    Standby,
}

/// The generation whose interrupted restores this start may roll back:
/// only at a normal start, and only that of the previous server process.
fn eligible_generation(
    mode: RecoveryMode,
    previous: &Result<Option<String>, String>,
) -> Option<String> {
    match (mode, previous) {
        (RecoveryMode::Primary, Ok(previous)) => previous.clone(),
        _ => None,
    }
}

/// Put back the originals of every restore a crash interrupted, report
/// every other unfinished restore, then start this process's generation.
/// Run at server start, before any app or workflow store opens app data.
pub(crate) fn recover_interrupted_restores(data_dir: &Path, mode: RecoveryMode) {
    let previous = read_generation(data_dir);
    if let Err(error) = &previous {
        tracing::error!(
            "Cannot tell which backup restores the previous server ran ({error}); none is rolled back at this start"
        );
    }
    recover_restore_dirs(
        data_dir,
        mode == RecoveryMode::Primary,
        eligible_generation(mode, &previous).as_deref(),
    );
    match mode {
        RecoveryMode::Primary | RecoveryMode::Successor => {
            if let Err(error) = start_generation_in(Some(data_dir)) {
                tracing::error!(
                    "{error}; restores this server runs are not rolled back automatically after a crash"
                );
            }
        }
        // Never recorded: a standby's restores are only ever reported.
        RecoveryMode::Standby => set_generation(nanoid::nanoid!(16)),
    }
}

/// Start a new generation when a standby takes over in its own process.
pub(crate) fn start_restore_generation(data_dir: &Path) {
    if let Err(error) = start_generation_in(Some(data_dir)) {
        tracing::error!(
            "{error}; restores this server runs are not rolled back automatically after a crash"
        );
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Recovery {
    /// A running restore holds the directory.
    InUse,
    /// The originals are back in place.
    RolledBack,
    /// The restore had finished or never started; the directory is gone.
    Cleaned,
    /// Nothing to do at this start; a later normal start finishes it.
    Deferred,
    /// Left in place for the operator, for this reason.
    Left(String),
}

fn recover_restore_dirs(data_dir: &Path, primary: bool, eligible: Option<&str>) {
    for dir in restore_dirs(data_dir) {
        match recover_restore_dir(&dir, primary, eligible) {
            Ok(Recovery::InUse) if primary => tracing::error!(
                path = %dir.display(),
                "Another process holds the lock of a backup restore at a normal start; it is left in place and never rolled back automatically"
            ),
            Ok(Recovery::InUse | Recovery::Deferred) => {}
            Ok(Recovery::RolledBack) => tracing::warn!(
                path = %dir.display(),
                "Rolled back a backup restore that was interrupted; the original data is in place"
            ),
            Ok(Recovery::Cleaned) => tracing::info!(
                path = %dir.display(),
                "Removed the rollback copy of a finished backup restore"
            ),
            Ok(Recovery::Left(reason)) => tracing::error!(
                path = %dir.display(),
                "A backup restore did not finish and is left in place: {reason}. The live app data may be the restored or a partly restored copy; the original data remains in this directory. Check the app data, keep what you need from the directory, then remove it."
            ),
            Err(error) => tracing::error!(
                path = %dir.display(),
                "Could not recover an interrupted backup restore; the original data remains in this directory: {error}"
            ),
        }
    }
}

/// Recover one rollback directory a previous server process left behind.
/// Only a `primary` start changes anything, and it rolls back only a record
/// of the `eligible` generation.
pub(super) fn recover_restore_dir(
    dir: &Path,
    primary: bool,
    eligible: Option<&str>,
) -> Result<Recovery, String> {
    if has_prefix(dir, FINISHED_DIR_PREFIX) {
        if !primary {
            return Ok(Recovery::Deferred);
        }
        super::mounts::remove_tree(dir)
            .map_err(|error| format!("remove previous data {}: {error}", dir.display()))?;
        return Ok(Recovery::Cleaned);
    }
    if has_prefix(dir, ABANDONED_DIR_PREFIX) {
        return Ok(Recovery::Left(
            "a server gave up on this restore and kept running".into(),
        ));
    }
    let lock = match std::fs::OpenOptions::new()
        .write(true)
        .open(dir.join(LOCK_FILE))
    {
        Ok(lock) => lock,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err("no restore lock; left in place for manual recovery".into());
        }
        Err(error) => return Err(format!("open restore lock: {error}")),
    };
    let deadline = std::time::Instant::now() + ORPHAN_LOCK_WAIT;
    loop {
        match rustix::fs::flock(&lock, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => break,
            Err(rustix::io::Errno::WOULDBLOCK) if !primary => return Ok(Recovery::InUse),
            // Never rolled back by a later start either: this start begins
            // a new generation.
            Err(rustix::io::Errno::WOULDBLOCK) if std::time::Instant::now() >= deadline => {
                return Ok(Recovery::InUse);
            }
            Err(rustix::io::Errno::WOULDBLOCK) => std::thread::sleep(Duration::from_millis(100)),
            Err(error) => return Err(format!("lock restore record: {error}")),
        }
    }
    let Some(intent) = read_intent(dir)? else {
        // Nothing changed, or the restore finished.
        if !primary {
            return Ok(Recovery::Deferred);
        }
        discard(dir)?;
        return Ok(Recovery::Cleaned);
    };
    if !primary {
        return Ok(Recovery::Left(
            "found by a standby, upgrade reload, or promotion start, which never rolls back".into(),
        ));
    }
    if eligible != Some(intent.generation.as_str()) {
        return Ok(Recovery::Left(
            "the server process that ran it is not the one this start replaces, so the data may have changed since"
                .into(),
        ));
    }
    roll_back(dir)?;
    Ok(Recovery::RolledBack)
}

fn has_prefix(dir: &Path, prefix: &str) -> bool {
    dir.file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with(prefix))
}

/// Rollback directories of every kind directly in `parent`.
fn restore_dirs_in(parent: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(parent)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .filter(|path| {
            [
                ROLLBACK_DIR_PREFIX,
                ABANDONED_DIR_PREFIX,
                FINISHED_DIR_PREFIX,
            ]
            .iter()
            .any(|prefix| has_prefix(path, prefix))
        })
        .collect();
    found.sort();
    found
}

/// Rollback directories below `apps/<name>/<environment>/`.
fn restore_dirs(data_dir: &Path) -> Vec<PathBuf> {
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
            found.extend(restore_dirs_in(&environment));
        }
    }
    found.sort();
    found
}

/// Run startup recovery as the next normal start after this process,
/// which wrote the records under test.
#[cfg(test)]
pub(super) fn recover_as_next_start(data_dir: &Path) {
    recover_restore_dirs(data_dir, true, Some(&current_generation()));
}

#[cfg(test)]
pub(super) fn recover_rollback_dir(dir: &Path) -> Result<Recovery, String> {
    recover_restore_dir(dir, true, Some(&current_generation()))
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
        self.dir.finish()
    }

    /// Put the original databases and sidecars back. Call only after
    /// stopping all processes that may open these databases. Runs in this
    /// process: it only inspects, renames, and unlinks files, and never
    /// opens a database.
    pub(super) fn rollback(self) -> Result<(), String> {
        self.dir.roll_back()
    }

    /// See [`RollbackDir::abandon`].
    pub(super) fn abandon(mut self, reason: &str) -> PathBuf {
        self.dir.abandon(reason)
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
    let mut dir = RollbackDir::create(data_root).map_err(SwapError::intact)?;
    let request = RestoreRequest::SwapDatabases {
        staged_root: staged_root.to_path_buf(),
        databases: databases.to_vec(),
        data_root: data_root.to_path_buf(),
        dir: dir.path().to_path_buf(),
        generation: current_generation(),
    };
    let not_intact = |dir: &mut RollbackDir, message: String| {
        let kept = dir.abandon(&message);
        SwapError {
            message: format!("{message}; original data remains in {}", kept.display()),
            originals_intact: false,
        }
    };
    match run_restore_request(&request) {
        Ok(reply) => Ok(DatabaseSwap {
            dir,
            summary: reply.summary.unwrap_or_default(),
            worker_pid: reply.pid,
        }),
        // The worker put everything back itself.
        Err(error) if error.reported && error.originals_intact => {
            if let Err(cleanup) = dir.finish() {
                tracing::warn!("Refused restore left its rollback directory: {cleanup}");
            }
            Err(SwapError::intact(error.message))
        }
        Err(error) if error.reported => Err(not_intact(&mut dir, error.message)),
        // The worker never ran (fork failed, or the binary is gone): there
        // is no record, and the rollback below only removes the directory.
        // Or it died: the record says what to undo. Either way the rollback
        // runs here, so it does not depend on starting another worker.
        Err(error) => {
            let message = if error.started {
                error.message
            } else {
                format!(
                    "{}; the restore worker never ran, so nothing was changed",
                    error.message
                )
            };
            match dir.roll_back() {
                Ok(()) => Err(SwapError::intact(message)),
                Err(rollback) => Err(SwapError {
                    message: format!("{message}; {rollback}"),
                    originals_intact: false,
                }),
            }
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
        /// Written into the intent record: the server's, not the worker's.
        generation: String,
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
    /// The worker process was started.
    pub(super) started: bool,
}

fn run_restore_request(request: &RestoreRequest) -> Result<RestoreReply, RequestError> {
    let crashed = |message: String| RequestError {
        message,
        originals_intact: false,
        reported: false,
        started: true,
    };
    let request = serde_json::to_string(request).map_err(|e| crashed(e.to_string()))?;
    let stdout =
        super::worker::run_worker_checked(&["--restore-worker".as_ref(), request.as_ref()])
            .map_err(|error| RequestError {
                started: error.started,
                ..crashed(error.message)
            })?;
    let reply: RestoreReply = serde_json::from_slice(&stdout)
        .map_err(|e| crashed(format!("restore worker sent an invalid reply: {e}")))?;
    match reply.error {
        Some(message) => Err(RequestError {
            message,
            originals_intact: reply.originals_intact,
            reported: true,
            started: true,
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
            generation,
        } => {
            set_generation(generation);
            match swap_databases(&staged_root, &databases, &data_root, &dir) {
                Ok(summary) => reply.summary = Some(summary),
                Err(error) => {
                    reply.error = Some(error.message);
                    reply.originals_intact = error.originals_intact;
                }
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
        let unused = RollbackDir::create(&data_root).unwrap();
        let unused_path = unused.path().to_path_buf();
        unused.leave_for_recovery();
        // A directory from before intent records: never guessed at.
        let legacy = data_root.with_file_name(format!("{ROLLBACK_DIR_PREFIX}legacy"));
        std::fs::create_dir(&legacy).unwrap();
        std::fs::write(legacy.join("kept"), b"original").unwrap();
        // A restore still running in this process.
        let running = RollbackDir::create_unchecked(&data_root).unwrap();
        // A restore a running server gave up on: reported, never applied.
        let abandoned = data_root.with_file_name(format!("{ABANDONED_DIR_PREFIX}given-up"));
        std::fs::create_dir(&abandoned).unwrap();
        std::fs::write(abandoned.join("kept"), b"original").unwrap();
        for work in ["tmp/backups/b1-abc/snapshot", "tmp/restore/restore-abc"] {
            std::fs::create_dir_all(temp.path().join(work)).unwrap();
        }

        assert_eq!(
            restore_dirs(temp.path()).len(),
            4,
            "{:?}",
            restore_dirs(temp.path())
        );
        recover_as_next_start(temp.path());
        sweep_backup_work_dirs(temp.path());
        assert!(!unused_path.exists());
        assert_eq!(std::fs::read(legacy.join("kept")).unwrap(), b"original");
        assert_eq!(std::fs::read(abandoned.join("kept")).unwrap(), b"original");
        assert!(running.path().exists());
        for kind in ["backups", "restore"] {
            let root = temp.path().join("tmp").join(kind);
            assert_eq!(std::fs::read_dir(root).unwrap().count(), 0, "{kind}");
        }
        running.finish().unwrap();
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

    /// A rollback directory with a record of a restore that a crash
    /// interrupted, below `<temp>/apps/demo/production/`.
    fn interrupted(temp: &Path, generation: &str) -> (PathBuf, PathBuf) {
        let data_root = temp.join("apps/demo/production/data");
        std::fs::create_dir_all(data_root.join("app")).unwrap();
        std::fs::write(data_root.join("app/marker"), b"restored").unwrap();
        let dir = RollbackDir::create(&data_root).unwrap();
        std::fs::create_dir_all(dir.path().join("data/app")).unwrap();
        std::fs::write(dir.path().join("data/app/marker"), b"original").unwrap();
        let mut intent = RestoreIntent::full(&data_root, true);
        intent.generation = generation.into();
        write_intent(dir.path(), &intent).unwrap();
        let path = dir.path().to_path_buf();
        dir.leave_for_recovery();
        (data_root, path)
    }

    #[test]
    fn only_a_normal_start_after_the_restoring_process_rolls_back() {
        let marker = |root: &Path| std::fs::read(root.join("app/marker")).unwrap();
        // The next normal start after the process that ran the restore.
        let temp = tempfile::tempdir().unwrap();
        let (data_root, dir) = interrupted(temp.path(), "g1");
        assert_eq!(
            recover_restore_dir(&dir, true, Some("g1")).unwrap(),
            Recovery::RolledBack
        );
        assert_eq!(marker(&data_root), b"original");
        assert!(restore_dirs(temp.path()).is_empty());

        // A start came after that process (an upgrade reload, a promotion,
        // or a normal start that could not roll it back): the data may
        // have changed since, so it is only reported.
        for (primary, eligible) in [(true, Some("g2")), (true, None), (false, Some("g1"))] {
            let temp = tempfile::tempdir().unwrap();
            let (data_root, dir) = interrupted(temp.path(), "g1");
            let recovered = recover_restore_dir(&dir, primary, eligible).unwrap();
            assert!(
                matches!(recovered, Recovery::Left(_)),
                "{primary} {eligible:?}: {recovered:?}"
            );
            assert_eq!(marker(&data_root), b"restored");
            assert!(dir.exists());
        }
    }

    #[test]
    fn only_a_normal_start_rolls_back_and_only_the_previous_generation() {
        let previous = Ok(Some("g1".to_string()));
        assert_eq!(
            eligible_generation(RecoveryMode::Primary, &previous).as_deref(),
            Some("g1")
        );
        for mode in [RecoveryMode::Successor, RecoveryMode::Standby] {
            assert_eq!(eligible_generation(mode, &previous), None, "{mode:?}");
        }
        assert_eq!(eligible_generation(RecoveryMode::Primary, &Ok(None)), None);
        assert_eq!(
            eligible_generation(RecoveryMode::Primary, &Err("unreadable".into())),
            None
        );

        let temp = tempfile::tempdir().unwrap();
        assert_eq!(read_generation(temp.path()), Ok(None));
        write_durably(temp.path(), GENERATION_TMP_FILE, GENERATION_FILE, b"g1\n").unwrap();
        assert_eq!(read_generation(temp.path()), Ok(Some("g1".into())));
    }

    /// A cleanup that stopped part way, even after the lock file went, is
    /// finished by the next normal start; other starts leave it.
    #[test]
    fn an_interrupted_cleanup_is_finished_at_the_next_normal_start() {
        let temp = tempfile::tempdir().unwrap();
        let env_dir = temp.path().join("apps/demo/production");
        let finished = env_dir.join(format!("{FINISHED_DIR_PREFIX}abc"));
        std::fs::create_dir_all(finished.join("data/app")).unwrap();
        std::fs::write(finished.join("data/app/leftover"), b"old").unwrap();
        assert_eq!(
            recover_restore_dir(&finished, false, None).unwrap(),
            Recovery::Deferred
        );
        assert!(finished.exists());
        recover_as_next_start(temp.path());
        assert!(!finished.exists());
        // A new restore of the app removes it too.
        std::fs::create_dir_all(&finished).unwrap();
        let dir = RollbackDir::create(&env_dir.join("data")).unwrap();
        assert!(!finished.exists());
        dir.finish().unwrap();
        assert!(restore_dirs(temp.path()).is_empty());
    }

    /// Committing removes the record before anything else, needs no new
    /// file, and when even that fails the directory is abandoned.
    #[test]
    fn a_commit_needs_no_free_space_and_a_failed_one_is_abandoned() {
        let temp = tempfile::tempdir().unwrap();
        let data_root = temp.path().join("apps/demo/production/data");
        std::fs::create_dir_all(&data_root).unwrap();
        let dir = RollbackDir::create(&data_root).unwrap();
        write_intent(dir.path(), &RestoreIntent::full(&data_root, true)).unwrap();
        // No new file can be created in the directory.
        std::fs::create_dir(dir.path().join(INTENT_TMP_FILE)).unwrap();
        dir.finish().unwrap();
        assert!(restore_dirs(temp.path()).is_empty());

        let dir = RollbackDir::create(&data_root).unwrap();
        write_intent(dir.path(), &RestoreIntent::full(&data_root, true)).unwrap();
        // The record cannot be removed.
        std::fs::remove_file(dir.path().join(INTENT_FILE)).unwrap();
        std::fs::create_dir_all(dir.path().join(INTENT_FILE).join("busy")).unwrap();
        let error = dir.finish().unwrap_err();
        assert!(error.contains("never rolled back"), "{error}");
        let [left] = restore_dirs(temp.path()).try_into().unwrap();
        assert!(
            has_prefix(&left, ABANDONED_DIR_PREFIX),
            "{}",
            left.display()
        );
        assert!(matches!(
            recover_restore_dir(&left, true, Some(&current_generation())).unwrap(),
            Recovery::Left(_)
        ));
    }
}
