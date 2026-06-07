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
fn prepare_data_dir_upgrades_legacy_0o700_dir_to_0o710() {
    // Regression: older installers left /opt/tako at mode 0o700, which
    // blocks tako-app (a group-tako member) from traversing in. On the
    // next server boot, prepare_data_dir must fix the mode in place.
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
