use tako_network::*;

fn request(keys: &LocalDeviceKeys) -> EnrollmentRequest {
    keys.enrollment_request(
        Some(NetworkId::random().unwrap()),
        DeviceKind::Laptop,
        "laptop".parse().unwrap(),
    )
    .unwrap()
}

#[test]
fn signed_request_binds_network_kind_keys_and_name() {
    let keys = LocalDeviceKeys::generate().unwrap();
    let original = request(&keys);
    original.verify().unwrap();
    let mut changed = original.clone();
    changed.name = "different".parse().unwrap();
    assert!(changed.verify().is_err());
    let mut changed = original.clone();
    changed.network_id = Some(NetworkId::random().unwrap());
    assert!(changed.verify().is_err());
    let mut changed = original.clone();
    changed.kind = DeviceKind::Server;
    assert!(changed.verify().is_err());
    let mut changed = original.clone();
    changed.request_id = RequestId::random().unwrap();
    assert!(changed.verify().is_err());
    let mut changed = original.clone();
    changed.expires_at = Some(100);
    assert!(changed.verify().is_err());
    let mut changed = original;
    changed.wg_public_key = LocalDeviceKeys::generate()
        .unwrap()
        .wg_public_key()
        .unwrap();
    assert!(changed.verify().is_err());
}

#[test]
fn identity_ignores_labels_but_separates_network_and_kind() {
    let keys = LocalDeviceKeys::generate().unwrap();
    let network = NetworkId::random().unwrap();
    let first = keys
        .enrollment_request(
            Some(network.clone()),
            DeviceKind::Laptop,
            "first".parse().unwrap(),
        )
        .unwrap();
    let second = keys
        .enrollment_request(
            Some(network.clone()),
            DeviceKind::Laptop,
            "second".parse().unwrap(),
        )
        .unwrap();
    assert_eq!(first.device_id, second.device_id);
    assert_ne!(
        keys.device_id(&network, DeviceKind::Laptop).unwrap(),
        keys.device_id(&network, DeviceKind::Server).unwrap()
    );
    assert_ne!(
        keys.device_id(&network, DeviceKind::Laptop).unwrap(),
        keys.device_id(&NetworkId::random().unwrap(), DeviceKind::Laptop)
            .unwrap()
    );
    let unbound = keys
        .enrollment_request(None, DeviceKind::Laptop, "offline".parse().unwrap())
        .unwrap();
    unbound.verify().unwrap();
    assert_eq!(
        unbound.device_id_for(&network).unwrap(),
        first.device_id.unwrap()
    );
}

#[test]
fn wireguard_proof_requires_both_actual_private_key_and_exact_challenge() {
    let keys = LocalDeviceKeys::generate().unwrap();
    let request = request(&keys);
    let network = request.network_id.clone().unwrap();
    let (challenge, pending) = WgChallenge::begin(&request, network.clone(), 10, 40).unwrap();
    let proof = keys.answer_challenge(&challenge, &request, 11).unwrap();
    pending.verify(&proof, 12).unwrap();
    let (other, pending) = WgChallenge::begin(&request, network.clone(), 10, 40).unwrap();
    assert_ne!(challenge.nonce, other.nonce);
    assert!(pending.verify(&proof, 12).is_err());
    let (_, pending) = WgChallenge::begin(&request, network, 10, 40).unwrap();
    assert!(pending.verify(&proof, 40).is_err());
    assert!(
        LocalDeviceKeys::generate()
            .unwrap()
            .answer_challenge(&challenge, &request, 11)
            .is_err()
    );
}

#[test]
fn copied_wg_public_key_with_valid_ssh_proof_does_not_prove_possession() {
    let victim = LocalDeviceKeys::generate().unwrap();
    let attacker = LocalDeviceKeys::generate().unwrap();
    let mut request = request(&attacker);
    request.wg_public_key = victim.wg_public_key().unwrap();
    request.device_id = Some(
        DeviceId::from_keys(
            request.network_id.as_ref().unwrap(),
            request.kind,
            &request.ssh_public_key,
            &request.wg_public_key,
        )
        .unwrap(),
    );
    request.proof = attacker
        .sign(ENROLLMENT_NAMESPACE, &request.canonical_bytes().unwrap())
        .unwrap()
        .to_pem(ssh_key::LineEnding::LF)
        .unwrap();
    request.verify().unwrap();
    let (challenge, pending) =
        WgChallenge::begin(&request, request.network_id.clone().unwrap(), 10, 20).unwrap();
    assert!(attacker.answer_challenge(&challenge, &request, 11).is_err());
    let forged = WgProof {
        response: KeyFingerprint::of_bytes(b"forged"),
    };
    assert!(pending.verify(&forged, 11).is_err());
}

