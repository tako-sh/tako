use super::*;
use base64::Engine;

#[test]
fn maintenance_commands_use_fixed_approved_helpers() {
    assert_eq!(
        remote_binary_replace_command(
            "https://example.test/tako-server-linux-x86_64-glibc.tar.zst",
            "abc"
        ),
        SshClient::run_as_root(
            "/usr/local/bin/tako-server-install-refresh 'tako-server-linux-x86_64-glibc.tar.zst' 'abc'"
        )
    );
    assert_eq!(
        remote_restore_previous_binary_command(),
        SshClient::run_as_root("/usr/local/bin/tako-server-service rollback")
    );
    assert_eq!(
        remote_cleanup_previous_binary_command(),
        SshClient::run_as_root("/usr/local/bin/tako-server-service cleanup-upgrade")
    );
}

#[test]
fn upgrade_marker_commands_work_as_service_user() {
    let directory = tempfile::tempdir().unwrap();
    let data_dir = directory.path().to_str().unwrap();
    for command in [
        remote_prepare_upgrade_reload_command(data_dir, "controller-a"),
        remote_cleanup_upgrade_reload_command(data_dir),
    ] {
        let result = std::process::Command::new("sh")
            .args(["-c", &format!("sudo() {{ return 99; }}; {command}")])
            .status()
            .unwrap();
        assert!(result.success());
    }
}

// Keep this signed fixture byte-for-byte: its asset names are opaque signature input.
const TEST_SERVER_CHECKSUM_MANIFEST: &str = "1111111111111111111111111111111111111111111111111111111111111111  tako-server-linux-x86_64-glibc.tar.zst\n\
     2222222222222222222222222222222222222222222222222222222222222222  tako-server-linux-aarch64-musl.tar.zst\n";
const TEST_SERVER_CHECKSUM_MANIFEST_SIG_BASE64: &str = "nZdPJ9zO2xgD3KYpdDWovNaMNko8XtBjcqSJVdNZs0aIwKKfc4pG8g0paADEUHIjwabW80jfj35n5qmEH1ko111qsUUsNwdB0ewUAckN5fvO+tprTmhWsFV9653I7q36LzFT3E3ORNI5JUHLQKqgn15DoOloPR7pi1sU/r4y2FFXJcfBIir0LR5jrR9eXuyPAqDDJSX2QJX19WtEnWNXZsAZUaTsHUtXrlHdqtQDb9fA+pr3w+dVUjg12mYRBi1CJbnxTbrZUyy7+LMDQwXWagTjivHXCaSiZVGz4JGuEMds838wNsy8nfwCqXhffrMXuIb3sOZ6sfPVLZgeUnr12ZpkDjYEiDAz0HEekNQUIIQqjvlcIkgxZYByZLRap0Vvi4NMfPkRI7K7FDtY1hhs7CurJ7Xcag784cx5V+pFEPIbCfMnEjK/beP+V36UbSbjnbOtbw4WUKQZH+knspw+MUBmy3ZdqGsgYDSyVQ6dE5u7lvl4V9/ai8f5pue5uWgL";

#[test]
fn build_upgrade_owner_is_shell_safe() {
    let owner = build_upgrade_owner("prod-1");
    assert!(owner.contains("upgrade-prod-1-"));
    assert!(owner.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
}

#[test]
fn server_release_base_uses_official_latest_tag() {
    assert_eq!(
        default_download_base(),
        "https://github.com/tako-sh/tako/releases/download/latest"
    );
}

#[test]
fn parse_sha256_manifest_value_finds_named_asset() {
    let sha = parse_sha256_manifest_value(
        TEST_SERVER_CHECKSUM_MANIFEST,
        "tako-server-linux-aarch64-musl.tar.zst",
    )
    .unwrap();
    assert_eq!(
        sha,
        "2222222222222222222222222222222222222222222222222222222222222222"
    );
}

#[test]
fn verify_signed_server_checksum_manifest_accepts_valid_signature() {
    let signature = base64::engine::general_purpose::STANDARD
        .decode(TEST_SERVER_CHECKSUM_MANIFEST_SIG_BASE64)
        .unwrap();
    verify_signed_server_checksum_manifest(TEST_SERVER_CHECKSUM_MANIFEST.as_bytes(), &signature)
        .unwrap();
}

#[test]
fn verify_signed_server_checksum_manifest_rejects_tampering() {
    let signature = base64::engine::general_purpose::STANDARD
        .decode(TEST_SERVER_CHECKSUM_MANIFEST_SIG_BASE64)
        .unwrap();
    let err = verify_signed_server_checksum_manifest(
        b"1111111111111111111111111111111111111111111111111111111111111111  tako-server-linux-x86_64-glibc.tar.zst\n",
        &signature,
    )
    .unwrap_err();
    assert!(err.contains("signature verification failed"));
}

#[test]
fn protocol_compatibility_command_checks_cli_and_active_releases() {
    let command = remote_protocol_compatibility_command("/opt/tako data", false);

    assert!(command.contains("--check-protocol-compatibility"));
    assert!(command.contains("--data-dir"));
    assert!(command.contains("/opt/tako data"));
    assert!(command.contains("--expected-protocol-version 0"));
    assert!(!command.contains("--allow-incompatible-protocol"));
}

#[test]
fn forced_protocol_compatibility_command_passes_override() {
    let command = remote_protocol_compatibility_command("/opt/tako", true);

    assert!(command.contains("--allow-incompatible-protocol"));
}

#[test]
fn upgrade_reload_handoff_commands_write_and_remove_owner_marker() {
    let prepare = remote_prepare_upgrade_reload_command("/opt/tako data", "controller-a");
    let cleanup = remote_cleanup_upgrade_reload_command("/opt/tako data");

    assert!(prepare.contains("umask 077"));
    assert!(prepare.contains("chmod 0644"));
    assert!(prepare.contains(tako_core::UPGRADE_RELOAD_MARKER_FILE));
    assert!(prepare.contains("controller-a"));
    assert!(cleanup.contains("rm -f"));
    assert!(cleanup.contains(tako_core::UPGRADE_RELOAD_MARKER_FILE));
}

#[test]
fn build_upgrade_owner_differs_by_server_name() {
    let a = build_upgrade_owner("prod-1");
    let b = build_upgrade_owner("prod-2");
    assert_ne!(a, b, "different servers should produce different owner IDs");
    assert!(a.contains("prod-1"));
    assert!(b.contains("prod-2"));
}

#[test]
fn first_non_empty_line_skips_blanks() {
    assert_eq!(first_non_empty_line("\n\n  hello\nworld"), Some("hello"));
    assert_eq!(first_non_empty_line(""), None);
    assert_eq!(first_non_empty_line("\n\n"), None);
    assert_eq!(first_non_empty_line("first"), Some("first"));
}
