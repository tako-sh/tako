use super::*;

thread_local! {
    static RENEWAL_FAILURE: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

pub(super) fn renewal_boundary() -> Result<()> {
    RENEWAL_FAILURE.with(|remaining| match remaining.get() {
        Some(0) => {
            remaining.set(None);
            Err(std::io::Error::other("injected renewal interruption").into())
        }
        Some(n) => {
            remaining.set(Some(n - 1));
            Ok(())
        }
        None => Ok(()),
    })
}

fn prepare(path: &Path) -> Result<ServerIdentity> {
    ServerIdentity::prepare(
        path,
        "1234567890abcdef1234567890abcdef".parse()?,
        "test-server".parse()?,
        "192.0.2.42:51820".parse().unwrap(),
        host_key(),
        22,
        443,
    )
}

fn host_key() -> SshPublicKey {
    use std::sync::OnceLock;
    static KEY: OnceLock<SshPublicKey> = OnceLock::new();
    KEY.get_or_init(|| {
        LocalDeviceKeys::generate()
            .unwrap()
            .ssh_public_key()
            .unwrap()
    })
    .clone()
}

#[test]
fn load_preserves_identity_and_exposes_the_local_keys_path() {
    let temp = crate::private_tempdir();
    let path = temp.path().join("identity");
    let first = prepare(&path).unwrap();
    let before = snapshot(&path);
    let loaded = ServerIdentity::load(&path).unwrap();
    assert_eq!(loaded.request(), first.request());
    assert_eq!(loaded.server_info(), first.server_info());
    assert_eq!(loaded.tls_paths(), first.tls_paths());
    assert_eq!(loaded.gossip_paths(), first.gossip_paths());
    assert_eq!(loaded.keys_path(), first.keys_path());
    assert_eq!(loaded.keys_path(), path.join(DEVICE_KEYS));
    let filename = Path::new(loaded.keys_path().file_name().unwrap());
    assert_eq!(filename.components().count(), 1);
    let keys = LocalDeviceKeys::load(&path.join(filename)).unwrap();
    assert_eq!(
        keys.wg_public_key().unwrap(),
        loaded.request().wg_public_key
    );
    assert_eq!(
        keys.ssh_public_key().unwrap(),
        loaded.request().ssh_public_key
    );
    let (challenge, pending) = WgChallenge::begin(
        first.request(),
        first.prepared.config.network_id.clone(),
        100,
        200,
    )
    .unwrap();
    pending
        .verify(&loaded.answer_challenge(&challenge, 101).unwrap(), 101)
        .unwrap();
    assert_eq!(snapshot(&path), before);
}

#[test]
fn load_never_creates_or_completes_unprepared_state() {
    let temp = crate::private_tempdir();
    let missing = temp.path().join("missing");
    assert!(ServerIdentity::load(&missing).is_err());
    assert!(!missing.exists());
    assert!(ServerIdentity::load(temp.path()).is_err());
    assert!(snapshot(temp.path()).is_empty());
    prepare(temp.path()).unwrap();
    std::fs::remove_file(temp.path().join(METADATA)).unwrap();
    let before = snapshot(temp.path());
    assert!(
        ServerIdentity::load(temp.path())
            .unwrap_err()
            .to_string()
            .contains("not prepared")
    );
    assert_eq!(snapshot(temp.path()), before);
}

#[test]
fn restart_preserves_keys_request_and_public_pins() {
    let temp = crate::private_tempdir();
    let path = temp.path().join("identity");
    let first = prepare(&path).unwrap();
    let second = prepare(&path).unwrap();
    assert_eq!(first.request(), second.request());
    assert_eq!(first.server_info(), second.server_info());
    assert_eq!(
        first.keys().wireguard_private_key().unwrap(),
        second.keys().wireguard_private_key().unwrap()
    );
    assert_eq!(
        first.keys().ssh_private_key().unwrap(),
        second.keys().ssh_private_key().unwrap()
    );
    assert_eq!(first.tls_paths(), second.tls_paths());
    let (challenge, pending) = WgChallenge::begin(
        first.request(),
        first.prepared.config.network_id.clone(),
        100,
        200,
    )
    .unwrap();
    let verified = pending
        .verify(&second.answer_challenge(&challenge, 101).unwrap(), 101)
        .unwrap();
    assert_eq!(verified.request().kind, DeviceKind::Server);
    assert_eq!(verified.request(), first.request());
    assert!(!format!("{first:?}").contains("PRIVATE"));
}
fn openssl_verify(paths: &ServerTlsPaths, purpose: &str, name_flag: &str, name: &str) -> bool {
    let result = std::process::Command::new("openssl")
        .args(["verify", "-x509_strict", "-CAfile"])
        .arg(&paths.ca_certificate)
        .args(["-purpose", purpose, name_flag, name])
        .arg(&paths.certificate)
        .output()
        .unwrap();
    result.status.success()
}

#[test]
fn openssl_verifies_chain_sans_and_distinct_tls_roles() {
    let temp = crate::private_tempdir();
    let identity = prepare(temp.path()).unwrap();
    let names = certificate_names(&identity.prepared.config, identity.request()).unwrap();
    let tls = identity.tls_paths();
    let gossip = identity.gossip_paths();
    assert!(openssl_verify(
        tls,
        "sslserver",
        "-verify_hostname",
        &names.hostname
    ));
    assert!(!openssl_verify(
        tls,
        "sslclient",
        "-verify_hostname",
        &names.hostname
    ));
    assert!(!openssl_verify(
        tls,
        "sslserver",
        "-verify_hostname",
        &names.gossip
    ));
    assert!(!openssl_verify(
        tls,
        "sslserver",
        "-verify_hostname",
        "wrong.tako.internal"
    ));
    assert!(!openssl_verify(
        tls,
        "sslserver",
        "-verify_ip",
        "192.0.2.99"
    ));
    for ip in names.ips {
        assert!(openssl_verify(
            tls,
            "sslserver",
            "-verify_ip",
            &ip.to_string()
        ));
        assert!(openssl_verify(
            gossip,
            "sslserver",
            "-verify_ip",
            &ip.to_string()
        ));
    }
    for hostname in [&names.hostname, &names.gossip] {
        for purpose in ["sslclient", "sslserver"] {
            assert!(openssl_verify(
                gossip,
                purpose,
                "-verify_hostname",
                hostname
            ));
        }
    }
    let mut wrong_issuer = tls.clone();
    wrong_issuer.ca_certificate = gossip.ca_certificate.clone();
    assert!(!openssl_verify(
        &wrong_issuer,
        "sslserver",
        "-verify_hostname",
        &names.hostname
    ));
}

#[test]
fn all_tls_keys_are_independent_and_match_their_certificates() {
    use std::{collections::BTreeSet, fs};
    let temp = crate::private_tempdir();
    let first = prepare(&temp.path().join("first")).unwrap();
    let second = prepare(&temp.path().join("second")).unwrap();
    let mut public_keys = BTreeSet::new();
    for identity in [&first, &second] {
        let info = identity.server_info();
        for (paths, pin) in [
            (
                identity.tls_paths(),
                info.https_certificate_pem.as_ref().unwrap(),
            ),
            (
                identity.gossip_paths(),
                info.corrosion_ca_certificate_pem.as_ref().unwrap(),
            ),
        ] {
            assert_eq!(fs::read_to_string(&paths.ca_certificate).unwrap(), *pin);
            assert_eq!(X509::stack_from_pem(pin.as_bytes()).unwrap().len(), 1);
            for (cert_path, key_path) in [
                (&paths.ca_certificate, &paths.ca_private_key),
                (&paths.certificate, &paths.private_key),
            ] {
                let cert = X509::from_pem(&fs::read(cert_path).unwrap()).unwrap();
                let key = PKey::private_key_from_pem(&fs::read(key_path).unwrap()).unwrap();
                assert!(cert.public_key().unwrap().public_eq(&key));
                assert!(public_keys.insert(key.public_key_to_der().unwrap()));
            }
            let ca = X509::from_pem(pin.as_bytes()).unwrap();
            assert!(ca.verify(&ca.public_key().unwrap()).unwrap());
            let chain = X509::stack_from_pem(&fs::read(&paths.fullchain).unwrap()).unwrap();
            assert_eq!(chain.len(), 2);
            assert_eq!(chain[1].to_pem().unwrap(), pin.as_bytes());
            assert_ne!(chain[0].to_pem().unwrap(), pin.as_bytes());
        }
        assert_eq!(
            info.https_spki_sha256,
            info.https_spki_fingerprint().unwrap()
        );
        let metadata = fs::read_to_string(
            identity
                .tls_paths()
                .certificate
                .parent()
                .unwrap()
                .join(METADATA),
        )
        .unwrap();
        assert!(!metadata.contains("PRIVATE KEY"));
        assert!(!metadata.contains(identity.keys().wireguard_private_key().unwrap().as_str()));
        let debug = format!("{identity:?}");
        assert!(!debug.contains(identity.keys().wireguard_private_key().unwrap().as_str()));
    }
}

#[test]
fn challenge_rejects_other_requests_networks_and_expiry() {
    let temp = crate::private_tempdir();
    let identity = prepare(&temp.path().join("first")).unwrap();
    let other = prepare(&temp.path().join("other")).unwrap();
    let network = identity.prepared.config.network_id.clone();
    let (challenge, _) = WgChallenge::begin(other.request(), network.clone(), 100, 200).unwrap();
    assert!(identity.answer_challenge(&challenge, 101).is_err());
    let (mut challenge, _) = WgChallenge::begin(identity.request(), network, 100, 200).unwrap();
    assert!(identity.answer_challenge(&challenge, 200).is_err());
    challenge.network_id = NetworkId::random().unwrap();
    assert!(identity.answer_challenge(&challenge, 101).is_err());
}

#[test]
fn leaf_renewal_with_the_local_ca_preserves_public_pins() {
    use std::fs;
    let temp = crate::private_tempdir();
    let identity = prepare(temp.path()).unwrap();
    let names = certificate_names(&identity.prepared.config, identity.request()).unwrap();
    for (paths, gossip) in [
        (identity.tls_paths(), false),
        (identity.gossip_paths(), true),
    ] {
        let ca_key_pem = fs::read(&paths.ca_private_key).unwrap();
        let old_leaf = fs::read(&paths.certificate).unwrap();
        // An external renewal re-signs the existing leaf key with the local
        // CA under a new serial and validity. (OpenSSL 3.0's `x509 -CA`
        // refuses to re-sign a CA-issued certificate, so build it here.)
        let ca = openssl::x509::X509::from_pem(&fs::read(&paths.ca_certificate).unwrap()).unwrap();
        let ca_key = openssl::pkey::PKey::private_key_from_pem(&ca_key_pem).unwrap();
        let leaf_key =
            openssl::pkey::PKey::private_key_from_pem(&fs::read(&paths.private_key).unwrap())
                .unwrap();
        let now = current_time().unwrap();
        let renewed = issue_leaf(
            &ca,
            &ca_key,
            &leaf_key,
            &names,
            gossip,
            now,
            now + 364 * 86_400,
        )
        .unwrap();
        let leaf_pem = renewed.to_pem().unwrap();
        assert_ne!(leaf_pem, old_leaf);
        fs::write(&paths.certificate, &leaf_pem).unwrap();
        fs::write(
            &paths.fullchain,
            [leaf_pem, fs::read(&paths.ca_certificate).unwrap()].concat(),
        )
        .unwrap();
        assert_eq!(fs::read(&paths.ca_private_key).unwrap(), ca_key_pem);
    }
    let renewed = prepare(temp.path()).unwrap();
    assert_eq!(identity.request(), renewed.request());
    assert_eq!(identity.server_info(), renewed.server_info());
}

#[test]
fn public_ipv6_endpoint_is_a_verified_ip_san() {
    let temp = crate::private_tempdir();
    let identity = ServerIdentity::prepare(
        temp.path(),
        NetworkId::random().unwrap(),
        "ipv6-server".parse().unwrap(),
        "[2001:db8::42]:51820".parse().unwrap(),
        host_key(),
        22,
        443,
    )
    .unwrap();
    for paths in [identity.tls_paths(), identity.gossip_paths()] {
        assert!(openssl_verify(
            paths,
            "sslserver",
            "-verify_ip",
            "2001:db8::42"
        ));
    }
}
fn snapshot(path: &Path) -> std::collections::BTreeMap<std::ffi::OsString, Vec<u8>> {
    std::fs::read_dir(path)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.file_name(), std::fs::read(entry.path()).unwrap())
        })
        .collect()
}

