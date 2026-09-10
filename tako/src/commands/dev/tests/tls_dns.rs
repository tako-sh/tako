use super::*;

#[cfg(target_os = "macos")]
#[test]
fn local_dns_resolver_template_targets_loopback_port() {
    assert_eq!(
        local_dns_resolver_contents(53535),
        "nameserver 127.0.0.1\nport 53535\n"
    );
}

#[test]
fn dev_server_tls_paths_are_under_certs_dir() {
    let home = Path::new("/tmp/tako-home");
    let (cert_path, key_path) = dev_server_tls_paths_for_home(home);
    assert_eq!(
        cert_path,
        Path::new("/tmp/tako-home/certs/fullchain.pem").to_path_buf()
    );
    assert_eq!(
        key_path,
        Path::new("/tmp/tako-home/certs/privkey.pem").to_path_buf()
    );
}

#[test]
fn ensure_dev_server_tls_material_writes_cert_and_key_when_missing() {
    let temp = TempDir::new().unwrap();
    let ca = LocalCA::generate().unwrap();
    let changed = ensure_dev_server_tls_material_for_home(&ca, temp.path(), "demo").unwrap();
    assert!(changed);

    let (cert_path, key_path) = dev_server_tls_paths_for_home(temp.path());
    let names_path = dev_server_tls_names_path_for_home(temp.path());
    let cert = std::fs::read_to_string(cert_path).unwrap();
    let key = std::fs::read_to_string(key_path).unwrap();
    let names = std::fs::read_to_string(names_path).unwrap();
    assert!(cert.contains("BEGIN CERTIFICATE"));
    assert!(key.contains("BEGIN PRIVATE KEY"));
    assert!(names.contains("*.demo.test"));
}

#[test]
fn ensure_dev_server_tls_material_keeps_existing_files() {
    let temp = TempDir::new().unwrap();
    let (cert_path, key_path) = dev_server_tls_paths_for_home(temp.path());
    let names_path = dev_server_tls_names_path_for_home(temp.path());
    std::fs::create_dir_all(cert_path.parent().unwrap()).unwrap();
    std::fs::write(&cert_path, "existing-cert").unwrap();
    std::fs::write(&key_path, "existing-key").unwrap();
    std::fs::write(
        &names_path,
        r#"[
  "*.demo.tako.test",
  "*.demo.test",
  "*.tako.test",
  "*.test",
  "demo.tako.test",
  "demo.test",
  "tako.test",
  "test"
]"#,
    )
    .unwrap();

    let ca = LocalCA::generate().unwrap();
    // Write matching CA fingerprint so the check passes.
    std::fs::write(
        ca_fingerprint_path_for_home(temp.path()),
        ca_fingerprint(&ca),
    )
    .unwrap();

    let changed = ensure_dev_server_tls_material_for_home(&ca, temp.path(), "demo").unwrap();
    assert!(!changed);

    let cert = std::fs::read_to_string(cert_path).unwrap();
    let key = std::fs::read_to_string(key_path).unwrap();
    assert_eq!(cert, "existing-cert");
    assert_eq!(key, "existing-key");
}

#[test]
fn ensure_dev_server_tls_material_regenerates_when_ca_changes() {
    let temp = TempDir::new().unwrap();
    let ca1 = LocalCA::generate().unwrap();
    let changed = ensure_dev_server_tls_material_for_home(&ca1, temp.path(), "demo").unwrap();
    assert!(changed);

    // Same CA, same names → no change.
    let changed = ensure_dev_server_tls_material_for_home(&ca1, temp.path(), "demo").unwrap();
    assert!(!changed);

    // Different CA, same names → must regenerate.
    let ca2 = LocalCA::generate().unwrap();
    let changed = ensure_dev_server_tls_material_for_home(&ca2, temp.path(), "demo").unwrap();
    assert!(changed);
}

#[test]
fn ensure_dev_server_tls_material_regenerates_files_without_names_manifest() {
    let temp = TempDir::new().unwrap();
    let (cert_path, key_path) = dev_server_tls_paths_for_home(temp.path());
    std::fs::create_dir_all(cert_path.parent().unwrap()).unwrap();
    std::fs::write(&cert_path, "existing-cert").unwrap();
    std::fs::write(&key_path, "existing-key").unwrap();

    let ca = LocalCA::generate().unwrap();
    let changed = ensure_dev_server_tls_material_for_home(&ca, temp.path(), "demo").unwrap();
    assert!(changed);

    let cert = std::fs::read_to_string(&cert_path).unwrap();
    let key = std::fs::read_to_string(&key_path).unwrap();
    let names = std::fs::read_to_string(dev_server_tls_names_path_for_home(temp.path())).unwrap();
    assert!(cert.contains("BEGIN CERTIFICATE"));
    assert!(key.contains("BEGIN PRIVATE KEY"));
    assert!(names.contains("*.demo.test"));
}

#[test]
fn ensure_dev_server_tls_material_merges_names_for_multiple_apps() {
    let temp = TempDir::new().unwrap();
    let ca = LocalCA::generate().unwrap();
    let first_changed = ensure_dev_server_tls_material_for_home(&ca, temp.path(), "alpha")
        .expect("first cert write");
    assert!(first_changed);
    let second_changed = ensure_dev_server_tls_material_for_home(&ca, temp.path(), "beta")
        .expect("second cert write");
    assert!(second_changed);

    let names = std::fs::read_to_string(dev_server_tls_names_path_for_home(temp.path())).unwrap();
    assert!(names.contains("*.alpha.test"));
    assert!(names.contains("*.beta.test"));
}

