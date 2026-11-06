//! Config watcher for `tako dev`.
//!
//! Watches Tako-owned inputs for `tako dev`.

use notify::{RecursiveMode, Watcher};
use notify_debouncer_mini::{Config, Debouncer, new_debouncer_opt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchChange {
    Config,
    Secrets,
    Channels,
    Services,
    Workflows,
    GeneratedDeclarations,
}

/// Handle that keeps the watcher alive
pub struct WatcherHandle {
    _debouncer: Arc<Mutex<Debouncer<ChangesOnlyWatcher>>>,
    _thread: std::thread::JoinHandle<()>,
}

// The mini debouncer erases event kinds. Filter access events first so loading
// tako.toml (including during a reload) cannot schedule another reload on Linux.
struct ChangesOnlyWatcher(notify::RecommendedWatcher);

impl Watcher for ChangesOnlyWatcher {
    fn new<F: notify::EventHandler>(
        mut handler: F,
        config: notify::Config,
    ) -> notify::Result<Self> {
        notify::RecommendedWatcher::new(
            move |event: notify::Result<notify::Event>| {
                if event.as_ref().is_ok_and(|event| event.kind.is_access()) {
                    return;
                }
                handler.handle_event(event);
            },
            config,
        )
        .map(Self)
    }

    fn watch(&mut self, path: &Path, mode: RecursiveMode) -> notify::Result<()> {
        self.0.watch(path, mode)
    }

    fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
        self.0.unwatch(path)
    }

    fn configure(&mut self, option: notify::Config) -> notify::Result<bool> {
        self.0.configure(option)
    }

    fn kind() -> notify::WatcherKind {
        notify::RecommendedWatcher::kind()
    }
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
        let debouncer = Arc::new(Mutex::new(new_debouncer_opt::<_, ChangesOnlyWatcher>(
            Config::default().with_timeout(Duration::from_millis(150)),
            tx,
        )?));

        watch_path(&debouncer, &self.config_path, RecursiveMode::NonRecursive)?;

        // Watch .tako/ directory for secrets.json changes.
        let tako_dir = self.project_dir.join(".tako");
        if tako_dir.is_dir() {
            let _ = watch_path(&debouncer, &tako_dir, RecursiveMode::NonRecursive);
        }
        watch_path(&debouncer, &self.project_dir, RecursiveMode::NonRecursive)?;
        let watched_app_root = self.app_root.clone();
        let watched_runtime_dirs = [
            (watched_app_root.join("channels"), WatchChange::Channels),
            (watched_app_root.join("presence"), WatchChange::Channels),
            (watched_app_root.join("stores"), WatchChange::Channels),
            (watched_app_root.join("services"), WatchChange::Services),
            (watched_app_root.join("workflows"), WatchChange::Workflows),
        ];
        let mut known_runtime_dirs = std::collections::HashSet::new();
        if watched_app_root != self.project_dir && watched_app_root.is_dir() {
            let _ = watch_path(&debouncer, &watched_app_root, RecursiveMode::NonRecursive);
        }
        for (dir, _) in &watched_runtime_dirs {
            if dir.is_dir() {
                let _ = watch_path(&debouncer, dir, RecursiveMode::NonRecursive);
                known_runtime_dirs.insert(dir.clone());
            }
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
                            if event.path == watched_app_root && watched_app_root.is_dir() {
                                let _ = watch_path(
                                    &debouncer_for_thread,
                                    &watched_app_root,
                                    RecursiveMode::NonRecursive,
                                );
                            }
                            for (dir, change) in &watched_runtime_dirs {
                                if event.path != *dir && event.path != watched_app_root {
                                    continue;
                                }
                                if dir.is_dir() {
                                    let _ = watch_path(
                                        &debouncer_for_thread,
                                        dir,
                                        RecursiveMode::NonRecursive,
                                    );
                                    // Moving a populated directory into the app root can produce
                                    // only a directory event; attach its watch and restart once.
                                    if known_runtime_dirs.insert(dir.clone()) {
                                        let _ = changed_tx.blocking_send(*change);
                                    }
                                } else {
                                    known_runtime_dirs.remove(dir);
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
    debouncer: &Arc<Mutex<Debouncer<ChangesOnlyWatcher>>>,
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
    if crate::build::js::is_generated_declaration_path(project_dir, path)
        || path == app_root.join("tako.services.ts")
    {
        return Some(WatchChange::GeneratedDeclarations);
    }

    for (dirname, change) in [
        ("channels", WatchChange::Channels),
        ("presence", WatchChange::Channels),
        ("stores", WatchChange::Channels),
        ("services", WatchChange::Services),
        ("workflows", WatchChange::Workflows),
    ] {
        let dir = app_root.join(dirname);
        if path == dir {
            return (!path.exists()).then_some(change);
        }
        if path.starts_with(&dir) {
            if is_preexisting_runtime_definition_event(
                path,
                ignore_existing_runtime_defs_modified_before,
            ) {
                return None;
            }
            return Some(change);
        }
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

    #[tokio::test]
    async fn reading_config_does_not_reload_but_editing_it_does() {
        let temp = tempfile::TempDir::new().unwrap();
        let project_dir = temp.path().canonicalize().unwrap();
        let config_path = project_dir.join("tako.toml");
        std::fs::write(&config_path, "name = \"before\"\n").unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        let _watcher =
            ConfigWatcher::new(project_dir.clone(), project_dir, config_path.clone(), tx)
                .unwrap()
                .start()
                .unwrap();

        // Loading the config during a reload must not schedule another reload.
        std::fs::read_to_string(&config_path).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(600), rx.recv())
                .await
                .is_err()
        );

        std::fs::write(&config_path, "name = \"after\"\n").unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap(),
            Some(WatchChange::Config),
        );
    }

    #[test]
    fn service_inputs_regenerate_but_generated_modules_do_not_restart() {
        let temp = tempfile::TempDir::new().unwrap();
        let project_dir = temp.path();
        let app_root = project_dir.join("client/app");
        let config_path = project_dir.join("tako.toml");
        let services = app_root.join("services");
        std::fs::create_dir_all(&services).unwrap();
        let declaration = services.join("catalog.mts");
        std::fs::write(&declaration, "export default {};\n").unwrap();
        let cutoff = SystemTime::now();
        assert_eq!(
            classify_path(
                project_dir,
                &app_root,
                &config_path,
                &declaration,
                Some(cutoff)
            ),
            None
        );
        assert_eq!(
            classify_path(project_dir, &app_root, &config_path, &services, None),
            None
        );
        std::thread::sleep(Duration::from_millis(2));
        std::fs::write(&declaration, "export default { name: 'catalog' };\n").unwrap();
        assert_eq!(
            classify_path(
                project_dir,
                &app_root,
                &config_path,
                &declaration,
                Some(cutoff)
            ),
            Some(WatchChange::Services)
        );
        std::fs::remove_file(&declaration).unwrap();
        assert_eq!(
            classify_path(
                project_dir,
                &app_root,
                &config_path,
                &declaration,
                Some(cutoff)
            ),
            Some(WatchChange::Services)
        );
        std::fs::remove_dir(&services).unwrap();
        assert_eq!(
            classify_path(
                project_dir,
                &app_root,
                &config_path,
                &services,
                Some(cutoff)
            ),
            Some(WatchChange::Services)
        );
        assert_eq!(
            classify_path(
                project_dir,
                &app_root,
                &config_path,
                &app_root.join("tako.services.ts"),
                None
            ),
            Some(WatchChange::GeneratedDeclarations)
        );
        assert_eq!(
            classify_path(
                project_dir,
                &app_root,
                &config_path,
                &project_dir.join("services/catalog.ts"),
                None
            ),
            None
        );
    }

    #[tokio::test]
    async fn watcher_tracks_services_moved_in_after_start_and_their_removal() {
        let temp = tempfile::TempDir::new().unwrap();
        let project_dir = temp.path().to_path_buf();
        let app_root = project_dir.join("src");
        let config_path = project_dir.join("tako.toml");
        std::fs::create_dir(&app_root).unwrap();
        std::fs::write(&config_path, "runtime = 'bun'\n").unwrap();
        let staged = temp.path().join("staged");
        std::fs::create_dir(&staged).unwrap();
        std::fs::write(staged.join("catalog.ts"), "export default {};\n").unwrap();
        let (tx, mut rx) = mpsc::channel(32);
        let _watcher = ConfigWatcher::new(project_dir, app_root.clone(), config_path, tx)
            .unwrap()
            .start()
            .unwrap();
        let services = app_root.join("services");
        std::fs::rename(staged, &services).unwrap();
        expect_change(&mut rx, WatchChange::Services).await;
        drain_changes(&mut rx).await;
        std::fs::write(
            services.join("catalog.ts"),
            "export default { name: 'catalog' };\n",
        )
        .unwrap();
        expect_change(&mut rx, WatchChange::Services).await;
        drain_changes(&mut rx).await;
        std::fs::read_to_string(services.join("catalog.ts")).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(400), rx.recv())
                .await
                .is_err()
        );
        std::fs::remove_dir_all(&services).unwrap();
        expect_change(&mut rx, WatchChange::Services).await;
        drain_changes(&mut rx).await;
        std::fs::write(
            app_root.join("tako.services.ts"),
            "export const services = {};\n",
        )
        .unwrap();
        expect_change(&mut rx, WatchChange::GeneratedDeclarations).await;
        while let Ok(Some(change)) =
            tokio::time::timeout(Duration::from_millis(400), rx.recv()).await
        {
            assert_eq!(change, WatchChange::GeneratedDeclarations);
        }
        std::fs::read_to_string(app_root.join("tako.services.ts")).unwrap();
        std::fs::read_to_string(app_root.parent().unwrap().join("tako.toml")).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(400), rx.recv())
                .await
                .is_err()
        );
    }

    async fn expect_change(rx: &mut mpsc::Receiver<WatchChange>, expected: WatchChange) {
        let received = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("watch event timed out")
            .expect("watcher stopped");
        assert_eq!(received, expected);
    }

    async fn drain_changes(rx: &mut mpsc::Receiver<WatchChange>) {
        while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(400), rx.recv()).await {}
    }

    #[test]
    fn relevant_paths_include_config_secrets_channels_workflows_and_generated_declarations() {
        let temp = tempfile::TempDir::new().unwrap();
        let project_dir = temp.path().join("demo");
        let app_root = project_dir.join("src");
        let config_path = project_dir.join("tako.toml");

        assert_eq!(
            classify_path(
                &project_dir,
                &app_root,
                &config_path,
                &app_root.join("presence/lobby.ts"),
                None
            ),
            Some(WatchChange::Channels)
        );

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
    fn store_definition_changes_and_directory_removal_restart_runtime() {
        let temp = tempfile::TempDir::new().unwrap();
        let project_dir = temp.path();
        let app_root = project_dir.join("app");
        let config_path = project_dir.join("tako.toml");
        let stores_dir = app_root.join("stores");
        std::fs::create_dir_all(&stores_dir).unwrap();
        let store = stores_dir.join("messages.ts");
        std::fs::write(&store, "export default null;\n").unwrap();
        let cutoff = SystemTime::now();
        assert_eq!(
            classify_path(project_dir, &app_root, &config_path, &store, Some(cutoff)),
            None
        );
        std::thread::sleep(Duration::from_millis(2));
        std::fs::write(&store, "export default 1;\n").unwrap();
        assert_eq!(
            classify_path(project_dir, &app_root, &config_path, &store, Some(cutoff)),
            Some(WatchChange::Channels)
        );
        std::fs::remove_dir_all(&stores_dir).unwrap();
        assert_eq!(
            classify_path(
                project_dir,
                &app_root,
                &config_path,
                &stores_dir,
                Some(cutoff)
            ),
            Some(WatchChange::Channels)
        );
        assert_eq!(
            classify_path(
                project_dir,
                &app_root,
                &config_path,
                &project_dir.join("stores/messages.ts"),
                None,
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