fn prepare_config(path: &Path, config: Config) -> Result<ServerIdentity> {
    ServerIdentity::prepare(
        path,
        config.network_id,
        config.name,
        config.wireguard_endpoint,
        config.ssh_host_public_key,
        config.ssh_port,
        config.https_port,
    )
}

#[test]
fn every_configuration_change_is_explicit_and_never_overwrites() {
    let temp = crate::private_tempdir();
    let identity = prepare(temp.path()).unwrap();
    let original = snapshot(temp.path());
    for change in 0..6 {
        let mut config = identity.prepared.config.clone();
        match change {
            0 => config.network_id = NetworkId::random().unwrap(),
            1 => config.name = "other-server".parse().unwrap(),
            2 => config.wireguard_endpoint = "192.0.2.99:51820".parse().unwrap(),
            3 => {
                config.ssh_host_public_key = LocalDeviceKeys::generate()
                    .unwrap()
                    .ssh_public_key()
                    .unwrap()
            }
            4 => config.ssh_port = 2222,
            5 => config.https_port = 8443,
            _ => unreachable!(),
        }
        let error = prepare_config(temp.path(), config).unwrap_err();
        assert!(error.to_string().contains("configuration mismatch"));
        assert_eq!(snapshot(temp.path()), original);
    }
}

