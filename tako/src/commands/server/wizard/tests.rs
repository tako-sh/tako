use super::*;

#[test]
fn parse_detected_arch_normalizes_supported_aliases() {
    assert_eq!(parse_detected_arch("x86_64\n").unwrap(), "x86_64");
    assert_eq!(parse_detected_arch("amd64\n").unwrap(), "x86_64");
    assert_eq!(parse_detected_arch("arm64\n").unwrap(), "aarch64");
}

#[test]
fn parse_detected_arch_rejects_unknown_values() {
    let err = parse_detected_arch("sparc\n").unwrap_err();
    assert!(err.contains("Unsupported server architecture"));
}

#[test]
fn parse_detected_libc_normalizes_supported_aliases() {
    assert_eq!(parse_detected_libc("glibc\n").unwrap(), "glibc");
    assert_eq!(parse_detected_libc("GNU libc\n").unwrap(), "glibc");
    assert_eq!(parse_detected_libc("musl\n").unwrap(), "musl");
}

#[test]
fn parse_detected_libc_rejects_unknown_values() {
    let err = parse_detected_libc("uclibc\n").unwrap_err();
    assert!(err.contains("Unsupported server libc"));
}

#[test]
fn remote_management_message_mentions_tailscale_without_endpoint_details() {
    let message = remote_management_unavailable_message();

    assert!(message.contains("requires Tailscale"));
    assert!(message.contains("MagicDNS"));
    assert!(!message.contains("endpoint"));
    assert!(!message.contains("9844"));
}

#[test]
fn server_not_installed_message_is_actionable() {
    let message = server_not_installed_message();

    assert!(message.contains("tako-server is not installed"));
    assert!(message.contains("try again"));
}