#[cfg(target_os = "macos")]
#[test]
fn parse_local_dns_resolver_extracts_nameserver_and_port() {
    let (ns, port) =
        parse_local_dns_resolver("# tako resolver\nnameserver 127.0.0.1\nport 53535\n");
    assert_eq!(ns.as_deref(), Some("127.0.0.1"));
    assert_eq!(port, Some(53535));
}

#[cfg(target_os = "macos")]
#[test]
fn parse_local_dns_resolver_prefers_latest_valid_entries() {
    let (ns, port) = parse_local_dns_resolver(
        "# stale resolver values\nnameserver 10.0.0.1\nport not-a-number\nnameserver 127.0.0.1\nport 53535\n",
    );
    assert_eq!(ns.as_deref(), Some("127.0.0.1"));
    assert_eq!(port, Some(53535));
}

#[cfg(target_os = "macos")]
#[test]
fn parse_local_dns_resolver_ignores_unknown_lines() {
    let (ns, port) = parse_local_dns_resolver(
        "# unrelated\nsearch local\noptions ndots:1\nnameserver 127.0.0.1\n",
    );
    assert_eq!(ns.as_deref(), Some("127.0.0.1"));
    assert_eq!(port, None);
}

#[cfg(target_os = "macos")]
#[test]
fn ensure_local_dns_resolver_non_interactive_error_is_actionable() {
    let err = ensure_local_dns_resolver_configured(65535)
        .expect_err("non-interactive setup should fail when resolver is not configured");
    let text = err.to_string();
    assert!(text.contains("/etc/resolver/tako"));
    assert!(text.contains("run `tako dev` interactively once"));
}

#[cfg(target_os = "macos")]
#[test]
fn sudo_setup_action_items_uses_expected_order() {
    let items = sudo_setup_action_items(
        Some("Trust the Tako local CA for trusted https://*.test"),
        true,
        Some("Install the local dev proxy for 127.77.0.1:80/443"),
    );
    assert_eq!(
        items,
        vec![
            "Trust the Tako local CA for trusted https://*.test".to_string(),
            local_dns_sudo_action_line().to_string(),
            "Install the local dev proxy for 127.77.0.1:80/443".to_string(),
        ]
    );
}

#[cfg(target_os = "macos")]
#[test]
fn sudo_setup_action_items_omits_absent_steps() {
    let items = sudo_setup_action_items(None, false, Some("Repair dev proxy"));
    assert_eq!(items, vec!["Repair dev proxy".to_string()]);
}

#[tokio::test]
async fn local_https_probe_rejects_untrusted_certificate() {
    use openssl::pkey::PKey;
    use openssl::ssl::{SslAcceptor, SslMethod};
    use openssl::x509::X509;
    use std::io::{Read, Write};

    let ca = LocalCA::generate().unwrap();
    let cert = ca.generate_leaf_cert("probe.test").unwrap();
    let mut acceptor = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
    acceptor
        .set_certificate(&X509::from_pem(cert.cert_pem.as_bytes()).unwrap())
        .unwrap();
    acceptor
        .set_private_key(&PKey::private_key_from_pem(cert.key_pem.as_bytes()).unwrap())
        .unwrap();
    let acceptor = acceptor.build();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::task::spawn_blocking(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let Ok(mut stream) = acceptor.accept(stream) else {
            return;
        };
        let mut request = [0; 4096];
        stream.read(&mut request).unwrap();
        // Port zero makes following the redirect fail without contacting a remote host.
        stream.write_all(b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:0/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    });
    let result = crate::commands::dev::prepare::local::localhost_https_host_reachable_via_ip(
        "probe.test",
        std::net::Ipv4Addr::LOCALHOST,
        port,
        2000,
    )
    .await;
    server.await.unwrap();
    assert!(
        result.is_err(),
        "the probe must not accept an untrusted local certificate: {result:?}"
    );
}

#[tokio::test]
async fn local_https_probe_accepts_certificate_from_explicitly_trusted_ca() {
    use openssl::pkey::PKey;
    use openssl::ssl::{SslAcceptor, SslMethod};
    use openssl::x509::X509;
    use std::io::{Read, Write};

    let ca = LocalCA::generate().unwrap();
    let cert = ca.generate_leaf_cert("probe.test").unwrap();
    let mut acceptor = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
    acceptor
        .set_certificate(&X509::from_pem(cert.cert_pem.as_bytes()).unwrap())
        .unwrap();
    acceptor
        .set_private_key(&PKey::private_key_from_pem(cert.key_pem.as_bytes()).unwrap())
        .unwrap();
    let acceptor = acceptor.build();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::task::spawn_blocking(move || {
        let (stream, _) = listener.accept().unwrap();
        let Ok(mut stream) = acceptor.accept(stream) else {
            return;
        };
        let mut request = [0; 4096];
        stream.read(&mut request).unwrap();
        stream
            .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();
    });

    let result =
        crate::commands::dev::prepare::local::localhost_https_host_reachable_via_ip_with_root(
            "probe.test",
            std::net::Ipv4Addr::LOCALHOST,
            port,
            2000,
            ca.ca_cert_pem(),
        )
        .await;

    server.await.unwrap();
    assert!(
        result.is_ok(),
        "an explicitly trusted CA should succeed: {result:?}"
    );
}

#[tokio::test]
async fn local_https_probe_requires_loopback_destination() {
    let error = crate::commands::dev::prepare::local::localhost_https_host_reachable_via_ip(
        "probe.test",
        std::net::Ipv4Addr::new(192, 0, 2, 1),
        443,
        10,
    )
    .await
    .unwrap_err();
    assert!(error.contains("loopback"));
}
