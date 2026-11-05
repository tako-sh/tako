use super::*;
use crate::tls::{CertManager, CertManagerConfig};
use std::collections::HashMap;
use tako_network::{DeviceKind, LocalDeviceKeys, NetworkId};
use tempfile::TempDir;

fn state() -> (TempDir, Arc<ServerState>) {
    std::thread::spawn(|| {
        let dir = tempfile::tempdir().unwrap();
        let certs = Arc::new(CertManager::new(CertManagerConfig {
            cert_dir: dir.path().join("certs"),
            ..Default::default()
        }));
        let state = Arc::new(
            ServerState::new_with_runtime(
                dir.path().to_path_buf(),
                certs,
                None,
                Arc::new(parking_lot::RwLock::new(HashMap::new())),
                crate::ServerRuntimeConfig::for_defaults(dir.path().to_path_buf()),
            )
            .unwrap(),
        );
        (dir, state)
    })
    .join()
    .expect("state fixture thread")
}

fn signed(action: ManagementAction) -> SignedRequest {
    let keys = LocalDeviceKeys::generate().unwrap();
    let network = NetworkId::random().unwrap();
    let server = keys.device_id(&network, DeviceKind::Server).unwrap();
    SignedRequest::sign(
        &keys,
        network,
        server,
        DeviceKind::Laptop,
        10,
        RpcRequest::Authorize { action },
    )
    .unwrap()
}
fn rpc(body: &[u8], scope: Option<&str>) -> SignedRequest {
    signed(ManagementAction::Rpc {
        scope: scope.map(|s| s.parse().unwrap()),
        sha256: hex::encode(Sha256::digest(body)),
    })
}
fn request(path: &str, signed: &SignedRequest, body: Vec<u8>) -> Request<Full<Bytes>> {
    Request::post(path)
        .header(
            AUTH_HEADER,
            STANDARD.encode(serde_json::to_vec(signed).unwrap()),
        )
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}
fn authorized(signed: &SignedRequest, permissions: Permissions) -> RpcResponse {
    let RpcRequest::Authorize { action } = &signed.request else {
        unreachable!()
    };
    RpcResponse::Authorized {
        signer: signed.signer.clone(),
        permissions,
        action: action.clone(),
    }
}
fn scoped() -> Permissions {
    Permissions {
        deployments: ["demo/production".parse().unwrap()].into_iter().collect(),
        ..Default::default()
    }
}

