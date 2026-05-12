use super::*;
use tempfile::TempDir;

#[test]
fn gather_user_targets_includes_existing_dirs() {
    let tmp = TempDir::new().unwrap();

    let targets =
        gather_user_targets_from(tmp.path().to_path_buf(), tmp.path().to_path_buf(), vec![]);
    assert!(targets.iter().any(|p| p == tmp.path()));
    // TAKO_HOME override makes config_dir == data_dir, so only one entry
    let dir_targets: Vec<_> = targets.iter().filter(|p| p.is_dir()).collect();
    assert_eq!(dir_targets.len(), 1);
}

#[test]
fn gather_user_targets_empty_when_nothing_exists() {
    let missing = TempDir::new().unwrap().path().join("missing");
    let targets = gather_user_targets_from(missing.clone(), missing.clone(), vec![]);
    assert!(
        !targets.iter().any(|p| p.starts_with(&missing)),
        "missing TAKO_HOME targets should not be collected"
    );
}

#[test]
fn find_tako_binaries_returns_existing_siblings() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("tako"), b"bin").unwrap();
    std::fs::write(tmp.path().join("tako-dev-server"), b"bin").unwrap();

    let names = ["tako", "tako-dev-server", "tako-dev-proxy"];
    let found: Vec<PathBuf> = names
        .iter()
        .map(|name| tmp.path().join(name))
        .filter(|path| path.exists())
        .collect();

    assert_eq!(found.len(), 2);
    assert!(found[0].ends_with("tako"));
    assert!(found[1].ends_with("tako-dev-server"));
}

#[test]
fn server_implode_script_stops_services() {
    let script = build_server_implode_script();
    assert!(script.contains("systemctl stop tako-server"));
    assert!(script.contains("systemctl disable tako-server"));
    assert!(script.contains("rc-service tako-server stop"));
    assert!(script.contains("rc-update del tako-server"));
}

#[test]
fn server_implode_script_removes_binaries() {
    let script = build_server_implode_script();
    assert!(script.contains("rm -f /usr/local/bin/tako-server"));
    assert!(script.contains("rm -f /usr/local/bin/tako-server-service"));
    assert!(script.contains("rm -f /usr/local/bin/tako-server-install-refresh"));
}

#[test]
fn server_implode_script_removes_data_and_sockets() {
    let script = build_server_implode_script();
    assert!(script.contains("rm -rf /opt/tako"));
    assert!(script.contains("rm -rf /var/run/tako"));
}

#[test]
fn server_implode_script_removes_service_files() {
    let script = build_server_implode_script();
    assert!(script.contains("rm -f /etc/systemd/system/tako-server.service"));
    assert!(script.contains("rm -f /etc/systemd/system/tako-server-standby.service"));
    assert!(script.contains("rm -rf /etc/systemd/system/tako-server.service.d"));
    assert!(script.contains("rm -f /etc/init.d/tako-server"));
    assert!(script.contains("rm -f /etc/init.d/tako-server-standby"));
    assert!(script.contains("systemctl daemon-reload"));
}

#[cfg(target_os = "macos")]
#[test]
fn macos_system_targets_include_dev_proxy_when_present() {
    // This is a detection test — it verifies the function runs without panic.
    // Actual file presence depends on the machine state.
    let targets = gather_macos_system_targets();
    // Each target should have a non-empty description and at least one command
    for t in &targets {
        assert!(!t.description.is_empty());
        assert!(!t.commands.is_empty());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn linux_system_targets_include_service_when_present() {
    let targets = gather_linux_system_targets();
    for t in &targets {
        assert!(!t.description.is_empty());
        assert!(!t.commands.is_empty());
    }
}
