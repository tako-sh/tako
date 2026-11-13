use super::*;
use futures_util::StreamExt;
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
async fn progressing_upload_can_finish_after_five_minutes() {
    let (dir, state) = state();
    let (request, signed) = upload_request();
    let grant = authorized(&signed, scoped());
    let (parts, archive) = request.into_parts();
    let archive = archive.collect().await.unwrap().to_bytes();
    // Twelve 25-second gaps plus two seconds exceed the old five-minute deadline.
    let frames = futures_util::stream::iter(0..13).then(move |i| {
        let bytes = archive.slice(archive.len() * i / 13..archive.len() * (i + 1) / 13);
        async move {
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(if i == 12 { 2 } else { 25 })).await;
            tokio::time::resume();
            Ok::<_, std::io::Error>(hyper::body::Frame::data(bytes))
        }
    });
    let body = http_body_util::StreamBody::new(Box::pin(frames));
    let response = handle_using(Request::from_parts(parts, body), state, |_| async {
        Ok(grant)
    })
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        std::fs::read(
            dir.path()
                .join("apps/demo/production/releases/v1/hello.txt")
        )
        .unwrap(),
        b"hello"
    );
}

#[tokio::test]
async fn stalled_upload_reports_timeout_and_cleans_up() {
    let (dir, state) = state();
    let (request, signed) = upload_request();
    let grant = authorized(&signed, scoped());
    let size = request.headers()["x-tako-artifact-size"]
        .to_str()
        .unwrap()
        .to_owned();
    let (started, reading_body) = tokio::sync::oneshot::channel();
    let frames = futures_util::stream::once(async {
        started.send(()).unwrap();
        std::future::pending::<std::io::Result<hyper::body::Frame<Bytes>>>().await
    });
    let response = tokio::spawn(handle_using(
        request.map(|_| http_body_util::StreamBody::new(Box::pin(frames))),
        state,
        |_| async { Ok(grant) },
    ));
    reading_body.await.unwrap();
    tokio::time::pause();
    tokio::time::advance(BODY_TIMEOUT + Duration::from_secs(1)).await;
    tokio::time::resume();
    let response = tokio::time::timeout(Duration::from_secs(2), response)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let result: tako_core::Response = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        result.error_message().unwrap(),
        format!("Artifact upload stalled (0 of {size} bytes received)")
    );
    assert_eq!(
        std::fs::read_dir(dir.path().join("tmp/uploads"))
            .unwrap()
            .count(),
        0
    );
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
async fn logs_authenticate_without_queueing_and_return_no_bytes_when_revoked() {
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
    let callback_checked = checked.clone();
    // Log reads are inspection: they are authorized with current authority and
    // answered at once instead of queueing behind lifecycle work.
    let _lock = state.management_lock.lock().await;
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        handle_using(request, state.clone(), move |_| async move {
            callback_checked.store(true, Ordering::Release);
            Err(StatusCode::UNAUTHORIZED)
        }),
    )
    .await
    .expect("log reads must not queue behind the dispatch lock");
    assert!(checked.load(Ordering::Acquire));
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
