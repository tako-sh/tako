use super::*;
use crate::management_http::{MANAGEMENT_RPC_TIMEOUT, ManagementAuth, ManagementClient};
use tako_network::api::RPC_NAMESPACE;

fn identity() -> Identity {
    let keys = LocalDeviceKeys::generate().unwrap();
    let network_id = NetworkId::random().unwrap();
    let server_id = LocalDeviceKeys::generate()
        .unwrap()
        .device_id(&network_id, DeviceKind::Server)
        .unwrap();
    let hostname = network::client::hostname(&network_id, &server_id);
    let certificate = rcgen::generate_simple_self_signed(vec![hostname])
        .unwrap()
        .cert
        .pem();
    let info = ServerInfo {
        wireguard_endpoint: "192.0.2.10:51820".parse().unwrap(),
        ssh_host_key: keys.ssh_public_key().unwrap(),
        ssh_port: 22,
        https_port: 8443,
        https_certificate_pem: Some(certificate),
        corrosion_ca_certificate_pem: None,
        https_spki_sha256: None,
    };
    Identity {
        target: NativeTarget {
            network: crate::config::NetworkAlias {
                name: "fleet".into(),
                id: network_id.to_string(),
                device: keys
                    .device_id(&network_id, DeviceKind::Laptop)
                    .unwrap()
                    .to_string(),
            },
            server_id,
        },
        network_id,
        kind: DeviceKind::Laptop,
        keys,
        info,
        permissions: Permissions {
            deployments: ["demo/production".parse().unwrap()].into(),
            ..Default::default()
        },
    }
}

fn verify(signed: &SignedRequest, key: &ssh_key::PublicKey, method: &str, path: &str) -> bool {
    let mut transcript = b"tako-network-native-rpc-v1\0".to_vec();
    transcript.extend_from_slice(signed.network_id.as_bytes());
    transcript.extend_from_slice(signed.server_id.as_bytes());
    transcript.extend_from_slice(signed.signer.as_bytes());
    for field in [method, path] {
        transcript.extend_from_slice(&(field.len() as u32).to_be_bytes());
        transcript.extend_from_slice(field.as_bytes());
    }
    transcript.extend_from_slice(&signed.timestamp.to_be_bytes());
    transcript.extend_from_slice(signed.nonce.as_bytes());
    transcript.extend_from_slice(&Sha256::digest(
        serde_json::to_vec(&signed.request).unwrap(),
    ));
    key.verify(
        RPC_NAMESPACE,
        &transcript,
        &ssh_key::SshSig::from_pem(&signed.signature).unwrap(),
    )
    .is_ok()
}

#[test]
fn native_request_binds_exact_bytes_scope_method_and_server_without_ssh_auth() {
    let identity = identity();
    let key =
        ssh_key::PublicKey::from_openssh(identity.keys.ssh_public_key().unwrap().as_str()).unwrap();
    let body = b"{ \"command\":\"status\", \"app\":\"demo/production\" }";
    let command = Command::Status {
        app: "demo/production".into(),
    };
    let action = rpc_action(&command, body, None).unwrap();
    let headers = identity.headers(action.clone()).unwrap();
    let signed: SignedRequest =
        serde_json::from_slice(&STANDARD.decode(headers[AUTHORIZATION].as_bytes()).unwrap())
            .unwrap();
    assert_eq!(signed.server_id, identity.target.server_id);
    assert_eq!(signed.network_id, identity.network_id);
    assert_eq!(action.scope().unwrap().as_str(), "demo/production");
    assert_eq!(action.sha256(), hex::encode(Sha256::digest(body)));
    assert_ne!(
        action.sha256(),
        hex::encode(Sha256::digest(serde_json::to_vec(&command).unwrap()))
    );
    assert!(verify(&signed, &key, "POST", "/v1/network/authorize"));
    assert!(!verify(&signed, &key, "GET", "/v1/network/authorize"));
    assert!(!verify(&signed, &key, "POST", "/v1/management/rpc"));
    let mut tampered = signed.clone();
    tampered.request = RpcRequest::Authorize {
        action: rpc_action(
            &Command::Status {
                app: "demo/staging".into(),
            },
            body,
            None,
        )
        .unwrap(),
    };
    assert!(!verify(&tampered, &key, "POST", "/v1/network/authorize"));
    tampered = signed.clone();
    tampered.request = RpcRequest::Authorize {
        action: rpc_action(&command, b"changed", None).unwrap(),
    };
    assert!(!verify(&tampered, &key, "POST", "/v1/network/authorize"));

    tampered = signed.clone();
    tampered.server_id = LocalDeviceKeys::generate()
        .unwrap()
        .device_id(&signed.network_id, DeviceKind::Server)
        .unwrap();
    assert!(!verify(&tampered, &key, "POST", "/v1/network/authorize"));
    tampered = signed.clone();
    tampered.network_id = NetworkId::random().unwrap();
    assert!(!verify(&tampered, &key, "POST", "/v1/network/authorize"));

    let client = ManagementClient {
        http: identity.client(MANAGEMENT_RPC_TIMEOUT).unwrap(),
        deploy_http: identity.client(MANAGEMENT_RPC_TIMEOUT).unwrap(),
        upload_http: identity.client(MANAGEMENT_RPC_TIMEOUT).unwrap(),
        signer: ManagementAuth::Native(identity),
        preflight_scope: None,
    };
    let request = client
        .request(&client.http, "rpc", headers)
        .body(body.as_slice())
        .build()
        .unwrap();
    assert_eq!(request.method(), reqwest::Method::POST);
    assert_eq!(request.url().scheme(), "https");
    assert_eq!(
        request.url().host_str(),
        Some(network::client::hostname(&signed.network_id, &signed.server_id).as_str())
    );
    assert_eq!(request.url().port(), Some(8443));
    assert_eq!(request.url().path(), "/v1/management/rpc");
    assert_eq!(request.body().unwrap().as_bytes().unwrap(), body);
    assert!(
        !request
            .headers()
            .contains_key(crate::management_http::auth::HEADER_SIGNATURE)
    );
    assert_eq!(
        client.scoped_apps(),
        Some(vec!["demo/production".to_string()])
    );
    assert_eq!(client.log_fetch_limit(24 * 1024 * 1024), MAX_LOG_BYTES);
    assert_eq!(client.log_fetch_limit(0), 0);
}