#[test]
fn invalid_endpoints_fail_before_creating_any_files() {
    let temp = crate::private_tempdir();
    let seed = prepare(&temp.path().join("seed")).unwrap();
    for (index, endpoint) in [
        "127.0.0.1:51820",
        "0.0.0.0:51820",
        "192.0.2.1:0",
        "[ff02::1]:51820",
    ]
    .into_iter()
    .enumerate()
    {
        let mut config = seed.prepared.config.clone();
        config.wireguard_endpoint = endpoint.parse().unwrap();
        let path = temp.path().join(index.to_string());
        assert!(prepare_config(&path, config).is_err());
        assert!(!path.exists());
    }
    for https in [true, false] {
        let mut config = seed.prepared.config.clone();
        if https {
            config.https_port = 0;
        } else {
            config.ssh_port = 0;
        }
        let path = temp.path().join("bad-port");
        assert!(prepare_config(&path, config).is_err());
        assert!(!path.exists());
    }
}

#[test]
fn missing_files_and_interrupted_prepare_fail_closed() {
    let temp = crate::private_tempdir();
    let seed = prepare(&temp.path().join("seed")).unwrap();
    let files = snapshot(seed.tls_paths().certificate.parent().unwrap());
    for missing in files.keys().filter(|name| *name != "prepare-started") {
        let case = crate::private_tempdir();
        prepare(case.path()).unwrap();
        std::fs::remove_file(case.path().join(missing)).unwrap();
        let before = snapshot(case.path());
        assert!(prepare(case.path()).is_err(), "missing {missing:?}");
        assert!(ServerIdentity::load(case.path()).is_err());
        assert!(ServerIdentity::renew(case.path()).is_err());
        assert_eq!(snapshot(case.path()), before);
    }
    for filename in [
        "prepare-started",
        DEVICE_KEYS,
        "https-ca-key.pem",
        "unrelated",
    ] {
        let case = crate::private_tempdir();
        std::fs::write(case.path().join(filename), b"do not replace").unwrap();
        let before = snapshot(case.path());
        assert!(prepare(case.path()).is_err());
        assert_eq!(snapshot(case.path()), before);
    }
}

