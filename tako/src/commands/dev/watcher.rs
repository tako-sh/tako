//! Config watcher for `tako dev`.
//!
//! Watches Tako-owned inputs for `tako dev`.

use notify::RecursiveMode;
use notify_debouncer_mini::new_debouncer;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchChange {
    Config,
    Secrets,
    Channels,
    Workflows,
    GeneratedFile,
}

/// Handle that keeps the watcher alive
pub struct WatcherHandle {
    _debouncer: Arc<Mutex<notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>>>,
    _thread: std::thread::JoinHandle<()>,
}

/// Watches files that affect Tako runtime metadata in a project directory.
pub struct ConfigWatcher {
    project_dir: PathBuf,
    app_root: PathBuf,
    config_path: PathBuf,
    changed_tx: mpsc::Sender<WatchChange>,
}

impl ConfigWatcher {
    pub fn new(
        project_dir: PathBuf,
        app_root: PathBuf,
        config_path: PathBuf,
        changed_tx: mpsc::Sender<WatchChange>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            project_dir,
            app_root,
            config_path,
            changed_tx,
        })
    }

    pub fn start(self) -> Result<WatcherHandle, Box<dyn std::error::Error>> {
        let (tx, rx) = std_mpsc::channel();
        let debouncer = Arc::new(Mutex::new(new_debouncer(Duration::from_millis(150), tx)?));

        watch_path(&debouncer, &self.config_path, RecursiveMode::NonRecursive)?;

        // Watch .tako/ directory for secrets.json changes.
        let tako_dir = self.project_dir.join(".tako");
        if tako_dir.is_dir() {
            let _ = watch_path(&debouncer, &tako_dir, RecursiveMode::NonRecursive);
        }
        watch_path(&debouncer, &self.project_dir, RecursiveMode::NonRecursive)?;
        let watched_app_root = self.app_root.clone();
        let watched_channels = watched_app_root.join("channels");
        let watched_workflows = watched_app_root.join("workflows");
        if watched_app_root != self.project_dir && watched_app_root.is_dir() {
            let _ = watch_path(&debouncer, &watched_app_root, RecursiveMode::NonRecursive);
        }
        if watched_channels.is_dir() {
            let _ = watch_path(&debouncer, &watched_channels, RecursiveMode::NonRecursive);
        }
        if watched_workflows.is_dir() {
            let _ = watch_path(&debouncer, &watched_workflows, RecursiveMode::NonRecursive);
        }

        let changed_tx = self.changed_tx.clone();
        let project_dir = self.project_dir.clone();
        let app_root = self.app_root.clone();
        let config_path = self.config_path.clone();
        let debouncer_for_thread = debouncer.clone();
        let handle = std::thread::spawn(move || {
            for result in rx {
                match result {
                    Ok(events) => {
                        for event in events {
                            if event.path == watched_channels && watched_channels.is_dir() {
                                let _ = watch_path(
                                    &debouncer_for_thread,
                                    &watched_channels,
                                    RecursiveMode::NonRecursive,
                                );
                            }
                            if event.path == watched_workflows && watched_workflows.is_dir() {
                                let _ = watch_path(
                                    &debouncer_for_thread,
                                    &watched_workflows,
                                    RecursiveMode::NonRecursive,
                                );
                            }
                            if event.path == watched_app_root && watched_app_root.is_dir() {
                                let _ = watch_path(
                                    &debouncer_for_thread,
                                    &watched_app_root,
                                    RecursiveMode::NonRecursive,
                                );
                                if watched_channels.is_dir() {
                                    let _ = watch_path(
                                        &debouncer_for_thread,
                                        &watched_channels,
                                        RecursiveMode::NonRecursive,
                                    );
                                }
                                if watched_workflows.is_dir() {
                                    let _ = watch_path(
                                        &debouncer_for_thread,
                                        &watched_workflows,
                                        RecursiveMode::NonRecursive,
                                    );
                                }
                            }
                            if let Some(change) =
                                classify_path(&project_dir, &app_root, &config_path, &event.path)
                            {
                                let _ = changed_tx.blocking_send(change);
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("Watch error: {:?}", e);
                    }
                }
            }
        });

        Ok(WatcherHandle {
            _debouncer: debouncer,
            _thread: handle,
        })
    }
}

fn watch_path(
    debouncer: &Arc<Mutex<notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>>>,
    path: &Path,
    mode: RecursiveMode,
) -> notify::Result<()> {
    let mut guard = debouncer.lock().expect("watcher mutex poisoned");
    guard.watcher().watch(path, mode)
}

fn classify_path(
    project_dir: &Path,
    app_root: &Path,
    config_path: &Path,
    path: &Path,
) -> Option<WatchChange> {
    if path == config_path {
        return Some(WatchChange::Config);
    }
    if path == project_dir.join(".tako").join("secrets.json") {
        return Some(WatchChange::Secrets);
    }
    if path == app_root.join("tako.gen.ts") {
        return Some(WatchChange::GeneratedFile);
    }

    let channels_dir = app_root.join("channels");
    if path == channels_dir || path.starts_with(&channels_dir) {
        return Some(WatchChange::Channels);
    }

    let workflows_dir = app_root.join("workflows");
    if path == workflows_dir || path.starts_with(&workflows_dir) {
        return Some(WatchChange::Workflows);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relevant_paths_include_config_secrets_channels_workflows_and_generated_file() {
        let project_dir = PathBuf::from("/tmp/demo");
        let app_root = PathBuf::from("/tmp/demo/src");
        let config_path = project_dir.join("tako.toml");

        assert_eq!(
            classify_path(&project_dir, &app_root, &config_path, &config_path),
            Some(WatchChange::Config)
        );
        assert_eq!(
            classify_path(
                &project_dir,
                &app_root,
                &config_path,
                &project_dir.join(".tako").join("secrets.json")
            ),
            Some(WatchChange::Secrets)
        );
        assert_eq!(
            classify_path(
                &project_dir,
                &app_root,
                &config_path,
                &app_root.join("channels")
            ),
            Some(WatchChange::Channels)
        );
        assert_eq!(
            classify_path(
                &project_dir,
                &app_root,
                &config_path,
                &app_root.join("channels").join("demo.ts")
            ),
            Some(WatchChange::Channels)
        );
        assert_eq!(
            classify_path(
                &project_dir,
                &app_root,
                &config_path,
                &app_root.join("workflows").join("demo.ts")
            ),
            Some(WatchChange::Workflows)
        );
        assert_eq!(
            classify_path(
                &project_dir,
                &app_root,
                &config_path,
                &app_root.join("tako.gen.ts")
            ),
            Some(WatchChange::GeneratedFile)
        );
        assert_eq!(
            classify_path(
                &project_dir,
                &app_root,
                &config_path,
                &project_dir.join("channels").join("demo.ts")
            ),
            None
        );
        assert_eq!(
            classify_path(
                &project_dir,
                &app_root,
                &config_path,
                &project_dir.join("src").join("index.ts")
            ),
            None
        );
    }
}
