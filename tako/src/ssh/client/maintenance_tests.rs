use super::tako::INSTALL_SERVER_SCRIPT;

fn helper_body(name: &str) -> &str {
    INSTALL_SERVER_SCRIPT
        .split_once(&format!("cat > \"$helper_dir/{name}\" <<'EOF'\n"))
        .unwrap()
        .1
        .split_once("\nEOF")
        .unwrap()
        .0
}

#[test]
fn maintenance_helpers_reject_extra_arguments_before_touching_host() {
    for (name, arguments) in [
        ("refresh", vec!["unexpected"]),
        ("service", vec!["restart", "unexpected"]),
        ("service", vec!["rollback", "unexpected"]),
        ("service", vec!["implode", "unexpected"]),
        ("service", vec!["configure", "80", "443", "unexpected"]),
        (
            "service",
            vec!["configure", "80;touch /tmp/unwanted", "443", "tailscale"],
        ),
        ("service", vec!["configure", "0", "443", "netbird"]),
        ("service", vec!["configure", "80", "80", "netbird"]),
    ] {
        let output = std::process::Command::new("sh")
            .args(["-c", helper_body(name), name])
            .args(arguments)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).starts_with("error: "));
    }
}

#[cfg(unix)]
fn executable(path: &std::path::Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, contents).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
#[test]
fn configure_helper_updates_ports_enables_and_restarts_existing_service() {
    check_configure_helper(
        " --management-host 100.64.0.2",
        "100.64.0.2",
        "tailscale",
        true,
    );
}

#[cfg(unix)]
#[test]
fn configure_helper_recovers_management_binding_after_tailscale_connects() {
    check_configure_helper("", "100.64.0.2", "tailscale", true);
}

#[cfg(unix)]
#[test]
fn configure_helper_rejects_non_tailscale_management_address() {
    check_configure_helper("", "8.8.8.8", "tailscale", false);
}

#[test]
fn service_maintenance_enables_boot_startup_before_reload_or_restart() {
    for action in ["reload", "restart"] {
        let directory = tempfile::tempdir().unwrap();
        let log = directory.path().join("calls");
        let command = format!(
            "systemctl() {{ printf '%s\\n' \"$*\" >> '{}'; }};\n{}",
            log.display(),
            helper_body("service")
        );
        let output = std::process::Command::new("sh")
            .args(["-c", &command, "service", action])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            std::fs::read_to_string(log).unwrap(),
            format!("enable tako-server\n{action} tako-server\n")
        );
    }
}

