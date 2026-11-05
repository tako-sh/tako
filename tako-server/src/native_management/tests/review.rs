use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

fn upload_request() -> (Request<Full<Bytes>>, SignedRequest) {
    let mut archive = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(5);
    header.set_mode(0o644);
    header.set_cksum();
    archive
        .append_data(&mut header, "hello.txt", &b"hello"[..])
        .unwrap();
    let archive = zstd::stream::encode_all(&archive.into_inner().unwrap()[..], 0).unwrap();
    let digest = hex::encode(Sha256::digest(&archive));
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
    let request = Request::post("/v1/management/release-artifact")
        .header(
            AUTH_HEADER,
            STANDARD.encode(serde_json::to_vec(&signed).unwrap()),
        )
        .header("x-tako-app", "demo/production")
        .header("x-tako-version", "v1")
        .header("x-tako-artifact-size", archive.len())
        .header("x-tako-artifact-sha256", digest)
        .body(Full::new(Bytes::from(archive)))
        .unwrap();
    (request, signed)
}

#[tokio::test]
async fn drain_arriving_during_upload_authority_recheck_prevents_publication() {
    let (dir, state) = state();
    let (request, signed) = upload_request();
    let early = authorized(&signed, scoped());
    let current = early.clone();
    let (entered, rechecking) = tokio::sync::oneshot::channel();
    let (finish, resume) = tokio::sync::oneshot::channel();
    let pending = tokio::spawn(handle_with_authority(
        request,
        state.clone(),
        move |_| async move { Ok(early) },
        move |_| async move {
            entered.send(()).unwrap();
            resume.await.unwrap();
            Ok(current)
        },
        None,
    ));
    rechecking.await.unwrap();
    assert!(state.management_lock.try_lock().is_err());
    // The authenticated pre-lock drain hook publishes this gate while recheck awaits IPC.
    state.drain_flag().store(true, Ordering::Release);
    finish.send(()).unwrap();
    assert_eq!(
        pending.await.unwrap().status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert!(!dir.path().join("apps/demo/production/releases/v1").exists());
    assert_eq!(
        std::fs::read_dir(dir.path().join("tmp/uploads"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn disconnected_upload_retains_guard_and_temporary_file_through_authority_recheck() {
    let (dir, state) = state();
    let (request, signed) = upload_request();
    let early = authorized(&signed, scoped());
    let (entered, rechecking) = tokio::sync::oneshot::channel();
    let (finish, resume) = tokio::sync::oneshot::channel();
    let pending = tokio::spawn(handle_with_authority(
        request,
        state.clone(),
        move |_| async move { Ok(early) },
        move |_| async move {
            entered.send(()).unwrap();
            resume.await.unwrap();
            Err(StatusCode::UNAUTHORIZED)
        },
        None,
    ));
    rechecking.await.unwrap();
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    assert!(state.management_lock.try_lock().is_err());
    assert_eq!(
        std::fs::read_dir(dir.path().join("tmp/uploads"))
            .unwrap()
            .count(),
        1
    );
    finish.send(()).unwrap();
    let _guard = tokio::time::timeout(Duration::from_secs(2), state.management_lock.lock())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while std::fs::read_dir(dir.path().join("tmp/uploads"))
            .unwrap()
            .count()
            != 0
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!dir.path().join("apps/demo/production/releases/v1").exists());
    assert_eq!(
        std::fs::read_dir(dir.path().join("tmp/uploads"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn queued_logs_authenticate_after_lock_and_return_no_bytes_when_revoked() {
    let (dir, state) = state();
    let logs = dir.path().join("apps/demo/production/logs");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(logs.join("current.log"), b"private app log\n").unwrap();
    let descriptor = tako_core::logs_request_auth_body("demo/production", 0, 0, None, 1024);
    let signed = signed(ManagementAction::Logs {
        scope: Some("demo/production".parse().unwrap()),
        sha256: hex::encode(Sha256::digest(&descriptor)),
    });
    let request = Request::post("/v1/management/logs")
        .header(
            AUTH_HEADER,
            STANDARD.encode(serde_json::to_vec(&signed).unwrap()),
        )
        .header("x-tako-app", "demo/production")
        .header("x-tako-log-previous-offset", "0")
        .header("x-tako-log-current-offset", "0")
        .header("x-tako-log-max-bytes", "1024")
        .body(Full::new(Bytes::new()))
        .unwrap();
    let checked = Arc::new(AtomicBool::new(false));
    let revoked = Arc::new(AtomicBool::new(false));
    let callback_checked = checked.clone();
    let callback_revoked = revoked.clone();
    let lock = state.management_lock.lock().await;
    let pending = tokio::spawn(handle_using(request, state.clone(), move |_| async move {
        callback_checked.store(true, Ordering::Release);
        assert!(callback_revoked.load(Ordering::Acquire));
        Err(StatusCode::UNAUTHORIZED)
    }));
    tokio::task::yield_now().await;
    assert!(!checked.load(Ordering::Acquire));
    revoked.store(true, Ordering::Release);
    drop(lock);
    let response = pending.await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(!String::from_utf8_lossy(&body).contains("private app log"));
}

#[tokio::test]
async fn scoped_caller_cannot_set_prelock_drain_gate() {
    let (_dir, state) = state();
    let body = serde_json::to_vec(&Command::DrainServer { timeout_seconds: 1 }).unwrap();
    let signed = rpc(&body, None);
    let response = authorized(&signed, scoped());
    let _lock = state.management_lock.lock().await;
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        handle_using(
            request("/v1/management/rpc", &signed, body),
            state.clone(),
            move |_| async move { Ok(response) },
        ),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(!state.is_draining());
    assert!(!state.state_store.drain_requested().unwrap());
}