#[test]
fn upload_and_log_descriptors_are_distinct_authorization_kinds() {
    let identity = identity();
    let app = "demo/production";
    let upload = tako_core::release_artifact_upload_auth_body(app, "v1", 42, &"a".repeat(64));
    let logs = tako_core::logs_request_auth_body(app, 7, 8, Some(123), 8192);
    for (endpoint, action, descriptor) in [
        (
            "release-artifact",
            ManagementAction::ReleaseArtifact {
                scope: Some(scope(app).unwrap()),
                sha256: hex::encode(Sha256::digest(&upload)),
            },
            upload,
        ),
        (
            "logs",
            ManagementAction::Logs {
                scope: Some(scope(app).unwrap()),
                sha256: hex::encode(Sha256::digest(&logs)),
            },
            logs,
        ),
    ] {
        let headers = identity.headers(action.clone()).unwrap();
        let signed: SignedRequest =
            serde_json::from_slice(&STANDARD.decode(headers[AUTHORIZATION].as_bytes()).unwrap())
                .unwrap();
        assert_eq!(
            signed.request,
            RpcRequest::Authorize {
                action: action.clone()
            }
        );
        assert_eq!(action.sha256(), hex::encode(Sha256::digest(descriptor)));
        assert!(
            identity
                .url(endpoint)
                .ends_with(&format!("/v1/management/{endpoint}"))
        );
    }
}

#[test]
fn scoped_preflight_and_application_scope_are_exact() {
    let app = scope("demo/production").unwrap();
    let command = Command::CheckDeploySpace { min_free_bytes: 10 };
    assert_eq!(
        rpc_action(&command, b"body", Some(&app)).unwrap().scope(),
        Some(&app)
    );
    assert!(
        rpc_action(&Command::ServerInfo, b"body", Some(&app))
            .unwrap()
            .scope()
            .is_none()
    );
    for invalid in [
        "demo",
        "demo/",
        "/production",
        "demo/production/extra",
        "demo/../production",
    ] {
        assert!(
            rpc_action(
                &Command::Status {
                    app: invalid.into()
                },
                b"body",
                None
            )
            .is_err()
        );
    }
}

#[test]
fn drain_and_resume_never_inherit_application_scope() {
    let app = scope("demo/production").unwrap();
    for command in [
        Command::DrainServer {
            timeout_seconds: 30,
        },
        Command::ResumeServer,
    ] {
        let body = serde_json::to_vec(&command).unwrap();
        let action = rpc_action(&command, &body, Some(&app)).unwrap();
        assert!(action.scope().is_none());
        assert_eq!(action.sha256(), hex::encode(Sha256::digest(&body)));
    }
}

#[test]
fn missing_or_mismatched_pins_fail_before_connecting() {
    let mut identity = identity();
    identity.info.https_spki_sha256 = Some(tako_network::KeyFingerprint::of_bytes(b"wrong"));
    assert!(
        identity
            .client(MANAGEMENT_RPC_TIMEOUT)
            .err()
            .unwrap()
            .to_string()
            .contains("SPKI")
    );
    identity.info.https_spki_sha256 = None;
    identity.info.https_certificate_pem = None;
    assert!(
        identity
            .client(MANAGEMENT_RPC_TIMEOUT)
            .err()
            .unwrap()
            .to_string()
            .contains("pinned HTTPS")
    );
}
