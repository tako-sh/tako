//! What app data directory archives, which earlier releases made, left in
//! the data directory. A normal start removes their work directories and
//! the rollback copies of restores that finished, and reports the rollback
//! copy of a restore that never finished: it holds the app data that
//! restore replaced, and only an operator can tell which copy to keep.

use std::io;
use std::path::{Path, PathBuf};

use super::RecoveryMode;

/// Rollback copies beside an app's data directory, by the state of their
/// restore.
const UNFINISHED: [&str; 2] = [".data-restore-prev-", ".data-restore-abandoned-"];
const FINISHED: &str = ".data-restore-done-";

pub(crate) fn sweep_retired_archive_state(data_dir: &Path, mode: RecoveryMode) {
    // An earlier process may still run beside a successor or a standby.
    if mode != RecoveryMode::Primary {
        return;
    }
    let tmp = data_dir.join("tmp");
    let work = [tmp.join("backups"), tmp.join("restore")];
    for path in work {
        remove(&path, std::fs::remove_dir_all(&path));
    }
    for name in ["restore-generation", "restore-generation.tmp"] {
        let path = data_dir.join(name);
        remove(&path, std::fs::remove_file(&path));
    }
    for path in rollback_dirs(data_dir) {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if name.starts_with(FINISHED) {
            remove(&path, std::fs::remove_dir_all(&path));
        } else if UNFINISHED.iter().any(|prefix| name.starts_with(prefix)) {
            tracing::warn!(
                path = %path.display(),
                "An app data restore of an earlier release did not finish. This directory holds the data it replaced; check the app's data directory beside it, then delete the one you don't keep"
            );
        }
    }
}

fn remove(path: &Path, result: io::Result<()>) {
    match result {
        Ok(()) => tracing::info!(
            path = %path.display(),
            "Removed what app data archives of an earlier release left"
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(
            path = %path.display(),
            "Cannot remove what app data archives of an earlier release left: {error}"
        ),
    }
}

/// Rollback copies sat beside the shared data directory and beside each
/// app environment's.
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
    let mut parents = vec![data_dir.join("shared")];
    for app in children(&data_dir.join("apps")) {
        parents.extend(children(&app));
    }
    let mut found: Vec<PathBuf> = parents
        .iter()
        .flat_map(|parent| children(parent))
        .filter(|path| {
            path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.starts_with(FINISHED) || UNFINISHED.iter().any(|p| name.starts_with(p))
            })
        })
        .collect();
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_normal_start_removes_archive_work_and_keeps_unfinished_restores() {
        let temp = tempfile::TempDir::new().unwrap();
        let data = temp.path();
        let env = data.join("apps/shop/production");
        for dir in [
            data.join("tmp/backups/b1-abc/snapshot"),
            data.join("tmp/restore/restore-abc"),
            data.join("tmp/uploads"),
            env.join("data"),
            env.join(".data-restore-done-aaaa"),
            env.join(".data-restore-prev-bbbb"),
            data.join("shared/.data-restore-abandoned-cccc"),
        ] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(data.join("restore-generation"), "g1").unwrap();

        sweep_retired_archive_state(data, RecoveryMode::Standby);
        assert!(data.join("tmp/backups").exists());

        sweep_retired_archive_state(data, RecoveryMode::Primary);
        assert!(!data.join("tmp/backups").exists());
        assert!(!data.join("tmp/restore").exists());
        assert!(!data.join("restore-generation").exists());
        assert!(!env.join(".data-restore-done-aaaa").exists());
        // Only an operator can tell which copy of an unfinished restore to keep.
        assert!(env.join(".data-restore-prev-bbbb").exists());
        assert!(data.join("shared/.data-restore-abandoned-cccc").exists());
        assert!(data.join("tmp/uploads").exists());
        assert!(env.join("data").exists());
    }
}