#[test]
fn metadata_is_bounded_strict_and_bound_to_keys_and_pins() {
    let temp = crate::private_tempdir();
    let identity = prepare(temp.path()).unwrap();
    let original = serde_json::to_value(&identity.prepared).unwrap();
    let mut unknown = original.clone();
    unknown["private_key_path"] = "../../victim".into();
    let mut version = original.clone();
    version["version"] = 2.into();
    let mut kind = original.clone();
    kind["request"]["kind"] = "laptop".into();
    let mut pin = original.clone();
    pin["server_info"]["https_certificate_pem"] =
        original["server_info"]["corrosion_ca_certificate_pem"].clone();
    let mut endpoint = original.clone();
    endpoint["server_info"]["https_port"] = 8443.into();
    let mut cases = vec![b"{".to_vec(), vec![b' '; MAX_FILE_BYTES + 1]];
    cases.extend(
        [unknown, version, kind, pin, endpoint]
            .iter()
            .map(|v| serde_json::to_vec(v).unwrap()),
    );
    for bytes in cases {
        std::fs::write(temp.path().join(METADATA), bytes).unwrap();
        let before = snapshot(temp.path());
        assert!(prepare(temp.path()).is_err());
        assert!(ServerIdentity::load(temp.path()).is_err());
        assert_eq!(snapshot(temp.path()), before);
    }
}

#[test]
fn rejects_substituted_device_and_tls_keys() {
    let temp = crate::private_tempdir();
    let first_path = temp.path().join("first");
    let other_path = temp.path().join("other");
    let first = prepare(&first_path).unwrap();
    let other = prepare(&other_path).unwrap();
    for (target, substitute) in [
        (first_path.join(DEVICE_KEYS), other_path.join(DEVICE_KEYS)),
        (
            first.tls_paths().private_key.clone(),
            other.tls_paths().private_key.clone(),
        ),
        (
            first.gossip_paths().ca_private_key.clone(),
            other.gossip_paths().ca_private_key.clone(),
        ),
        (
            first.tls_paths().certificate.clone(),
            other.tls_paths().certificate.clone(),
        ),
    ] {
        let original = std::fs::read(&target).unwrap();
        std::fs::write(&target, std::fs::read(substitute).unwrap()).unwrap();
        let before = snapshot(&first_path);
        assert!(prepare(&first_path).is_err());
        assert_eq!(snapshot(&first_path), before);
        std::fs::write(target, original).unwrap();
    }
}

#[test]
fn directories_reject_permissions_symlinks_and_parent_traversal() {
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };
    let temp = crate::private_tempdir();
    let private = temp.path().join("private");
    let identity = prepare(&private).unwrap();
    let before = snapshot(&private);
    fs::set_permissions(&private, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(prepare(&private).is_err());
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
    let alias = temp.path().join("alias");
    symlink(&private, &alias).unwrap();
    assert!(prepare(&alias).is_err());
    assert!(ServerIdentity::load(&alias).is_err());
    assert!(ServerIdentity::renew(&alias).is_err());
    assert!(prepare(&alias.join("new-identity")).is_err());
    assert!(!private.join("new-identity").exists());
    assert!(prepare(&private.join("..").join("traversal")).is_err());
    assert!(!temp.path().join("traversal").exists());
    assert_eq!(snapshot(&private), before);
    assert_eq!(prepare(&private).unwrap().request(), identity.request());
}

#[test]
fn files_require_regular_single_link_owned_0600_and_no_symlinks() {
    use std::{
        fs,
        os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    };
    let temp = crate::private_tempdir();
    let path = temp.path().join("identity");
    prepare(&path).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o700);
    for name in snapshot(&path)
        .keys()
        .filter(|name| *name != "prepare-started")
    {
        let file = path.join(name);
        let meta = fs::metadata(&file).unwrap();
        assert_eq!(meta.mode() & 0o7777, 0o600);
        assert_eq!(meta.uid(), rustix::process::geteuid().as_raw());
        assert_eq!(meta.nlink(), 1);
        let original = fs::read(&file).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(prepare(&path).is_err(), "permissions {name:?}");
        assert!(ServerIdentity::load(&path).is_err());
        assert!(ServerIdentity::renew(&path).is_err());
        assert_eq!(fs::read(&file).unwrap(), original);
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        let link = temp.path().join("hardlink");
        fs::hard_link(&file, &link).unwrap();
        assert!(prepare(&path).is_err(), "hardlink {name:?}");
        assert!(ServerIdentity::load(&path).is_err());
        assert!(ServerIdentity::renew(&path).is_err());
        fs::remove_file(&link).unwrap();
        let saved = temp.path().join("saved");
        fs::rename(&file, &saved).unwrap();
        symlink(&saved, &file).unwrap();
        assert!(prepare(&path).is_err(), "symlink {name:?}");
        assert!(ServerIdentity::load(&path).is_err());
        assert!(ServerIdentity::renew(&path).is_err());
        assert_eq!(fs::read(&saved).unwrap(), original);
        fs::remove_file(&file).unwrap();
        fs::create_dir(&file).unwrap();
        assert!(prepare(&path).is_err(), "directory {name:?}");
        fs::remove_dir(&file).unwrap();
        fs::rename(&saved, &file).unwrap();
    }
    prepare(&path).unwrap();
}

