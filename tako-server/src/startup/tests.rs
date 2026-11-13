use super::*;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn mode_of(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn prepare_data_dir_creates_dir_with_group_traverse_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("tako-data");

    prepare_data_dir(&dir).expect("prepare_data_dir");

    assert!(dir.is_dir());
    assert_eq!(
        mode_of(&dir),
        0o710,
        "data dir must grant group traverse so tako-app can descend into \
         runtimes/ and releases/ to exec app binaries; 0o700 triggers \
         ENOENT on execve because the kernel denies directory traversal"
    );
}

#[test]
fn prepare_data_dir_sets_mode_on_existing_dir() {
    // Existing directories need the same traversal permission as new ones.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("tako-data");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();

    prepare_data_dir(&dir).expect("prepare_data_dir");

    assert_eq!(mode_of(&dir), 0o710);
}

#[test]
fn prepare_data_dir_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("tako-data");

    prepare_data_dir(&dir).expect("prepare_data_dir first call");
    prepare_data_dir(&dir).expect("prepare_data_dir second call");

    assert_eq!(mode_of(&dir), 0o710);
}

#[test]
fn prepare_data_dir_keeps_certificate_and_account_directories_private() {
    let tmp = tempfile::tempdir().unwrap();
    prepare_data_dir(tmp.path()).unwrap();
    assert_eq!(mode_of(&tmp.path().join("certs")), 0o700);
    assert_eq!(mode_of(&tmp.path().join("acme")), 0o700);
}

#[test]
fn upgrade_reload_marker_is_consumed() {
    let tmp = tempfile::tempdir().unwrap();
    let marker = tmp.path().join(tako_core::UPGRADE_RELOAD_MARKER_FILE);
    std::fs::write(&marker, "controller-a\n").unwrap();

    let owner = take_upgrade_reload_owner(tmp.path()).unwrap();

    assert_eq!(owner.as_deref(), Some("controller-a"));
    assert!(!marker.exists());
}

fn start_state(data_dir: &Path, standby: bool) -> Result<ServerState, String> {
    ServerState::new_with_runtime(
        data_dir.to_path_buf(),
        Arc::new(crate::tls::CertManager::new(
            crate::tls::CertManagerConfig {
                cert_dir: data_dir.join("certs"),
                ..Default::default()
            },
        )),
        None,
        Arc::new(parking_lot::RwLock::new(HashMap::new())),
        crate::ServerRuntimeConfig {
            standby,
            ..crate::ServerRuntimeConfig::for_defaults(data_dir.to_path_buf())
        },
    )
    .map_err(|error| error.to_string())
}

#[test]
fn restarting_a_standby_preserves_the_primary_upgrade_lock() {
    let temp = tempfile::tempdir().unwrap();
    let primary = start_state(temp.path(), false).unwrap();
    assert!(
        primary
            .state_store
            .try_acquire_upgrade_lock("upgrade-test")
            .unwrap()
    );
    primary
        .state_store
        .set_server_mode(tako_core::UpgradeMode::Upgrading)
        .unwrap();
    let standby = start_state(temp.path(), true).unwrap();
    assert_eq!(
        primary.state_store.server_mode().unwrap(),
        tako_core::UpgradeMode::Upgrading
    );
    assert_eq!(
        primary.state_store.upgrade_lock_owner().unwrap().as_deref(),
        Some("upgrade-test")
    );
    drop(standby);
}

/// A standby never runs apps while a restore runs: its start waits for the
/// restore's exclusive standby lock, fails once the wait runs out, and holds
/// a shared one for as long as it runs, which restores refuse to start under.
#[test]
fn a_standby_start_waits_for_a_running_restore_and_then_locks_restores_out() {
    use crate::state_store::DrainLease;
    let tmp = tempfile::tempdir().unwrap();
    let restoring = |exclusive| DrainLease::try_standby_in(tmp.path(), exclusive).unwrap();

    let restore = restoring(true).unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        drop(restore);
    });
    let standby = start_state(tmp.path(), true);
    release.join().unwrap();
    assert!(
        standby.is_ok(),
        "the standby must start once the restore ends"
    );
    assert!(
        restoring(true).is_none(),
        "a running standby must lock restores out"
    );
    drop(standby);
    assert!(restoring(true).is_some(), "a stopped standby holds nothing");

    let primary = start_state(tmp.path(), false);
    assert!(
        primary.is_ok() && restoring(true).is_some(),
        "only a standby holds the standby lock"
    );
    drop(primary);

    let _restore = restoring(true).unwrap();
    assert!(
        start_state(tmp.path(), true).is_err(),
        "a standby must not start while a restore runs"
    );
}

/// A normal start holds the drain activity lease exclusively while it
/// recovers backup restores, which can take longer than a service manager
/// allows for quick restarts: another start waits for it instead of failing
/// at once, and fails only once the wait runs out.
#[test]
fn a_start_waits_while_another_start_recovers_the_data_directory() {
    use crate::state_store::DrainLease;
    let tmp = tempfile::tempdir().unwrap();
    let recovering = DrainLease::try_exclusive_in(tmp.path()).unwrap().unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        drop(recovering);
    });
    let standby = start_state(tmp.path(), true);
    release.join().unwrap();
    assert!(
        standby.is_ok(),
        "a start must wait for another start's recovery"
    );
    drop(standby);

    let _recovering = DrainLease::try_exclusive_in(tmp.path()).unwrap().unwrap();
    assert!(
        start_state(tmp.path(), false).is_err_and(|error| error.contains("retry startup")),
        "a start must fail once the wait runs out"
    );
}

/// A normal start never rolls back a restore while another server process
/// (a hot standby serving the same apps) holds the drain activity lease,
/// and none can start serving app data while a normal start recovers.
#[test]
fn a_normal_start_only_reports_restores_while_another_server_serves_the_data() {
    use crate::operations::RecoveryMode;
    let tmp = tempfile::tempdir().unwrap();
    let store =
        crate::state_store::SqliteStateStore::new(tmp.path().join("state.sqlite"), [7u8; 32]);
    store.init().unwrap();

    let standby = store.try_drain_activity(false).unwrap();
    assert!(standby.is_some(), "a serving process holds a shared lease");
    let (mode, lease) = startup_recovery_mode(tmp.path(), RecoveryMode::Primary);
    assert_eq!(mode, RecoveryMode::Successor);
    assert!(lease.is_none());
    drop(standby);

    let (mode, lease) = startup_recovery_mode(tmp.path(), RecoveryMode::Primary);
    assert_eq!(mode, RecoveryMode::Primary);
    assert!(lease.is_some());
    assert!(
        store.try_drain_activity(false).unwrap().is_none(),
        "no server process starts serving while a normal start recovers"
    );
    drop(lease);
    assert!(store.try_drain_activity(false).unwrap().is_some());

    for other in [RecoveryMode::Successor, RecoveryMode::Standby] {
        let (mode, lease) = startup_recovery_mode(tmp.path(), other);
        assert_eq!(mode, other);
        assert!(lease.is_none());
    }
}