#[test]
fn strict_parsing_rejects_ambiguous_keys_ids_commands_and_unknown_fields() {
    assert!("A".repeat(32).parse::<NetworkId>().is_err());
    assert!("00".parse::<DeviceId>().is_err());
    assert!(WgPublicKey::from_bytes([0; 32]).is_err());
    let mut low_order = [0; 32];
    low_order[0] = 1;
    assert!(WgPublicKey::from_bytes(low_order).is_err());
    assert!(WgPublicKey::from_bytes([255; 32]).is_err());
    for name in ["--command", "user;id", "user\nroot", "$(id)"] {
        assert!(name.parse::<SshUser>().is_err());
    }
    for scope in ["app", "app/../prod", "App/prod", "app/*", "app/prod/extra"] {
        assert!(scope.parse::<AppEnv>().is_err());
    }
    let keys = LocalDeviceKeys::generate().unwrap();
    assert!(
        format!("{} comment", keys.ssh_public_key().unwrap())
            .parse::<SshPublicKey>()
            .is_err()
    );
    let mut value = serde_json::to_value(request(&keys)).unwrap();
    value["admin"] = serde_json::json!(true);
    assert!(serde_json::from_value::<EnrollmentRequest>(value).is_err());
}

#[test]
fn address_is_stable_private_and_network_specific() {
    let keys = LocalDeviceKeys::generate().unwrap();
    let wg = keys.wg_public_key().unwrap();
    let network = NetworkId::random().unwrap();
    let address = network.address(&wg);
    assert_eq!(address.octets()[0], 0xfd);
    assert_eq!(network.address(&wg), address);
    assert_ne!(NetworkId::random().unwrap().address(&wg), address);
    let other = LocalDeviceKeys::generate()
        .unwrap()
        .wg_public_key()
        .unwrap();
    assert_eq!(
        &network.address(&other).octets()[..6],
        &address.octets()[..6]
    );
    assert_ne!(network.address(&other), address);
}

#[cfg(unix)]
#[test]
fn key_storage_is_private_exclusive_and_roundtrips_without_public_secret_leak() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("device.keys");
    let keys = LocalDeviceKeys::generate().unwrap();
    keys.save_new(&path).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let restored = LocalDeviceKeys::load(&path).unwrap();
    assert_eq!(
        keys.ssh_public_key().unwrap(),
        restored.ssh_public_key().unwrap()
    );
    assert_eq!(
        keys.wg_public_key().unwrap(),
        restored.wg_public_key().unwrap()
    );
    assert!(
        LocalDeviceKeys::generate()
            .unwrap()
            .save_new(&path)
            .is_err()
    );
    assert_eq!(
        keys.wg_public_key().unwrap(),
        LocalDeviceKeys::load(&path)
            .unwrap()
            .wg_public_key()
            .unwrap()
    );
    let encoded = serde_json::to_string(&request(&keys)).unwrap();
    assert!(!encoded.contains(keys.wireguard_private_key().unwrap().as_str()));
    assert!(!encoded.contains("PRIVATE KEY"));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(LocalDeviceKeys::load(&path).is_err());
}

#[cfg(unix)]
#[test]
fn key_storage_rejects_symlink_components_and_hardlinks() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let keys = LocalDeviceKeys::generate().unwrap();
    let original = dir.path().join("original");
    keys.save_new(&original).unwrap();
    let link = dir.path().join("symlink");
    symlink(&original, &link).unwrap();
    assert!(keys.save_new(&link).is_err());
    assert!(LocalDeviceKeys::load(&link).is_err());
    let parent_link = dir.path().join("parent-link");
    symlink(dir.path(), &parent_link).unwrap();
    assert!(keys.save_new(&parent_link.join("new")).is_err());
    assert!(LocalDeviceKeys::load(&parent_link.join("original")).is_err());
    std::fs::hard_link(&original, dir.path().join("hardlink")).unwrap();
    assert!(LocalDeviceKeys::load(&original).is_err());
    assert!(!dir.path().join("new").exists());
}