#[test]
fn concurrent_prepare_never_replaces_the_winners_identity() {
    let temp = crate::private_tempdir();
    let barrier = std::sync::Barrier::new(4);
    let results = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    prepare(temp.path())
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    let winners: Vec<_> = results.into_iter().filter_map(Result::ok).collect();
    assert!(!winners.is_empty());
    let reloaded = prepare(temp.path()).unwrap();
    for winner in winners {
        assert_eq!(winner.request(), reloaded.request());
        assert_eq!(winner.server_info(), reloaded.server_info());
    }
}
fn materials(identity: &ServerIdentity) -> [TlsMaterial; 2] {
    let dir = secure::Directory::open(identity.keys_path().parent().unwrap(), false).unwrap();
    identity
        .validate_at(&dir, current_time().unwrap(), LeafValidation::Current, None)
        .unwrap()
}

fn assert_immutable_files(
    before: &std::collections::BTreeMap<std::ffi::OsString, Vec<u8>>,
    path: &Path,
) {
    for (name, bytes) in before {
        if !matches!(
            name.to_str().unwrap(),
            "https-leaf.pem"
                | "https-fullchain.pem"
                | "gossip-leaf.pem"
                | "gossip-fullchain.pem"
                | RENEWAL_JOURNAL
                | RENEWAL_STAGE
        ) {
            assert_eq!(
                &std::fs::read(path.join(name)).unwrap(),
                bytes,
                "changed {name:?}"
            );
        }
    }
}

#[test]
fn native_renewal_preserves_keys_pins_and_renews_expiring_or_expired_leaves() {
    use std::{fs, os::unix::fs::MetadataExt};
    for remaining in [30 * DAY, -DAY] {
        let temp = crate::private_tempdir();
        let first = prepare(temp.path()).unwrap();
        let original = snapshot(temp.path());
        let old = materials(&first);
        let latest_expiry = old
            .iter()
            .map(|tls| timestamp(tls.leaf.not_after()).unwrap())
            .max()
            .unwrap();
        let now = latest_expiry - remaining;
        let renewed = ServerIdentity::renew_at(temp.path(), now).unwrap();
        assert_eq!(first.request(), renewed.request());
        assert_eq!(first.server_info(), renewed.server_info());
        assert_immutable_files(&original, temp.path());
        let names = certificate_names(&first.prepared.config, first.request()).unwrap();
        for (paths, gossip) in [(renewed.tls_paths(), false), (renewed.gossip_paths(), true)] {
            let leaf = X509::from_pem(&fs::read(&paths.certificate).unwrap()).unwrap();
            assert_eq!(timestamp(leaf.not_before()).unwrap(), now);
            assert_eq!(timestamp(leaf.not_after()).unwrap(), now + LEAF_LIFETIME);
            assert_ne!(
                &fs::read(&paths.certificate).unwrap(),
                &original[paths.certificate.file_name().unwrap()]
            );
            for purpose in ["sslserver", "sslclient"] {
                let result = std::process::Command::new("openssl")
                    .args([
                        "verify",
                        "-x509_strict",
                        "-attime",
                        &now.to_string(),
                        "-CAfile",
                    ])
                    .arg(&paths.ca_certificate)
                    .args(["-purpose", purpose, "-verify_hostname", &names.hostname])
                    .arg(&paths.certificate)
                    .output()
                    .unwrap();
                assert_eq!(
                    result.status.success(),
                    gossip || purpose == "sslserver",
                    "{}",
                    String::from_utf8_lossy(&result.stderr)
                );
            }
            let meta = fs::metadata(&paths.certificate).unwrap();
            assert_eq!(meta.mode() & 0o7777, 0o600);
            assert_eq!(meta.nlink(), 1);
            assert_eq!(meta.uid(), rustix::process::geteuid().as_raw());
        }
        let before = snapshot(temp.path());
        let meta = fs::metadata(&renewed.tls_paths().fullchain).unwrap();
        ServerIdentity::renew_at(temp.path(), now).unwrap();
        assert_eq!(snapshot(temp.path()), before);
        let again = fs::metadata(&renewed.tls_paths().fullchain).unwrap();
        assert_eq!(meta.ino(), again.ino());
        assert_eq!(meta.modified().unwrap(), again.modified().unwrap());
    }
}

#[test]
fn renewal_is_read_only_before_threshold_and_never_creates_identity() {
    let temp = crate::private_tempdir();
    let missing = temp.path().join("missing");
    assert!(ServerIdentity::renew(&missing).is_err());
    assert!(!missing.exists());
    assert!(ServerIdentity::renew(temp.path()).is_err());
    assert!(snapshot(temp.path()).is_empty());
    let identity = prepare(temp.path()).unwrap();
    let before = snapshot(temp.path());
    let renewed = ServerIdentity::renew(temp.path()).unwrap();
    assert_eq!(renewed.server_info(), identity.server_info());
    let expires = materials(&identity)
        .iter()
        .map(|m| timestamp(m.leaf.not_after()).unwrap())
        .min()
        .unwrap();
    ServerIdentity::renew_at(temp.path(), expires - RENEW_BEFORE - 1).unwrap();
    assert_eq!(snapshot(temp.path()), before);
    ServerIdentity::load(temp.path()).unwrap();
}

