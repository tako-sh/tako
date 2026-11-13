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
//!   that wrote it; otherwise it reports it and, as [`RecoveryMode`]
//!   describes, marks it so that no later start rolls it back.
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
/// In the work directory of a backup or restore, below `tmp/backups/` or
/// `tmp/restore/`, locked for as long as its work runs.
const WORK_LOCK_FILE: &str = "work.lock";
const INTENT_VERSION: u32 = 2;
/// Names the server generation: a new one at every start that serves apps.
const GENERATION_FILE: &str = "restore-generation";
const GENERATION_TMP_FILE: &str = "restore-generation.tmp";
/// How long a normal start waits for a restore worker that outlived its
/// server. On Linux it is killed when its server dies. One that still holds
/// the lock after the wait (another OS) is left to finish in its directory:
/// the start removes the lock file, so no later start rolls it back, and
/// the record keeps the app stopped (see [`unresolved_restore`]).
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
    /// changing its state, which needs no free disk space. Left behind, the
    /// live data is partly restored or in an unknown state.
    Swapping,
    /// Given up after the original data was put back in full.
    Original,
    /// Given up after the swap completed: the live data is the restored
    /// data in full.
    Restored,
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
///
/// A directory that is gone was retired already: only a retired one is
/// renamed for removal (see [`discard`]).
pub(super) fn retire(dir: &Path) -> Result<(), String> {
    if is_gone(dir) {
        return Ok(());
    }
    #[cfg(test)]
    if RETIRE_FAILS.get() {
        return Err("retire failed".into());
    }
    let path = dir.join(INTENT_FILE);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("remove {}: {error}", path.display())),
    }
    sync_dir(dir)
}

