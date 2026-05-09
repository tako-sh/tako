use super::*;
use base64::Engine;

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
fn server_binary_download_url_uses_latest_tag() {
    let target = crate::config::ServerTarget {
        arch: "x86_64".to_string(),
        libc: "glibc".to_string(),
    };
    let url = server_binary_download_url(&target, None, false).unwrap();
    assert_eq!(
        url,
        "https://github.com/lilienblum/tako/releases/download/latest/tako-server-linux-x86_64-glibc.tar.zst"
    );
}

#[test]
fn server_binary_download_url_rejects_insecure_custom_base_without_override() {
    let target = crate::config::ServerTarget {
        arch: "x86_64".to_string(),
        libc: "glibc".to_string(),
    };
    let err = server_binary_download_url(&target, Some("http://example.test/releases"), false)
        .unwrap_err();
    assert!(err.contains("must use https://"));
}

#[test]
fn server_binary_download_url_allows_insecure_custom_base_with_explicit_override() {
    let target = crate::config::ServerTarget {
        arch: "x86_64".to_string(),
        libc: "glibc".to_string(),
    };
    let url =
        server_binary_download_url(&target, Some("http://example.test/releases"), true).unwrap();
    assert_eq!(
        url,
        "http://example.test/releases/tako-server-linux-x86_64-glibc.tar.zst"
    );
}

#[test]
fn remote_binary_replace_preserves_app_user_switch_capabilities() {
    let command = remote_binary_replace_command("https://example.com/tako.tar.zst", "a");
    assert!(command.contains("cap_net_bind_service,cap_setuid,cap_setgid,cap_kill=+ep"));
}

#[test]
fn remote_restore_previous_binary_preserves_app_user_switch_capabilities() {
    let command = remote_restore_previous_binary_command();
    assert!(command.contains("cap_net_bind_service,cap_setuid,cap_setgid,cap_kill=+ep"));
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
fn remote_binary_replace_command_uses_root_shell_wrapper_and_verifies_sha256() {
    let cmd = remote_binary_replace_command(
        "https://example.com/tako-server.tar.zst",
        "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
    );
    assert!(cmd.contains("then sh -c '"));
    assert!(cmd.contains("sudo sh -c '"));
    assert!(cmd.contains("curl -fsSL"));
    assert!(cmd.contains("GH_TOKEN"));
    assert!(cmd.contains("GITHUB_TOKEN"));
    assert!(cmd.contains("Authorization: Bearer"));
    assert!(cmd.contains("sha256 mismatch"));
    assert!(cmd.contains("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"));
    assert!(cmd.contains("install -m 0755"));
    assert!(cmd.contains("/usr/local/bin/tako-server.prev"));
    assert!(cmd.contains("/usr/local/bin/tako-server"));
}

#[test]
fn remote_restore_previous_binary_command_restores_prev_binary() {
    let cmd = remote_restore_previous_binary_command();
    assert!(cmd.contains("sudo sh -c '"));
    assert!(cmd.contains("previous tako-server binary not found"));
    assert!(cmd.contains("/usr/local/bin/tako-server.prev"));
    assert!(cmd.contains("/usr/local/bin/tako-server"));
}

#[test]
fn remote_cleanup_previous_binary_command_removes_prev_binary() {
    let cmd = remote_cleanup_previous_binary_command();
    assert!(cmd.contains("rm -f /usr/local/bin/tako-server.prev"));
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