#[test]
fn renewal_caps_leaf_validity_and_refuses_expired_or_future_ca() {
    let temp = crate::private_tempdir();
    let identity = prepare(temp.path()).unwrap();
    let tls = materials(&identity);
    let ca_start = tls
        .iter()
        .map(|m| timestamp(m.ca.not_before()).unwrap())
        .max()
        .unwrap();
    let ca_end = tls
        .iter()
        .map(|m| timestamp(m.ca.not_after()).unwrap())
        .min()
        .unwrap();
    for now in [ca_start - 1, ca_end] {
        let before = snapshot(temp.path());
        assert!(ServerIdentity::renew_at(temp.path(), now).is_err());
        assert_eq!(snapshot(temp.path()), before);
    }
    ServerIdentity::renew_at(temp.path(), ca_end - 10 * DAY).unwrap();
    for paths in [identity.tls_paths(), identity.gossip_paths()] {
        let leaf = X509::from_pem(&std::fs::read(&paths.certificate).unwrap()).unwrap();
        let ca = X509::from_pem(&std::fs::read(&paths.ca_certificate).unwrap()).unwrap();
        assert_eq!(leaf.not_after(), ca.not_after());
    }
    let before = snapshot(temp.path());
    ServerIdentity::renew_at(temp.path(), ca_end - 9 * DAY).unwrap();
    assert_eq!(snapshot(temp.path()), before);
}

#[test]
fn renewal_refuses_corrupt_bindings_and_future_leaf_before_any_writes() {
    let temp = crate::private_tempdir();
    let identity = prepare(temp.path()).unwrap();
    let original = snapshot(temp.path());
    let [tls, _] = materials(&identity);
    let names = certificate_names(&identity.prepared.config, identity.request()).unwrap();
    let now = current_time().unwrap();
    let wrong_key = new_key().unwrap();
    for target in [
        &identity.tls_paths().ca_private_key,
        &identity.tls_paths().private_key,
    ] {
        std::fs::write(target, wrong_key.private_key_to_pem_pkcs8().unwrap()).unwrap();
        let before = snapshot(temp.path());
        assert!(ServerIdentity::renew_at(temp.path(), now + LEAF_LIFETIME + DAY).is_err());
        assert_eq!(snapshot(temp.path()), before);
        std::fs::write(target, &original[target.file_name().unwrap()]).unwrap();
    }
    for leaf in [
        issue_leaf(
            &tls.ca,
            &wrong_key,
            &tls.leaf_key,
            &names,
            false,
            now,
            now + DAY,
        )
        .unwrap(),
        issue_leaf(
            &tls.ca,
            &tls.ca_key,
            &tls.leaf_key,
            &names,
            false,
            now + DAY,
            now + LEAF_LIFETIME,
        )
        .unwrap(),
    ] {
        let pem = leaf.to_pem().unwrap();
        std::fs::write(&identity.tls_paths().certificate, &pem).unwrap();
        std::fs::write(
            &identity.tls_paths().fullchain,
            [pem, tls.ca.to_pem().unwrap()].concat(),
        )
        .unwrap();
        let before = snapshot(temp.path());
        assert!(ServerIdentity::renew_at(temp.path(), now).is_err());
        assert_eq!(snapshot(temp.path()), before);
    }
}
fn restore_snapshot(path: &Path, files: &std::collections::BTreeMap<std::ffi::OsString, Vec<u8>>) {
    let dir = secure::Directory::open(path, false).unwrap();
    for (name, bytes) in files {
        dir.write_new(name.to_str().unwrap(), bytes).unwrap();
    }
}

fn interrupt_renewal(path: &Path, now: i64, boundary: usize) -> Result<ServerIdentity> {
    RENEWAL_FAILURE.with(|failure| failure.set(Some(boundary)));
    let result = ServerIdentity::renew_at(path, now);
    RENEWAL_FAILURE.with(|failure| failure.set(None));
    result
}

fn assert_recovered(
    path: &Path,
    now: i64,
    original: &ServerIdentity,
    before: &std::collections::BTreeMap<std::ffi::OsString, Vec<u8>>,
) {
    let renewed = ServerIdentity::renew_at(path, now).unwrap();
    assert_eq!(renewed.request(), original.request());
    assert_eq!(renewed.server_info(), original.server_info());
    assert_immutable_files(before, path);
    assert!(!path.join(RENEWAL_JOURNAL).exists());
    assert!(!path.join(RENEWAL_STAGE).exists());
    let after = snapshot(path);
    ServerIdentity::renew_at(path, now).unwrap();
    assert_eq!(snapshot(path), after);
}