#[tokio::test]
async fn exact_body_action_and_audience_are_required_before_dispatch() {
    let (_dir, state) = state();
    let body = serde_json::to_vec(&Command::Status {
        app: "demo/production".into(),
    })
    .unwrap();
    let signed = rpc(&body, Some("demo/production"));
    let altered = serde_json::to_vec(&Command::Status {
        app: "other/production".into(),
    })
    .unwrap();
    let response = handle_using(
        request("/v1/management/rpc", &signed, altered),
        state.clone(),
        |_| async { panic!("mismatched body reached agent") },
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = handle_using(
        request("/v1/management/logs", &signed, body.clone()),
        state.clone(),
        |_| async { panic!("wrong action reached agent") },
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let mut wrong = authorized(&signed, scoped());
    if let RpcResponse::Authorized { signer, .. } = &mut wrong {
        *signer = signed.server_id.clone();
    }
    let response = handle_using(
        request("/v1/management/rpc", &signed, body),
        state,
        |_| async { Ok(wrong) },
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn server_admin_still_requires_exact_application_scope() {
    let (_dir, state) = state();
    let body = serde_json::to_vec(&Command::Status {
        app: "demo/production".into(),
    })
    .unwrap();
    for scope in [None, Some("other/production")] {
        let signed = rpc(&body, scope);
        let response = authorized(
            &signed,
            Permissions {
                server_admin: true,
                ..Default::default()
            },
        );
        let response = handle_using(
            request("/v1/management/rpc", &signed, body.clone()),
            state.clone(),
            |_| async { Ok(response) },
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
}

#[tokio::test]
async fn unready_agent_denies_even_probes_and_queries_are_not_authentication() {
    let (_dir, state) = state();
    let body = serde_json::to_vec(&Command::ServerInfo).unwrap();
    let signed = rpc(&body, None);
    let response = handle_using(
        request("/v1/management/rpc", &signed, body.clone()),
        state.clone(),
        |_| async { Err(StatusCode::SERVICE_UNAVAILABLE) },
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let response = handle_using(
        request("/v1/management/rpc?token=secret", &signed, body),
        state,
        |_| async { panic!("query reached agent") },
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn duplicate_authorization_and_body_quota_are_rejected() {
    let (_dir, state) = state();
    let body = serde_json::to_vec(&Command::ServerInfo).unwrap();
    let signed = rpc(&body, None);
    let mut req = request("/v1/management/rpc", &signed, body);
    let value = req.headers()[AUTH_HEADER].clone();
    req.headers_mut().append(AUTH_HEADER, value);
    assert_eq!(
        handle_using(req, state, |_| async { panic!("duplicate reached agent") })
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        read_body(
            Full::new(Bytes::from(vec![0; MAX_RPC_BODY + 1])),
            MAX_RPC_BODY
        )
        .await
        .unwrap_err(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
}

#[tokio::test]
async fn duplex_http_transport_dispatches_authenticated_probe() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (_dir, state) = state();
    let body = serde_json::to_vec(&Command::Hello {
        protocol_version: tako_core::PROTOCOL_VERSION,
    })
    .unwrap();
    let signed = rpc(&body, None);
    let response = authorized(&signed, Permissions::default());
    let expected = signed.clone();
    let (mut client, server) = tokio::io::duplex(16 * 1024);
    let serve = tokio::spawn(async move {
        hyper::server::conn::http1::Builder::new()
            .keep_alive(false)
            .serve_connection(
                hyper_util::rt::TokioIo::new(server),
                hyper::service::service_fn(move |req| {
                    let state = state.clone();
                    let response = response.clone();
                    let expected = expected.clone();
                    async move {
                        Ok::<_, std::convert::Infallible>(
                            handle_using(req, state, |bytes| async move {
                                assert_eq!(SignedRequest::from_slice(&bytes).unwrap(), expected);
                                Ok(response)
                            })
                            .await,
                        )
                    }
                }),
            )
            .await
            .unwrap();
    });
    let headers = format!(
        "POST /v1/management/rpc HTTP/1.1\r\nHost: reserved-native.invalid\r\n{AUTH_HEADER}: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        STANDARD.encode(serde_json::to_vec(&signed).unwrap()),
        body.len()
    );
    client.write_all(headers.as_bytes()).await.unwrap();
    for chunk in body.chunks(3) {
        client.write_all(chunk).await.unwrap();
    }
    let mut bytes = Vec::new();
    client.read_to_end(&mut bytes).await.unwrap();
    serve.await.unwrap();
    let wire = String::from_utf8(bytes).unwrap();
    assert!(wire.starts_with("HTTP/1.1 200"), "{wire}");
    assert!(wire.contains("protocol_version"));
    assert!(!wire.contains(&signed.signature));
}

#[tokio::test]
async fn log_descriptor_matches_scope_and_reads_existing_format() {
    let (dir, state) = state();
    let log_dir = dir.path().join("apps/demo/production/logs");
    std::fs::create_dir_all(&log_dir).unwrap();
    std::fs::write(log_dir.join("current.log"), b"test log\n").unwrap();
    let descriptor = tako_core::logs_request_auth_body("demo/production", 0, 0, None, 100);
    let signed = signed(ManagementAction::Logs {
        scope: Some("demo/production".parse().unwrap()),
        sha256: hex::encode(Sha256::digest(&descriptor)),
    });
    let response = authorized(&signed, scoped());
    let req = Request::post("/v1/management/logs")
        .header(
            AUTH_HEADER,
            STANDARD.encode(serde_json::to_vec(&signed).unwrap()),
        )
        .header("x-tako-app", "demo/production")
        .header("x-tako-log-previous-offset", "0")
        .header("x-tako-log-current-offset", "0")
        .header("x-tako-log-max-bytes", "100")
        .body(Full::new(Bytes::new()))
        .unwrap();
    let response = handle_using(req, state, |_| async { Ok(response) }).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-tako-log-current-offset"], "9");
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "test log\n"
    );
}

#[tokio::test]
async fn upload_verifies_digest_and_confines_extraction() {
    let (dir, state) = state();
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(5);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(&mut header, "hello.txt", &b"hello"[..])
        .unwrap();
    let archive = zstd::stream::encode_all(&tar.into_inner().unwrap()[..], 0).unwrap();
    for bad_digest in [true, false] {
        let digest = if bad_digest {
            "00".repeat(32)
        } else {
            hex::encode(Sha256::digest(&archive))
        };
        let descriptor = tako_core::release_artifact_upload_auth_body(
            "demo/production",
            "v1",
            archive.len() as u64,
            &digest,
        );
        let signed = signed(ManagementAction::ReleaseArtifact {
            scope: Some("demo/production".parse().unwrap()),
            sha256: hex::encode(Sha256::digest(&descriptor)),
        });
        let response = authorized(&signed, scoped());
        let req = Request::post("/v1/management/release-artifact")
            .header(
                AUTH_HEADER,
                STANDARD.encode(serde_json::to_vec(&signed).unwrap()),
            )
            .header("x-tako-app", "demo/production")
            .header("x-tako-version", "v1")
            .header("x-tako-artifact-size", archive.len())
            .header("x-tako-artifact-sha256", digest)
            .body(Full::new(Bytes::from(archive.clone())))
            .unwrap();
        let response = handle_using(req, state.clone(), |_| async { Ok(response) }).await;
        assert_eq!(
            response.status(),
            if bad_digest {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::OK
            }
        );
    }
    assert_eq!(
        std::fs::read(
            dir.path()
                .join("apps/demo/production/releases/v1/hello.txt")
        )
        .unwrap(),
        b"hello"
    );
    assert_eq!(
        std::fs::read_dir(dir.path().join("tmp/uploads"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn policy_reads_bindings_after_acquiring_the_dispatch_guard() {
    let (dir, state) = state();
    let release = dir.path().join("apps/demo/production/releases/v1");
    std::fs::create_dir_all(&release).unwrap();
    let config = crate::instances::AppConfig {
        name: "demo".into(),
        environment: "production".into(),
        version: "v1".into(),
        ..Default::default()
    };
    state
        .state_store
        .upsert_app(&config, &["old.example".into()])
        .unwrap();
    state
        .state_store
        .set_ssl("demo/production", &Default::default())
        .unwrap();
    let command = Command::PrepareDeploy {
        app: "demo/production".into(),
        version: "v1".into(),
        path: release.to_str().unwrap().into(),
        routes: vec!["old.example".into()],
        ssl: Default::default(),
        force: false,
    };
    let body = serde_json::to_vec(&command).unwrap();
    let signed = rpc(&body, Some("demo/production"));
    let authorized = authorized(&signed, scoped());
    let guard = state.management_lock.lock().await;
    let (tx, rx) = tokio::sync::oneshot::channel();
    let pending = tokio::spawn(handle_using(
        request("/v1/management/rpc", &signed, body),
        state.clone(),
        move |_| async {
            tx.send(()).unwrap();
            Ok(authorized)
        },
    ));
    rx.await.unwrap();
    state
        .state_store
        .upsert_app(&config, &["new.example".into()])
        .unwrap();
    drop(guard);
    let response = pending.await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&body).contains("provisioned routes"));
}

#[test]
fn since_log_reader_skips_large_old_lines_without_unbounded_allocation() {
    let (dir, state) = state();
    let logs = dir.path().join("apps/demo/production/logs");
    std::fs::create_dir_all(&logs).unwrap();
    let mut previous = b"2020-01-01T00:00:00 ".to_vec();
    previous.extend(vec![b'x'; 128 * 1024]);
    previous.extend(b"\n2026-01-01T00:00:00 recent\n");
    std::fs::write(logs.join("previous.log"), previous).unwrap();
    std::fs::write(logs.join("current.log"), b"unstructured\n").unwrap();
    let metadata = Logs {
        app: "demo/production".into(),
        previous: 0,
        current: 0,
        since: Some(1_735_689_600),
        max_bytes: 100,
    };
    let result = super::logs::read(&state, &metadata).unwrap();
    assert_eq!(result.bytes, b"2026-01-01T00:00:00 recent\nunstructured\n");
    assert!(!result.truncated);
    std::fs::write(logs.join("current.log"), vec![b'x'; 128 * 1024]).unwrap();
    let result = super::logs::read(&state, &metadata).unwrap();
    assert!(result.truncated);
    assert!(result.bytes.len() <= metadata.max_bytes);
}

#[test]
fn log_symlink_and_redirected_upload_roots_are_denied() {
    let (dir, _state) = state();
    let logs = dir.path().join("apps/demo/production/logs");
    std::fs::create_dir_all(&logs).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", logs.join("current.log")).unwrap();
    assert!(upload::confined_logs(dir.path(), "demo/production").is_err());
    std::os::unix::fs::symlink("/tmp", dir.path().join("tmp")).unwrap();
    assert!(upload::confined_directory(&dir.path().join("tmp/uploads")).is_err());
}

struct QueueBody(tokio::sync::mpsc::Receiver<Bytes>);
impl Body for QueueBody {
    type Data = Bytes;
    type Error = std::io::Error;
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, Self::Error>>> {
        self.0
            .poll_recv(cx)
            .map(|frame| frame.map(|bytes| Ok(hyper::body::Frame::data(bytes))))
    }
}

#[tokio::test]
async fn disconnected_stream_removes_partial_upload_without_extraction() {
    let (dir, state) = state();
    let digest = hex::encode(Sha256::digest(b"complete"));
    let descriptor =
        tako_core::release_artifact_upload_auth_body("demo/production", "v1", 8, &digest);
    let signed = signed(ManagementAction::ReleaseArtifact {
        scope: Some("demo/production".parse().unwrap()),
        sha256: hex::encode(Sha256::digest(&descriptor)),
    });
    let response = authorized(&signed, scoped());
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let req = Request::post("/v1/management/release-artifact")
        .header(
            AUTH_HEADER,
            STANDARD.encode(serde_json::to_vec(&signed).unwrap()),
        )
        .header("x-tako-app", "demo/production")
        .header("x-tako-version", "v1")
        .header("x-tako-artifact-size", "8")
        .header("x-tako-artifact-sha256", digest)
        .body(QueueBody(rx))
        .unwrap();
    let task = tokio::spawn(handle_using(req, state, |_| async { Ok(response) }));
    tx.send(Bytes::from_static(b"com")).await.unwrap();
    let uploads = dir.path().join("tmp/uploads");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if uploads.is_dir() && std::fs::read_dir(&uploads).unwrap().count() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(std::fs::read_dir(&uploads).unwrap().count(), 0);
    assert!(!dir.path().join("apps/demo/production/releases/v1").exists());
}

#[tokio::test]
async fn prepare_upload_rpc_cannot_follow_an_existing_shared_log_redirect() {
    let (dir, state) = state();
    let shared = dir.path().join("apps/demo/production/shared");
    std::fs::create_dir_all(&shared).unwrap();
    std::os::unix::fs::symlink("/tmp", shared.join("logs")).unwrap();
    let command = Command::PrepareReleaseUpload {
        app: "demo/production".into(),
        version: "v1".into(),
    };
    let body = serde_json::to_vec(&command).unwrap();
    let signed = rpc(&body, Some("demo/production"));
    let response = authorized(&signed, scoped());
    let response = handle_using(
        request("/v1/management/rpc", &signed, body),
        state,
        |_| async { Ok(response) },
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}