#[test]
fn service_maintenance_fails_without_restarting_when_boot_startup_cannot_be_enabled() {
    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("calls");
    let command = format!(
        "systemctl() {{ printf '%s\\n' \"$*\" >> '{}'; [ \"$1\" != enable ]; }};\n{}",
        log.display(),
        helper_body("service")
    );
    let output = std::process::Command::new("sh")
        .args(["-c", &command, "service", "restart"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        std::fs::read_to_string(log).unwrap(),
        "enable tako-server\n"
    );
}

#[cfg(unix)]
#[test]
fn configure_helper_recovers_netbird_and_refreshes_stale_address() {
    check_configure_helper("", "100.80.0.2", "netbird", true);
    check_configure_helper(
        " --management-host 100.80.0.1",
        "100.80.0.2",
        "netbird",
        true,
    );
    check_configure_helper("", "fd00::2", "netbird", true);
}

#[cfg(unix)]
fn check_configure_helper(
    management_argument: &str,
    detected_address: &str,
    network: &str,
    succeeds: bool,
) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let service = root.join("service");
    std::fs::write(
        &service,
        format!("ExecStart=tako-server --http-port 80 --https-port 443{management_argument}\n"),
    )
    .unwrap();
    let log = root.join("calls");
    executable(
        &root.join(network),
        &format!(
            "#!/bin/sh\nif [ \"$1\" = status ] && [ \"$2\" = --json ]; then echo '{{\"BackendState\":\"Running\"}}'; elif [ \"$#\" = 1 ]; then echo 'Management: Connected'; else echo {detected_address}; fi\n"
        ),
    );
    executable(
        &root.join("systemctl"),
        &format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n", log.display()),
    );
    #[cfg(target_os = "macos")]
    executable(
        &root.join("sed"),
        "#!/bin/sh\nshift\nexec /usr/bin/sed -i '' \"$@\"\n",
    );
    let other_network = if network == "netbird" {
        "tailscale"
    } else {
        "netbird"
    };
    executable(&root.join(other_network), "#!/bin/sh\necho 100.99.0.1\n");
    let network_address = root.join("network-address");
    let functions = INSTALL_SERVER_SCRIPT
        .split_once("cat <<'NETWORK_EOF'\n")
        .unwrap()
        .1
        .split_once("\nNETWORK_EOF")
        .unwrap()
        .0;
    executable(
        &network_address,
        &format!("#!/bin/sh\nset -eu\n{functions}\ndetect_management_ip \"$@\"\n"),
    );
    let network_provider = root.join("network-provider");
    std::fs::write(&network_provider, format!("{network}\n")).unwrap();
    let command = helper_body("service")
        .replace(
            "/usr/local/libexec/tako/network-address",
            network_address.to_str().unwrap(),
        )
        .replace(
            "/usr/local/libexec/tako/network-provider",
            network_provider.to_str().unwrap(),
        )
        .replace(
            "/etc/systemd/system/tako-server.service",
            service.to_str().unwrap(),
        )
        .replace(
            "/etc/systemd/system/tako-server-standby.service",
            root.join("absent-standby").to_str().unwrap(),
        );
    let output = std::process::Command::new("sh")
        .args([
            "-c",
            &command,
            "service",
            "configure",
            "8080",
            "8443",
            network,
        ])
        .env("PATH", format!("{}:/usr/bin:/bin", root.display()))
        .output()
        .unwrap();
    assert_eq!(
        output.status.success(),
        succeeds,
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    if !succeeds {
        assert!(!log.exists());
        assert_eq!(
            std::fs::read_to_string(service).unwrap(),
            "ExecStart=tako-server --http-port 80 --https-port 443\n"
        );
        return;
    }
    assert_eq!(
        std::fs::read_to_string(service).unwrap(),
        format!(
            "ExecStart=tako-server --http-port 8080 --https-port 8443 --management-host {detected_address}\n"
        )
    );
    assert_eq!(
        std::fs::read_to_string(log).unwrap(),
        "daemon-reload\nenable tako-server\nrestart tako-server\n"
    );
}

#[cfg(unix)]
#[test]
fn refresh_helper_binds_download_to_verified_hash_before_installing() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let settings = root.join("settings");
    std::fs::write(
        &settings,
        "custom-user\n/srv/custom-tako\n/run/custom-tako/server.sock\n",
    )
    .unwrap();
    let network_provider = root.join("network-provider");
    std::fs::write(&network_provider, "netbird\n").unwrap();
    let binary = root.join("tako-server");
    std::fs::write(&binary, "old binary").unwrap();
    let archive = root.join("payload");
    std::fs::write(&archive, "verified archive").unwrap();
    let installer = root.join("installer");
    let installed = root.join("installed");
    std::fs::write(&installer, format!("#!/bin/sh\nset -eu\nprintf '%s\\n' \"$TAKO_USER\" \"$TAKO_HOME\" \"$TAKO_SOCKET\" \"$TAKO_RESTART_SERVICE\" \"$TAKO_NETWORK\" > '{}'\ncp \"${{TAKO_SERVER_URL#file://}}\" '{}'\n", installed.display(), binary.display())).unwrap();
    let urls = root.join("urls");
    executable(
        &root.join("curl"),
        &format!(
            "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$2\" >> '{}'\ncase \"$2\" in\n https://tako.sh/install-server.sh) cp '{}' \"$4\" ;;\n https://github.com/tako-sh/tako/releases/download/latest/tako-server-linux-x86_64-glibc.tar.zst) cp '{}' \"$4\" ;;\n *) exit 91 ;;\nesac\n",
            urls.display(),
            installer.display(),
            archive.display()
        ),
    );
    let command = helper_body("refresh")
        .replace(
            "/usr/local/libexec/tako/network-provider",
            network_provider.to_str().unwrap(),
        )
        .replace(
            "/usr/local/libexec/tako/install-settings",
            settings.to_str().unwrap(),
        )
        .replace("/usr/local/bin/tako-server", binary.to_str().unwrap());
    let digest = openssl::sha::sha256(b"verified archive");
    let sha = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    for (hash, succeeds) in [("0".repeat(64), false), (sha, true)] {
        let output = std::process::Command::new("sh")
            .args([
                "-c",
                &command,
                "refresh",
                "tako-server-linux-x86_64-glibc.tar.zst",
                &hash,
            ])
            .env("PATH", format!("{}:/usr/bin:/bin", root.display()))
            .env("TAKO_HOME", "/untrusted")
            .env("TAKO_SERVER_URL", "https://untrusted.invalid/archive")
            .output()
            .unwrap();
        assert_eq!(
            output.status.success(),
            succeeds,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(installed.exists(), succeeds);
    }
    assert_eq!(std::fs::read_to_string(binary).unwrap(), "verified archive");
    assert_eq!(
        std::fs::read_to_string(root.join("tako-server.prev")).unwrap(),
        "old binary"
    );
    assert_eq!(
        std::fs::read_to_string(installed).unwrap(),
        "custom-user\n/srv/custom-tako\n/run/custom-tako/server.sock\n0\nnetbird\n"
    );
}