#[test]
fn renewal_recovers_at_every_write_and_sync_boundary_including_recovery_itself() {
    let seed = crate::private_tempdir();
    let original = prepare(seed.path()).unwrap();
    let before = snapshot(seed.path());
    let now = timestamp(materials(&original)[0].leaf.not_after()).unwrap() + DAY;
    let mut interrupted = Vec::new();
    let mut failures = 0;
    for boundary in 0..64 {
        let case = crate::private_tempdir();
        restore_snapshot(case.path(), &before);
        match interrupt_renewal(case.path(), now, boundary) {
            Ok(_) => break,
            Err(error) => assert!(
                error.to_string().contains("injected renewal interruption"),
                "{error}"
            ),
        }
        failures += 1;
        if case.path().join(RENEWAL_JOURNAL).exists() {
            let data = std::fs::read_to_string(case.path().join(RENEWAL_JOURNAL)).unwrap();
            assert!(!data.contains("PRIVATE"));
            assert!(
                ServerIdentity::load(case.path())
                    .unwrap_err()
                    .to_string()
                    .contains("pending leaf renewal")
            );
            if boundary == 8 {
                // Leaf rename is visible, while its fullchain still contains the old leaf.
                assert_ne!(
                    std::fs::read(case.path().join("https-leaf.pem")).unwrap(),
                    before[std::ffi::OsStr::new("https-leaf.pem")]
                );
                assert_eq!(
                    std::fs::read(case.path().join("https-fullchain.pem")).unwrap(),
                    before[std::ffi::OsStr::new("https-fullchain.pem")]
                );
            }
            if boundary == 7 || boundary == 8 {
                interrupted.push((snapshot(case.path()), if boundary == 7 { 24 } else { 17 }));
            }
        }
        assert_recovered(case.path(), now, &original, &before);
    }
    assert_eq!(failures, 27); // journal + four certificate replacements + journal removal
    assert_eq!(interrupted.len(), 2);
    for (interrupted, expected_failures) in interrupted {
        let mut failures = 0;
        for boundary in 0..64 {
            let case = crate::private_tempdir();
            restore_snapshot(case.path(), &interrupted);
            if interrupt_renewal(case.path(), now, boundary).is_ok() {
                break;
            }
            failures += 1;
            assert_recovered(case.path(), now, &original, &before);
        }
        assert_eq!(failures, expected_failures);
    }
}

#[test]
fn recovery_discards_only_safe_uncommitted_staging_data() {
    let seed = crate::private_tempdir();
    let original = prepare(seed.path()).unwrap();
    let before = snapshot(seed.path());
    let now = timestamp(materials(&original)[0].leaf.not_after()).unwrap() + DAY;
    for committed in [false, true] {
        for fragment in [
            b"".as_slice(),
            b"{\"version\":",
            b"-----BEGIN CERTIFICATE-----\npartial",
        ] {
            let case = crate::private_tempdir();
            restore_snapshot(case.path(), &before);
            if committed {
                assert!(interrupt_renewal(case.path(), now, 5).is_err());
            }
            let dir = secure::Directory::open(case.path(), false).unwrap();
            if case.path().join(RENEWAL_STAGE).exists() {
                std::fs::write(case.path().join(RENEWAL_STAGE), fragment).unwrap();
            } else {
                dir.write_new(RENEWAL_STAGE, fragment).unwrap();
            }
            assert_recovered(case.path(), now, &original, &before);
        }
    }
}

#[test]
fn recovery_rejects_journal_tampering_and_unrelated_disk_state_without_writes() {
    let seed = crate::private_tempdir();
    let identity = prepare(seed.path()).unwrap();
    let now = timestamp(materials(&identity)[0].leaf.not_after()).unwrap() + DAY;
    assert!(interrupt_renewal(seed.path(), now, 4).is_err());
    let baseline = snapshot(seed.path());
    let journal: serde_json::Value =
        serde_json::from_slice(&baseline[std::ffi::OsStr::new(RENEWAL_JOURNAL)]).unwrap();
    for change in 0..9 {
        let case = crate::private_tempdir();
        restore_snapshot(case.path(), &baseline);
        let mut corrupted = journal.clone();
        match change {
            0 => corrupted["version"] = 2.into(),
            1 => {
                corrupted["prepared_digest"] = KeyFingerprint::of_bytes(b"other identity")
                    .to_string()
                    .into()
            }
            2 => {
                corrupted["https"]["replacement_pem"] = journal["gossip"]["replacement_pem"].clone()
            }
            3 => corrupted["gossip"]["previous_pem"] = "bad PEM".into(),
            4 => corrupted["https"]["path"] = "../victim".into(),
            5 => std::fs::write(case.path().join("https-leaf.pem"), b"corrupt leaf").unwrap(),
            6 => {
                std::fs::write(case.path().join("gossip-fullchain.pem"), b"corrupt chain").unwrap()
            }
            7 => std::fs::write(case.path().join(RENEWAL_JOURNAL), b"{").unwrap(),
            8 => std::fs::write(
                case.path().join(RENEWAL_JOURNAL),
                vec![b' '; MAX_FILE_BYTES + 1],
            )
            .unwrap(),
            _ => unreachable!(),
        }
        if change < 5 {
            std::fs::write(
                case.path().join(RENEWAL_JOURNAL),
                serde_json::to_vec(&corrupted).unwrap(),
            )
            .unwrap();
        }
        let before = snapshot(case.path());
        assert!(
            ServerIdentity::renew_at(case.path(), now).is_err(),
            "case {change}"
        );
        assert_eq!(snapshot(case.path()), before);
    }
    // Even a valid journal cannot waive CA expiry, or historical validation
    // of a replacement that itself expired while the server was offline.
    let ca_end = timestamp(
        X509::from_pem(&baseline[std::ffi::OsStr::new("https-ca.pem")])
            .unwrap()
            .not_after(),
    )
    .unwrap();
    assert!(ServerIdentity::renew_at(seed.path(), ca_end).is_err());
    assert_eq!(snapshot(seed.path()), baseline);
    assert_recovered(seed.path(), now + 400 * DAY, &identity, &baseline);
}