#[cfg(test)]
thread_local! {
    /// Makes [`retire`] fail on this thread, as a transient disk error
    /// would, while other writes still work.
    static RETIRE_FAILS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn is_gone(dir: &Path) -> bool {
    std::fs::symlink_metadata(dir).is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
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

/// Put back the originals the intent in `dir` describes. Repeating it after
/// it stopped part way is safe, and so is repeating it after the directory
/// was retired and removed. The caller owns the directory: its restore
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
/// of the directory afterwards leaves it for the next start to remove. When
/// the record cannot be retired after the undo, it is changed to say the
/// original data is live, if it can be.
pub(super) fn roll_back(dir: &Path) -> Result<(), String> {
    // A restore worker that rolled back and removed the directory itself.
    if is_gone(dir) {
        return Ok(());
    }
    undo(dir)?;
    if let Err(error) = retire(dir) {
        // The original data is live in full again: say so before the
        // caller gives the directory up, as `RollbackDir::roll_back` does.
        record_live_data(dir, IntentState::Original);
        return Err(error);
    }
    if let Err(error) = discard(dir) {
        tracing::warn!("Restore rolled back but its rollback copy remains: {error}");
    }
    Ok(())
}

/// Mark the restore in `dir` committed, then remove the originals.
#[cfg(test)]
pub(super) fn commit(dir: &Path) -> Result<(), String> {
    retire(dir)?;
    discard(dir)
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
    lock: File,
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
            lock,
            ended: false,
        })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    /// The locked descriptor, which a restore worker inherits: a flock
    /// belongs to the open file description, so the worker holds the lock
    /// for as long as it lives, even if this process dies first.
    fn lock_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self.lock.as_raw_fd()
    }

    fn exists(&self) -> bool {
        std::fs::symlink_metadata(&self.path).is_ok()
    }

    /// The live data needs nothing more from this directory: the restore
    /// was committed. If even retiring the record fails, the directory is
    /// abandoned.
    pub(super) fn finish(self) -> Result<(), String> {
        self.finish_with(IntentState::Restored)
    }

    /// [`Self::finish`] for a restore that changed nothing, or whose changes
    /// were put back.
    pub(super) fn finish_unchanged(self) -> Result<(), String> {
        self.finish_with(IntentState::Original)
    }

    fn finish_with(mut self, live: IntentState) -> Result<(), String> {
        self.ended = true;
        if !self.exists() {
            return Ok(());
        }
        if let Err(error) = retire(&self.path) {
            let kept = self.abandon(
                &format!("could not record the restore as finished: {error}"),
                live,
            );
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
        // Its worker rolled back and retired it already, then was stopped
        // while it removed the directory.
        if !self.exists() {
            return Ok(());
        }
        let undone = undo(&self.path);
        if let Err(error) = undone.clone().and_then(|()| retire(&self.path)) {
            let live = match undone {
                Ok(()) => IntentState::Original,
                Err(_) => IntentState::Swapping,
            };
            let kept = self.abandon(&format!("rollback failed: {error}"), live);
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
    /// `live` says what the live data is now; when the record cannot say
    /// so, it keeps saying that the live data may be partly restored.
    ///
    /// Renaming needs no free disk space. If it fails, removing the lock
    /// file also makes recovery leave the directory alone; if that fails
    /// too, starting a new server generation does, unless the disk refuses
    /// that write as well.
    pub(super) fn abandon(&mut self, reason: &str, live: IntentState) -> PathBuf {
        self.ended = true;
        record_live_data(&self.path, live);
        let marked = mark_abandoned(&self.path).map(|path| self.path = path);
        match marked {
            Ok(()) => tracing::error!(
                path = %self.path.display(),
                "Gave up on a backup restore: {reason}. {} The original data remains in this directory; recovery never applies it. See {UNFINISHED_RESTORE_DOCS}",
                live_data_summary(&self.path)
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
        if !self.ended && self.exists() {
            self.abandon(
                "the restore stopped before it was committed or rolled back",
                IntentState::Swapping,
            );
        }
        // Explicitly: a child another thread forked holds a copy of the
        // descriptor until it execs, and closing ours alone would leave the
        // lock held until then. No worker of this restore is alive now.
        let _ = rustix::fs::flock(&self.lock, FlockOperation::Unlock);
    }
}

/// Where the docs say how to resolve a restore Tako gave up on.
pub(super) const UNFINISHED_RESTORE_DOCS: &str =
    "https://tako.sh/docs/backups#resolve-an-unfinished-restore";

/// Record in the intent record of `dir` what the live data is now that the
/// restore is given up. Without a record nothing changed live, or the
/// restore was retired; a record that cannot be updated keeps saying the
/// live data may be partly restored.
fn record_live_data(dir: &Path, live: IntentState) {
    if live == IntentState::Swapping {
        return;
    }
    let recorded = match read_intent(dir) {
        Ok(Some(mut intent)) => {
            intent.state = live;
            write_intent(dir, &intent).map_err(|error| error.to_string())
        }
        Ok(None) => Ok(()),
        Err(error) => Err(error),
    };
    if let Err(error) = recorded {
        tracing::warn!(
            path = %dir.display(),
            "Could not record what the live data is after giving up on a backup restore: {error}"
        );
    }
}

/// The live data an unfinished restore in `dir` left, as its record says.
/// `None` when the live data is whole: the original or the restored data.
fn partial_restore(dir: &Path) -> Option<String> {
    match read_intent(dir) {
        Ok(None) => None,
        Ok(Some(intent)) if intent.state != IntentState::Swapping => None,
        Ok(Some(_)) => Some("its record says the swap did not finish".into()),
        Err(error) => Some(format!("its record cannot be read ({error})")),
    }
}

fn live_data_summary(dir: &Path) -> &'static str {
    if partial_restore(dir).is_some() {
        return "The live data may be partly restored, so Tako does not start the app until the directory is resolved and removed.";
    }
    match read_intent(dir) {
        Ok(Some(intent)) if intent.state == IntentState::Restored => {
            "The live data is the restored data."
        }
        Ok(Some(_)) => "The live data is the original data.",
        _ => "The live data is either the original or the restored data, in full.",
    }
}

/// An unfinished restore beside `data_root` that leaves the live data
/// partly restored or in an unknown state. Its app must not start until
/// the operator resolves it and removes the directory.
pub(super) fn unresolved_restore(data_root: &Path) -> Option<PathBuf> {
    let parent = data_root.parent()?;
    restore_dirs_in(parent)
        .into_iter()
        .filter(|dir| !has_prefix(dir, FINISHED_DIR_PREFIX))
        .find(|dir| partial_restore(dir).is_some())
}

/// Read-only inventory: do not recover or remove records while reporting status.
pub(super) fn unfinished_restores(data_root: &Path) -> Vec<tako_core::BackupRestoreStatus> {
    use tako_core::{BackupRestoreState, BackupRestoreStatus};
    let Some(parent) = data_root.parent() else {
        return Vec::new();
    };
    restore_dirs_in(parent)
        .into_iter()
        .filter(|dir| !has_prefix(dir, FINISHED_DIR_PREFIX))
        .map(|dir| {
            let state = if has_prefix(&dir, ABANDONED_DIR_PREFIX) {
                BackupRestoreState::Abandoned
            } else if lock_is_held(&dir) {
                BackupRestoreState::InProgress
            } else {
                BackupRestoreState::Unfinished
            };
            BackupRestoreStatus {
                path: dir.display().to_string(),
                state,
                blocks_app: partial_restore(&dir).is_some(),
                message: if state == BackupRestoreState::InProgress {
                    "A restore is in progress; wait for it to finish.".into()
                } else {
                    format!(
                        "{} Resolve the record before another restore: {UNFINISHED_RESTORE_DOCS}",
                        live_data_summary(&dir)
                    )
                },
            }
        })
        .collect()
}

/// Why an app with the unresolved restore `dir` stays stopped.
pub(super) fn unresolved_restore_message(dir: &Path) -> String {
    format!(
        "A backup restore of this app did not finish and may have left its data partly restored; the original data is in {}. Tako does not start the app until you resolve it and remove that directory: see {UNFINISHED_RESTORE_DOCS}",
        dir.display()
    )
}

/// Make sure no later start rolls back the restore in `dir`, without
/// touching what it holds: rename it to an abandoned directory, or failing
/// that remove its lock file, or failing that start a new server
/// generation. Returns where the directory is now.
fn mark_abandoned(dir: &Path) -> Result<PathBuf, String> {
    let abandoned = renamed(dir, ABANDONED_DIR_PREFIX);
    match std::fs::rename(dir, &abandoned) {
        Ok(()) => {
            if let Some(parent) = abandoned.parent() {
                let _ = sync_dir(parent);
            }
            Ok(abandoned)
        }
        Err(rename) => remove_lock(dir)
            .map_err(|unlink| format!("rename: {rename}; {unlink}"))
            .or_else(|error| {
                // Rollback directories sit at <data dir>/apps/<name>/<environment>/.
                start_generation_in(dir.ancestors().nth(4))
                    .map_err(|generation| format!("{error}; {generation}"))
            })
            .map(|()| dir.to_path_buf()),
    }
}

/// Recovery never rolls back a directory without a lock file. Removing it
/// needs no free disk space and leaves a process that holds the lock alone.
fn remove_lock(dir: &Path) -> Result<(), String> {
    std::fs::remove_file(dir.join(LOCK_FILE))
        .map_err(|error| format!("remove lock: {error}"))
        .and_then(|()| sync_dir(dir))
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
            "A previous restore of this app did not finish and left {}. Tako does not start another restore over it: resolve it as described in {UNFINISHED_RESTORE_DOCS}, remove the directory, then retry.",
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
    record_generation(data_dir.ok_or("no server data directory")?, &generation)
}

fn record_generation(data_dir: &Path, generation: &str) -> Result<(), String> {
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
    /// A normal start while no other server process serves this data
    /// directory (startup checks the drain activity lease every serving
    /// process holds; otherwise it starts as a successor).
    /// Rolls back records of the previous generation whose lock is free and
    /// marks every other record abandoned, so that no later start rolls one
    /// back even if this start cannot record its generation.
    Primary,
    /// An upgrade reload, a native promotion child, an in-process standby
    /// promotion, or a normal start beside another serving process: each
    /// takes over from or runs beside a server that may still be running.
    /// Rolls nothing back, and marks abandoned every record whose lock is
    /// free, so that no later start rolls one back even if this start
    /// cannot record its generation; a record whose restore still runs is
    /// left as it is. Removes what finished restores left. Then starts a
    /// new generation, so records of the server it replaces are only
    /// reported at later starts.
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
    set_generation(recover_restores_in(data_dir, mode));
}

/// [`recover_interrupted_restores`], returning the generation this process
/// adopts instead of setting it.
fn recover_restores_in(data_dir: &Path, mode: RecoveryMode) -> String {
    let previous = read_generation(data_dir);
    if let Err(error) = &previous {
        tracing::error!(
            "Cannot tell which backup restores the previous server ran ({error}); none is rolled back at this start"
        );
    }
    recover_restore_dirs(
        data_dir,
        mode,
        eligible_generation(mode, &previous).as_deref(),
    );
    let generation = nanoid::nanoid!(16);
    match mode {
        RecoveryMode::Primary | RecoveryMode::Successor => {
            if let Err(error) = record_generation(data_dir, &generation) {
                let earlier = if mode == RecoveryMode::Primary {
                    "this start rolled back or marked abandoned every one it could"
                } else {
                    "this start marked abandoned every one whose process is gone"
                };
                tracing::error!(
                    "{error}. The file still names an earlier server process, so a restore this process runs and does not finish is never rolled back automatically: the next start reports it and keeps its app stopped. Restores of earlier processes are not affected: {earlier}."
                );
            }
        }
        // Never recorded: a standby's restores are only ever reported.
        RecoveryMode::Standby => {}
    }
    generation
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

fn recover_restore_dirs(data_dir: &Path, mode: RecoveryMode, eligible: Option<&str>) {
    let primary = mode == RecoveryMode::Primary;
    for dir in restore_dirs(data_dir) {
        let recovered = if mode == RecoveryMode::Successor && has_prefix(&dir, FINISHED_DIR_PREFIX)
        {
            remove_finished(&dir)
        } else {
            recover_restore_dir(&dir, primary, eligible)
        };
        // No record stays where a later start could roll it back: the
        // generation this start records may never reach the disk. A
        // directory whose lock another process still holds is not renamed:
        // that process may still write into it by its path. Only a normal
        // start, after its wait, removes that lock file.
        let marked = match &recovered {
            _ if mode == RecoveryMode::Standby || !has_prefix(&dir, ROLLBACK_DIR_PREFIX) => None,
            Ok(Recovery::InUse) if primary => Some(remove_lock(&dir).map(|()| dir.clone())),
            // Beside other server processes, only a record whose lock this
            // start took: an error may hide a lock that is held.
            Ok(Recovery::Left(_)) if std::fs::symlink_metadata(&dir).is_ok() => {
                Some(mark_abandoned(&dir))
            }
            Err(_) if primary && std::fs::symlink_metadata(&dir).is_ok() => {
                Some(mark_abandoned(&dir))
            }
            _ => None,
        };
        let marked_path = marked
            .as_ref()
            .and_then(|marked| marked.as_ref().ok())
            .cloned();
        match marked {
            Some(Ok(path)) if path != dir => tracing::info!(
                path = %path.display(),
                "Marked an unfinished backup restore abandoned; no later start rolls it back"
            ),
            Some(Err(error)) => tracing::error!(
                path = %dir.display(),
                "Could not mark an unfinished backup restore abandoned ({error}); a later server start may roll it back. Move this directory out of the app directory before restarting the server."
            ),
            _ => {}
        }
        match recovered {
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
                "A backup restore did not finish and is left in place: {reason}. {} The original data remains in this directory. See {UNFINISHED_RESTORE_DOCS}",
                live_data_summary(marked_path.as_deref().unwrap_or(&dir))
            ),
            Err(error) => tracing::error!(
                path = %dir.display(),
                "Could not recover an interrupted backup restore ({error}); it is left in place. {} See {UNFINISHED_RESTORE_DOCS}",
                live_data_summary(marked_path.as_deref().unwrap_or(&dir))
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
        return remove_finished(dir);
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
            // The caller removes its lock file, so no later start rolls it
            // back either.
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
            "found by a start that never rolls back: a standby, an upgrade reload, a promotion, or a normal start beside another server process".into(),
        ));
    }
    if intent.state != IntentState::Swapping {
        return Ok(Recovery::Left(
            "a server gave up on this restore and recorded what the live data is".into(),
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

/// Remove what a finished restore left in `dir`, which nothing needs. Safe
/// while another process removes it too.
fn remove_finished(dir: &Path) -> Result<Recovery, String> {
    super::mounts::remove_tree(dir)
        .map_err(|error| format!("remove previous data {}: {error}", dir.display()))?;
    Ok(Recovery::Cleaned)
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
    recover_restore_dirs(data_dir, RecoveryMode::Primary, Some(&current_generation()));
}

#[cfg(test)]
pub(super) fn recover_rollback_dir(dir: &Path) -> Result<Recovery, String> {
    recover_restore_dir(dir, true, Some(&current_generation()))
}

/// Lock the new, empty work directory `dir` of a backup or restore for as
/// long as the returned file is open: [`sweep_backup_work_dirs`] leaves it.
pub(super) fn lock_work_dir(dir: &Path) -> io::Result<File> {
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dir.join(WORK_LOCK_FILE))?;
    rustix::fs::flock(&lock, FlockOperation::NonBlockingLockExclusive).map_err(io::Error::from)?;
    Ok(lock)
}

/// The lock of the work directory `dir`, once its work has ended; `None`
/// while its work runs. A directory without a lock file is an older
/// server's, or one whose creation stopped part way.
fn ended_work_lock(dir: &Path) -> io::Result<Option<File>> {
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .open(dir.join(WORK_LOCK_FILE))?;
    match rustix::fs::flock(&lock, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Some(lock)),
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Remove the backup and restore work directories whose work has ended.
/// Only a normal start, which no other server process runs beside, also
/// removes those without a lock it can check.
pub(crate) fn sweep_backup_work_dirs(data_dir: &Path, mode: RecoveryMode) {
    for kind in ["backups", "restore"] {
        let root = data_dir.join("tmp").join(kind);
        for entry in std::fs::read_dir(&root).into_iter().flatten().flatten() {
            let path = entry.path();
            let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
            let lock = if is_dir {
                ended_work_lock(&path)
            } else {
                Err(io::ErrorKind::NotFound.into())
            };
            // Held until the directory is gone.
            let _lock = match lock {
                Ok(Some(lock)) => Some(lock),
                Ok(None) => {
                    tracing::info!(
                        path = %path.display(),
                        "Left a backup work directory whose work still runs"
                    );
                    continue;
                }
                Err(_) if mode == RecoveryMode::Primary => None,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    tracing::warn!(
                        path = %path.display(),
                        "Left a backup work directory whose lock cannot be checked: {error}"
                    );
                    continue;
                }
            };
            let removed = if is_dir {
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

    /// See [`RollbackDir::abandon`]. The swap completed, so the live
    /// databases are the restored ones.
    pub(super) fn abandon(mut self, reason: &str) -> PathBuf {
        self.dir.abandon(reason, IntentState::Restored)
    }
}

/// A database swap only checks, links, and renames files; a worker still
/// running after this long is stuck and is stopped and rolled back.
pub(super) const RESTORE_WORKER_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// [`swap_databases_in_worker`] on the blocking pool, so a slow worker never
/// holds up a runtime thread.
pub(super) async fn swap_databases_off_runtime(
    staged_root: PathBuf,
    databases: Vec<PathBuf>,
    data_root: PathBuf,
    timeout: Duration,
) -> Result<DatabaseSwap, SwapError> {
    tokio::task::spawn_blocking(move || {
        swap_databases_in_worker(&staged_root, &databases, &data_root, timeout)
    })
    .await
    // A panic dropped the rollback directory, which abandoned it.
    .unwrap_or_else(|error| {
        Err(SwapError {
            message: format!("restore worker task failed: {error}"),
            originals_intact: false,
        })
    })
}

/// Swap the staged databases into the live tree in a worker process. Its
/// checks open the live databases, and closing a descriptor to a SQLite
/// file drops every POSIX lock the closing process holds on it. A worker
/// that runs past `timeout` is killed and its swap rolled back.
pub(super) fn swap_databases_in_worker(
    staged_root: &Path,
    databases: &[PathBuf],
    data_root: &Path,
    timeout: Duration,
) -> Result<DatabaseSwap, SwapError> {
    let mut dir = RollbackDir::create(data_root).map_err(SwapError::intact)?;
    let request = RestoreRequest::SwapDatabases {
        staged_root: staged_root.to_path_buf(),
        databases: databases.to_vec(),
        data_root: data_root.to_path_buf(),
        dir: dir.path().to_path_buf(),
        generation: current_generation(),
        lock_fd: dir.lock_fd(),
    };
    let not_intact = |dir: &mut RollbackDir, message: String| {
        let kept = dir.abandon(&message, IntentState::Swapping);
        SwapError {
            message: format!("{message}; original data remains in {}", kept.display()),
            originals_intact: false,
        }
    };
    match run_restore_request(&request, Some(timeout), Some(dir.lock_fd())) {
        Ok(reply) => Ok(DatabaseSwap {
            dir,
            summary: reply.summary.unwrap_or_default(),
            worker_pid: reply.pid,
        }),
        // The worker put everything back itself.
        Err(error) if error.reported && error.originals_intact => {
            if let Err(cleanup) = dir.finish_unchanged() {
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
        /// The inherited descriptor holding the lock of `dir`.
        lock_fd: i32,
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

fn run_restore_request(
    request: &RestoreRequest,
    timeout: Option<Duration>,
    inherit: Option<std::os::fd::RawFd>,
) -> Result<RestoreReply, RequestError> {
    let crashed = |message: String| RequestError {
        message,
        originals_intact: false,
        reported: false,
        started: true,
    };
    let request = serde_json::to_string(request).map_err(|e| crashed(e.to_string()))?;
    let stdout = super::worker::run_worker_checked(
        &["--restore-worker".as_ref(), request.as_ref()],
        timeout,
        inherit,
    )
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

/// Check that descriptor `fd` of this process holds the lock of `dir`.
fn check_lock(dir: &Path, fd: i32) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let lock = std::fs::metadata(dir.join(LOCK_FILE))
        .map_err(|e| format!("inspect restore lock in {}: {e}", dir.display()))?;
    // SAFETY: fstat and flock only use the descriptor number; one that is
    // not open fails with EBADF.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut stat) } != 0 {
        return Err(format!(
            "restore lock descriptor {fd}: {}",
            io::Error::last_os_error()
        ));
    }
    if stat.st_dev as u64 != lock.dev() || stat.st_ino as u64 != lock.ino() {
        return Err(format!(
            "descriptor {fd} is not the restore lock of {}",
            dir.display()
        ));
    }
    // Locking again through the description that holds the lock succeeds;
    // through any other it would block.
    if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(format!(
            "the restore worker does not hold the restore lock of {}",
            dir.display()
        ));
    }
    Ok(())
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
            lock_fd,
        } => {
            set_generation(generation);
            // Never change data unless this process holds the lock, so no
            // recovery can run beside it.
            let swapped = check_lock(&dir, lock_fd)
                .map_err(SwapError::intact)
                .and_then(|()| swap_databases(&staged_root, &databases, &data_root, &dir));
            match swapped {
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
        sweep_backup_work_dirs(temp.path(), RecoveryMode::Primary);
        assert!(!unused_path.exists());
        // Marked abandoned, so no later start guesses at it either.
        let legacy = renamed(&legacy, ABANDONED_DIR_PREFIX);
        assert_eq!(std::fs::read(legacy.join("kept")).unwrap(), b"original");
        assert_eq!(std::fs::read(abandoned.join("kept")).unwrap(), b"original");
        assert!(running.path().exists());
        for kind in ["backups", "restore"] {
            let root = temp.path().join("tmp").join(kind);
            assert_eq!(std::fs::read_dir(root).unwrap().count(), 0, "{kind}");
        }
        running.finish().unwrap();
    }

    /// A backup or restore holds the lock in its work directory while it
    /// runs: every start but a standby's removes the work directories whose
    /// work has ended, keeps those whose work runs, and only a normal start,
    /// beside no other server process, removes one without a lock.
    #[test]
    fn starts_sweep_only_the_work_dirs_whose_work_has_ended() {
        for mode in [RecoveryMode::Primary, RecoveryMode::Successor] {
            let temp = tempfile::tempdir().unwrap();
            let work = |path: &str| {
                let dir = temp.path().join(path);
                std::fs::create_dir_all(dir.join("snapshot")).unwrap();
                dir
            };
            let running = work("tmp/backups/b1-running");
            let _running = lock_work_dir(&running).unwrap();
            let ended = work("tmp/restore/restore-ended");
            drop(lock_work_dir(&ended).unwrap());
            let unlocked = work("tmp/backups/b0-unlocked");

            sweep_backup_work_dirs(temp.path(), mode);
            assert!(running.exists(), "work that runs must be kept");
            assert!(!ended.exists(), "work that ended must be removed");
            assert!(
                unlocked.exists() == (mode != RecoveryMode::Primary),
                "only a normal start may remove a work directory without a lock"
            );
        }
    }

    /// What a finished restore left is never needed: every start but a
    /// standby's removes it.
    #[test]
    fn starts_beside_other_server_processes_remove_finished_restores() {
        for (mode, removed) in [
            (RecoveryMode::Successor, true),
            (RecoveryMode::Standby, false),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let finished = temp
                .path()
                .join(format!("apps/demo/production/{FINISHED_DIR_PREFIX}abc"));
            std::fs::create_dir_all(finished.join("data/app")).unwrap();
            recover_restores_in(temp.path(), mode);
            assert!(
                finished.exists() != removed,
                "a finished restore must be removed by every start but a standby's"
            );
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

    /// A restore worker only changes data while it holds the restore lock
    /// through the descriptor it inherited from the server.
    #[test]
    fn a_restore_worker_checks_that_it_holds_the_lock() {
        use std::os::fd::AsRawFd;
        let temp = tempfile::tempdir().unwrap();
        let data_root = temp.path().join("apps/demo/production/data");
        std::fs::create_dir_all(&data_root).unwrap();
        let dir = RollbackDir::create(&data_root).unwrap();
        check_lock(dir.path(), dir.lock_fd()).unwrap();
        let other = std::fs::File::open(dir.path().join(LOCK_FILE)).unwrap();
        let error = check_lock(dir.path(), other.as_raw_fd()).unwrap_err();
        assert!(error.contains("does not hold the restore lock"), "{error}");
        let unrelated = std::fs::File::open(temp.path()).unwrap();
        let error = check_lock(dir.path(), unrelated.as_raw_fd()).unwrap_err();
        assert!(error.contains("is not the restore lock"), "{error}");
        let error = check_lock(dir.path(), 1_000_000).unwrap_err();
        assert!(error.contains("Bad file descriptor"), "{error}");
        dir.finish().unwrap();
    }

    /// A rollback directory with a record of a restore that a crash
    /// interrupted, below `<temp>/apps/demo/production/`.
    fn interrupted(temp: &Path, generation: &str) -> (PathBuf, PathBuf) {
        interrupted_app(temp, "demo", generation)
    }

    /// [`interrupted`] for the app `name`.
    fn interrupted_app(temp: &Path, name: &str, generation: &str) -> (PathBuf, PathBuf) {
        let data_root = temp.join("apps").join(name).join("production/data");
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

    /// A normal start that leaves a record in place and then fails to
    /// record its generation: the file still names the generation of the
    /// record, yet no later normal start rolls it back over what the app
    /// wrote since.
    #[test]
    fn a_failed_generation_write_never_makes_a_left_record_eligible_later() {
        let marker = |root: &Path| std::fs::read(root.join("app/marker")).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let data_dir = temp.path();
        record_generation(data_dir, "gA").unwrap();
        // Its lock is still held after the wait.
        let (held_root, held) = interrupted_app(data_dir, "held", "gA");
        let holder = std::fs::File::open(held.join(LOCK_FILE)).unwrap();
        rustix::fs::flock(&holder, FlockOperation::NonBlockingLockExclusive).unwrap();
        // Its rollback fails: the restored tree cannot be moved aside.
        let (broken_root, broken) = interrupted_app(data_dir, "broken", "gA");
        std::fs::create_dir_all(broken.join("restored/busy")).unwrap();
        // Its lock cannot be opened.
        let (unopened_root, unopened) = interrupted_app(data_dir, "unopened", "gA");
        std::fs::remove_file(unopened.join(LOCK_FILE)).unwrap();
        std::fs::create_dir(unopened.join(LOCK_FILE)).unwrap();
        // The generation cannot be recorded.
        std::fs::create_dir_all(data_dir.join(GENERATION_TMP_FILE).join("busy")).unwrap();

        recover_restores_in(data_dir, RecoveryMode::Primary);
        assert_eq!(read_generation(data_dir), Ok(Some("gA".into())));

        // Whatever kept each record from rolling back is gone by the next
        // normal start, and the generation file still says gA.
        drop(holder);
        std::fs::remove_dir_all(data_dir.join(GENERATION_TMP_FILE)).unwrap();
        for app in ["held", "broken", "unopened"] {
            let env_dir = data_dir.join("apps").join(app).join("production");
            for dir in restore_dirs_in(&env_dir) {
                let _ = std::fs::remove_dir_all(dir.join("restored"));
                if dir.join(LOCK_FILE).is_dir() {
                    std::fs::remove_dir(dir.join(LOCK_FILE)).unwrap();
                    std::fs::write(dir.join(LOCK_FILE), b"").unwrap();
                }
            }
        }
        assert_eq!(read_generation(data_dir), Ok(Some("gA".into())));
        recover_restores_in(data_dir, RecoveryMode::Primary);

        for (root, dir) in [
            (held_root, held),
            (broken_root, broken),
            (unopened_root, unopened),
        ] {
            assert!(
                marker(&root) == b"restored",
                "a record left at a normal start must never be rolled back later"
            );
            let left = restore_dirs_in(root.parent().unwrap());
            assert!(
                left.len() == 1 && read_intent(&left[0]).is_ok_and(|intent| intent.is_some()),
                "the record must stay for the operator"
            );
            assert!(!dir.exists() || dir == left[0]);
        }
    }

    /// A start beside another server process rolls nothing back, but marks
    /// abandoned every record no running process holds: when it then fails
    /// to record its generation, the next normal start must not find such a
    /// record eligible and roll it back over what was written since.
    #[test]
    fn a_successor_start_marks_free_records_so_no_later_start_rolls_them_back() {
        let marker = |root: &Path| std::fs::read(root.join("app/marker")).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let data_dir = temp.path();
        record_generation(data_dir, "gA").unwrap();
        // Left by the dead process gA.
        let (free_root, free) = interrupted_app(data_dir, "free", "gA");
        // Its restore still runs in another process.
        let (held_root, held) = interrupted_app(data_dir, "held", "gA");
        let holder = std::fs::File::open(held.join(LOCK_FILE)).unwrap();
        rustix::fs::flock(&holder, FlockOperation::NonBlockingLockExclusive).unwrap();
        // The generation cannot be recorded.
        std::fs::create_dir_all(data_dir.join(GENERATION_TMP_FILE).join("busy")).unwrap();

        recover_restores_in(data_dir, RecoveryMode::Successor);
        assert!(
            read_generation(data_dir) == Ok(Some("gA".into())),
            "the generation write must have failed"
        );
        assert!(
            !free.exists() && renamed(&free, ABANDONED_DIR_PREFIX).exists(),
            "a record no running process holds must be marked abandoned"
        );
        assert!(
            held.join(LOCK_FILE).exists() && read_intent(&held).is_ok_and(|i| i.is_some()),
            "a record a running process holds must be left as it is"
        );
        assert!(
            marker(&held_root) == b"restored",
            "a successor start must roll nothing back"
        );

        std::fs::remove_dir_all(data_dir.join(GENERATION_TMP_FILE)).unwrap();
        recover_restores_in(data_dir, RecoveryMode::Primary);
        assert!(
            marker(&free_root) == b"restored",
            "a record a successor start found free must never be rolled back later"
        );
        drop(holder);
    }

    /// A worker that rolled back its own swap and was killed while it
    /// removed the directory: the server's rollback finds the restore
    /// finished, and says the originals are in place.
    #[test]
    fn rolling_back_a_restore_its_worker_already_rolled_back_succeeds() {
        let temp = tempfile::tempdir().unwrap();
        let data_root = temp.path().join("apps/demo/production/data");
        std::fs::create_dir_all(&data_root).unwrap();
        let dir = RollbackDir::create(&data_root).unwrap();
        write_intent(dir.path(), &RestoreIntent::full(&data_root, true)).unwrap();
        // The worker's own rollback, which also removes the directory.
        roll_back(dir.path()).unwrap();
        assert!(!dir.path().exists());
        assert!(
            dir.roll_back().is_ok(),
            "the server's rollback must see the restore already rolled back"
        );
        assert!(restore_dirs(temp.path()).is_empty());
    }

    /// A rollback or commit that cannot retire its record records what the
    /// live data is before the directory is marked abandoned, so whole data
    /// does not keep the app stopped: startup recovery that put the original
    /// data back, as a running server's rollback does, and a commit.
    #[test]
    fn a_restore_that_cannot_retire_its_record_records_what_is_live() {
        let temp = tempfile::tempdir().unwrap();
        let (data_root, _) = interrupted(temp.path(), "g1");
        let live = |state| {
            let left = restore_dirs_in(data_root.parent().unwrap());
            left.len() == 1
                && has_prefix(&left[0], ABANDONED_DIR_PREFIX)
                && read_intent(&left[0])
                    .is_ok_and(|intent| intent.is_some_and(|i| i.state == state))
        };
        RETIRE_FAILS.set(true);
        recover_restore_dirs(temp.path(), RecoveryMode::Primary, Some("g1"));
        RETIRE_FAILS.set(false);
        assert!(
            std::fs::read(data_root.join("app/marker")).unwrap() == b"original",
            "recovery must put the original data back"
        );
        assert!(
            live(IntentState::Original),
            "the record must say the original data is live"
        );
        assert!(
            unresolved_restore(&data_root).is_none(),
            "whole original data must not keep the app stopped"
        );
        for dir in restore_dirs_in(data_root.parent().unwrap()) {
            std::fs::remove_dir_all(dir).unwrap();
        }

        let dir = RollbackDir::create(&data_root).unwrap();
        write_intent(dir.path(), &RestoreIntent::full(&data_root, true)).unwrap();
        RETIRE_FAILS.set(true);
        let committed = dir.finish();
        RETIRE_FAILS.set(false);
        assert!(committed.is_err(), "the commit must report the failure");
        assert!(
            live(IntentState::Restored),
            "the record must say the restored data is live"
        );
        assert!(
            unresolved_restore(&data_root).is_none(),
            "whole restored data must not keep the app stopped"
        );
    }

    /// Giving up records what the live data is; recovery never rolls back
    /// such a record, and only partly restored data keeps the app stopped.
    #[test]
    fn a_given_up_restore_records_what_the_live_data_is() {
        let temp = tempfile::tempdir().unwrap();
        let data_root = temp.path().join("apps/demo/production/data");
        std::fs::create_dir_all(&data_root).unwrap();
        for (live, partial) in [
            (IntentState::Swapping, true),
            (IntentState::Original, false),
            (IntentState::Restored, false),
        ] {
            let mut dir = RollbackDir::create(&data_root).unwrap();
            write_intent(dir.path(), &RestoreIntent::full(&data_root, true)).unwrap();
            let kept = dir.abandon("given up", live);
            drop(dir);
            let recorded = read_intent(&kept).unwrap().unwrap();
            assert_eq!(recorded.state, live);
            assert_eq!(unresolved_restore(&data_root).is_some(), partial);

            // Even named as an interrupted restore of the eligible generation.
            let interrupted = renamed(&kept, ROLLBACK_DIR_PREFIX);
            std::fs::rename(&kept, &interrupted).unwrap();
            std::fs::write(interrupted.join(LOCK_FILE), b"").unwrap();
            let recovered = recover_restore_dir(&interrupted, true, Some(&recorded.generation));
            if live == IntentState::Swapping {
                assert!(matches!(recovered, Ok(Recovery::RolledBack)));
                assert!(unresolved_restore(&data_root).is_none());
            } else {
                assert!(matches!(recovered, Ok(Recovery::Left(_))));
                std::fs::remove_dir_all(&interrupted).unwrap();
            }
        }
        // Dropped without an end: the record still says partly restored.
        let dir = RollbackDir::create(&data_root).unwrap();
        write_intent(dir.path(), &RestoreIntent::full(&data_root, true)).unwrap();
        drop(dir);
        assert!(unresolved_restore(&data_root).is_some());
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
