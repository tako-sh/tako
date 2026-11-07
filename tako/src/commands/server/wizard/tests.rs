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
}

#[test]
fn parse_detected_libc_rejects_unknown_values() {
    for libc in ["musl", "uclibc"] {
        let err = parse_detected_libc(libc).unwrap_err();
        assert!(err.contains("requires glibc"));
    }
}

#[test]
fn public_ports_from_cli_defaults_omitted_pair_member() {
    assert_eq!(
        public_ports_from_cli(Some(8080), None).unwrap(),
        Some(ServerPublicPorts {
            http_port: 8080,
            https_port: 443,
        })
    );
    assert_eq!(
        public_ports_from_cli(None, Some(8443)).unwrap(),
        Some(ServerPublicPorts {
            http_port: 80,
            https_port: 8443,
        })
    );
}

#[test]
fn public_ports_from_cli_rejects_invalid_values() {
    assert!(public_ports_from_cli(Some(0), Some(8443)).is_err());
    assert!(public_ports_from_cli(Some(8080), Some(8080)).is_err());
}