#[test]
fn journal_and_stage_reject_symlinks_hardlinks_and_unsafe_permissions() {
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };
    let seed = crate::private_tempdir();
    let identity = prepare(seed.path()).unwrap();
    let now = timestamp(materials(&identity)[0].leaf.not_after()).unwrap() + DAY;
    assert!(interrupt_renewal(seed.path(), now, 5).is_err());
    let baseline = snapshot(seed.path());
    for name in [RENEWAL_JOURNAL, RENEWAL_STAGE] {
        for change in 0..4 {
            let case = crate::private_tempdir();
            let path = case.path().join("identity");
            std::fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            restore_snapshot(&path, &baseline);
            let target = path.join(name);
            let victim = case.path().join("victim");
            match change {
                0 => fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap(),
                1 => fs::hard_link(&target, &victim).unwrap(),
                2 => {
                    fs::rename(&target, &victim).unwrap();
                    symlink(&victim, &target).unwrap();
                }
                3 => {
                    fs::remove_file(&target).unwrap();
                    fs::create_dir(&target).unwrap();
                }
                _ => unreachable!(),
            }
            assert!(ServerIdentity::renew_at(&path, now).is_err());
            assert_immutable_files(&baseline, &path);
            assert_eq!(
                fs::read(path.join("https-leaf.pem")).unwrap(),
                baseline[std::ffi::OsStr::new("https-leaf.pem")]
            );
            if change == 2 {
                assert_eq!(
                    fs::read(&victim).unwrap(),
                    baseline[std::ffi::OsStr::new(name)]
                );
            }
        }
    }
}

#[test]
fn concurrent_renewal_uses_exclusive_inode_lock_and_one_leaf_generation() {
    let temp = crate::private_tempdir();
    let identity = prepare(temp.path()).unwrap();
    let before = snapshot(temp.path());
    let now = timestamp(materials(&identity)[0].leaf.not_after()).unwrap() + DAY;
    let dir = secure::Directory::open(temp.path(), false).unwrap();
    dir.lock().unwrap();
    let other = rustix::fs::open(
        temp.path(),
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
        rustix::fs::Mode::empty(),
    )
    .unwrap();
    assert_eq!(
        rustix::fs::flock(&other, rustix::fs::FlockOperation::NonBlockingLockExclusive)
            .unwrap_err(),
        rustix::io::Errno::WOULDBLOCK
    );
    drop(dir);
    let barrier = std::sync::Barrier::new(4);
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..4)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    ServerIdentity::renew_at(temp.path(), now).unwrap()
                })
            })
            .collect();
        for worker in workers {
            let result = worker.join().unwrap();
            assert_eq!(result.server_info(), identity.server_info());
        }
    });
    assert_recovered(temp.path(), now, &identity, &before);
}
#[test]
fn renewal_refuses_missing_or_wrong_eku_ca_leaf_and_mismatched_sans() {
    let temp = crate::private_tempdir();
    let identity = prepare(temp.path()).unwrap();
    let tls = materials(&identity);
    let names = certificate_names(&identity.prepared.config, identity.request()).unwrap();
    for (material, paths, gossip) in [
        (&tls[0], identity.tls_paths(), false),
        (&tls[1], identity.gossip_paths(), true),
    ] {
        let original = snapshot(temp.path());
        for corruption in 0..4 {
            let mut leaf = certificate(&material.leaf_key, "malformed managed leaf", 365).unwrap();
            leaf.set_not_before(material.ca.not_before()).unwrap();
            let end = Asn1Time::from_unix(current_time().unwrap() + DAY).unwrap();
            leaf.set_not_after(&end).unwrap();
            leaf.set_issuer_name(material.ca.subject_name()).unwrap();
            let mut constraints = BasicConstraints::new();
            constraints.critical();
            if corruption == 2 {
                constraints.ca();
            }
            leaf.append_extension(constraints.build().unwrap()).unwrap();
            leaf.append_extension(
                KeyUsage::new()
                    .critical()
                    .digital_signature()
                    .build()
                    .unwrap(),
            )
            .unwrap();
            if corruption != 0 {
                let mut eku = ExtendedKeyUsage::new();
                if corruption == 1 {
                    if gossip {
                        eku.server_auth();
                    } else {
                        eku.client_auth();
                    }
                } else {
                    eku.server_auth();
                    if gossip {
                        eku.client_auth();
                    }
                }
                leaf.append_extension(eku.build().unwrap()).unwrap();
            }
            let mut san = SubjectAlternativeName::new();
            san.dns(if corruption == 3 {
                "other.tako.internal"
            } else {
                &names.hostname
            });
            if gossip {
                san.dns(&names.gossip);
            }
            for ip in names.ips {
                san.ip(&ip.to_string());
            }
            let san = san
                .build(&leaf.x509v3_context(Some(&material.ca), None))
                .unwrap();
            leaf.append_extension(san).unwrap();
            let aki = AuthorityKeyIdentifier::new()
                .keyid(true)
                .build(&leaf.x509v3_context(Some(&material.ca), None))
                .unwrap();
            leaf.append_extension(aki).unwrap();
            leaf.sign(&material.ca_key, MessageDigest::sha256())
                .unwrap();
            let pem = leaf.build().to_pem().unwrap();
            std::fs::write(&paths.certificate, &pem).unwrap();
            std::fs::write(
                &paths.fullchain,
                [pem, material.ca.to_pem().unwrap()].concat(),
            )
            .unwrap();
            let before = snapshot(temp.path());
            assert!(
                ServerIdentity::renew(temp.path()).is_err(),
                "role {gossip}, corruption {corruption}"
            );
            assert_eq!(snapshot(temp.path()), before);
        }
        for path in [&paths.certificate, &paths.fullchain] {
            std::fs::write(path, &original[path.file_name().unwrap()]).unwrap();
        }
    }
}
