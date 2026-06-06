//! Config watcher for `tako dev`.
//!
//! Watches Tako-owned inputs for `tako dev`.

use notify::RecursiveMode;
use notify_debouncer_mini::new_debouncer;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchChange {
    Config,
    Secrets,
    Channels,
    Workflows,
    GeneratedDeclarations,
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
        for dir in crate::build::js::generated_declaration_parent_dirs(&self.project_dir) {
            if dir != self.project_dir && dir.is_dir() {
                let _ = watch_path(&debouncer, &dir, RecursiveMode::NonRecursive);
            }
        }

        let changed_tx = self.changed_tx.clone();
        let project_dir = self.project_dir.clone();
        let app_root = self.app_root.clone();
        let config_path = self.config_path.clone();
        let ignore_existing_runtime_defs_modified_before = SystemTime::now();
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
                            for dir in
                                crate::build::js::generated_declaration_parent_dirs(&project_dir)
                            {
                                if dir != project_dir && event.path == dir && dir.is_dir() {
                                    let _ = watch_path(
                                        &debouncer_for_thread,
                                        &dir,
                                        RecursiveMode::NonRecursive,
                                    );
                                }
                            }
                            if let Some(change) = classify_path(
                                &project_dir,
                                &app_root,
                                &config_path,
                                &event.path,
                                Some(ignore_existing_runtime_defs_modified_before),
                            ) {
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
    ignore_existing_runtime_defs_modified_before: Option<SystemTime>,
) -> Option<WatchChange> {
    if path == config_path {
        return Some(WatchChange::Config);
    }
    if path == project_dir.join(".tako").join("secrets.json") {
        return Some(WatchChange::Secrets);
    }
    if crate::build::js::is_generated_declaration_path(project_dir, path) {
        return Some(WatchChange::GeneratedDeclarations);
    }

    let channels_dir = app_root.join("channels");
    if path == channels_dir {
        return (!path.exists()).then_some(WatchChange::Channels);
    }
    if path.starts_with(&channels_dir) {
        if is_preexisting_runtime_definition_event(
            path,
            ignore_existing_runtime_defs_modified_before,
        ) {
            return None;
        }
        return Some(WatchChange::Channels);
    }

    let workflows_dir = app_root.join("workflows");
    if path == workflows_dir {
        return (!path.exists()).then_some(WatchChange::Workflows);
    }
    if path.starts_with(&workflows_dir) {
        if is_preexisting_runtime_definition_event(
            path,
            ignore_existing_runtime_defs_modified_before,
        ) {
            return None;
        }
        return Some(WatchChange::Workflows);
    }

    None
}

fn is_preexisting_runtime_definition_event(path: &Path, cutoff: Option<SystemTime>) -> bool {
    let Some(cutoff) = cutoff else {
        return false;
    };
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    modified <= cutoff
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relevant_paths_include_config_secrets_channels_workflows_and_generated_declarations() {
        let temp = tempfile::TempDir::new().unwrap();
        let project_dir = temp.path().join("demo");
        let app_root = project_dir.join("src");
        let config_path = project_dir.join("tako.toml");

        assert_eq!(
            classify_path(&project_dir, &app_root, &config_path, &config_path, None),
            Some(WatchChange::Config)
        );
        assert_eq!(
            classify_path(
                &project_dir,
                &app_root,
                &config_path,
                &project_dir.join(".tako").join("secrets.json"),
                None
            ),
            Some(WatchChange::Secrets)
        );
        assert_eq!(
            classify_path(
                &project_dir,
                &app_root,
                &config_path,
                &app_root.join("channels").join("demo.ts"),
                None
            ),
            Some(WatchChange::Channels)
        );
        assert_eq!(
            classify_path(
                &project_dir,
                &app_root,
                &config_path,
                &app_root.join("workflows").join("demo.ts"),
                None
            ),
            Some(WatchChange::Workflows)
        );
        assert_eq!(
            classify_path(
                &project_dir,
                &app_root,
                &config_path,
                &app_root.join("channels"),
                None
            ),
            Some(WatchChange::Channels)
        );
        assert_eq!(
            classify_path(
                &project_dir,
                &app_root,
                &config_path,
                &app_root.join("workflows"),
                None
            ),
            Some(WatchChange::Workflows)
        );
        assert_eq!(
            classify_path(
                &project_dir,
                &app_root,
                &config_path,
                &project_dir.join("tako.d.ts"),
                None
            ),
            Some(WatchChange::GeneratedDeclarations)
        );
        assert_eq!(
            classify_path(
                &project_dir,
                &app_root,
                &config_path,
                &project_dir.join("app").join("tako.d.ts"),
                None
            ),
            Some(WatchChange::GeneratedDeclarations)
        );
        assert_eq!(
            classify_path(
                &project_dir,
                &app_root,
                &config_path,
                &project_dir.join("src").join("tako.d.ts"),
                None
            ),
            Some(WatchChange::GeneratedDeclarations)
        );
        assert_eq!(
            classify_path(
                &project_dir,
                &app_root,
                &config_path,
                &project_dir.join("client").join("tako.d.ts"),
                None
            ),
            None
        );
        assert_eq!(
            classify_path(
                &project_dir,
                &app_root,
                &config_path,
                &app_root.join("tako.d.ts"),
                None
            ),
            Some(WatchChange::GeneratedDeclarations)
        );
        assert_eq!(
            classify_path(
                &project_dir,
                &app_root,
                &config_path,
                &project_dir.join("channels").join("demo.ts"),
                None
            ),
            None
        );
        assert_eq!(
            classify_path(
                &project_dir,
                &app_root,
                &config_path,
                &project_dir.join("src").join("index.ts"),
                None
            ),
            None
        );
    }

    #[test]
    fn existing_channel_and_workflow_directory_events_are_ignored() {
        let temp = tempfile::TempDir::new().unwrap();
        let project_dir = temp.path().join("demo");
        let app_root = project_dir.join("src");
        let config_path = project_dir.join("tako.toml");
        let channels_dir = app_root.join("channels");
        let workflows_dir = app_root.join("workflows");
        std::fs::create_dir_all(&channels_dir).unwrap();
        std::fs::create_dir_all(&workflows_dir).unwrap();

        assert_eq!(
            classify_path(&project_dir, &app_root, &config_path, &channels_dir, None),
            None
        );
        assert_eq!(
            classify_path(&project_dir, &app_root, &config_path, &workflows_dir, None),
            None
        );
        assert_eq!(
            classify_path(
                &project_dir,
                &app_root,
                &config_path,
                &channels_dir.join("demo.ts"),
                None
            ),
            Some(WatchChange::Channels)
        );
        assert_eq!(
            classify_path(
                &project_dir,
                &app_root,
                &config_path,
                &workflows_dir.join("broadcast.ts"),
                None
            ),
            Some(WatchChange::Workflows)
        );
    }

    #[test]
    fn preexisting_runtime_definition_file_events_are_ignored_at_watcher_start() {
        let temp = tempfile::TempDir::new().unwrap();
        let project_dir = temp.path().join("demo");
        let app_root = project_dir.join("src");
        let config_path = project_dir.join("tako.toml");
        let channels_dir = app_root.join("channels");
        let workflows_dir = app_root.join("workflows");
        std::fs::create_dir_all(&channels_dir).unwrap();
        std::fs::create_dir_all(&workflows_dir).unwrap();
        let channel_file = channels_dir.join("demo.ts");
        let workflow_file = workflows_dir.join("broadcast.ts");
        std::fs::write(&channel_file, "export default null;\n").unwrap();
        std::fs::write(&workflow_file, "export default null;\n").unwrap();
        let watcher_started_after_existing_files = SystemTime::now();

        assert_eq!(
            classify_path(
                &project_dir,
                &app_root,
                &config_path,
                &channel_file,
                Some(watcher_started_after_existing_files),
            ),
            None
        );
        assert_eq!(
            classify_path(
                &project_dir,
                &app_root,
                &config_path,
                &workflow_file,
                Some(watcher_started_after_existing_files),
            ),
            None
        );

        std::thread::sleep(Duration::from_millis(2));
        std::fs::write(&channel_file, "export default 1;\n").unwrap();
        std::fs::write(&workflow_file, "export default 1;\n").unwrap();

        assert_eq!(
            classify_path(
                &project_dir,
                &app_root,
                &config_path,
                &channel_file,
                Some(watcher_started_after_existing_files),
            ),
            Some(WatchChange::Channels)
        );
        assert_eq!(
            classify_path(
                &project_dir,
                &app_root,
                &config_path,
                &workflow_file,
                Some(watcher_started_after_existing_files),
            ),
            Some(WatchChange::Workflows)
        );
    }
}
