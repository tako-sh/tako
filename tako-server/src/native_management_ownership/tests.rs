use super::*;
use std::os::unix::fs::PermissionsExt;

fn directory(path: &Path) -> Directory {
    Directory::open(path, true).unwrap()
}

fn temporary_directory() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    temp
}

async fn claim(path: &Path) -> io::Result<PrimaryHandoff> {
    PrimaryHandoff::claim_in(directory(path), Duration::ZERO).await
}

#[tokio::test]
async fn native_ownership_handoff_waits_for_all_admitted_work_and_fences_old_listener() {
    let temp = temporary_directory();
    let old = claim(temp.path()).await.unwrap().finish();
    let first = old.admit().unwrap();
    let second = old.admit().unwrap();
    assert!(claim(temp.path()).await.is_err());
    drop(first);
    assert!(claim(temp.path()).await.is_err());
    drop(second);

    let handoff = claim(temp.path()).await.unwrap();
    let new = handoff.generation();
    assert_eq!(old.admit().unwrap_err().kind(), io::ErrorKind::WouldBlock);
    assert_eq!(new.admit().unwrap_err().kind(), io::ErrorKind::WouldBlock);
    handoff.finish();
    assert_eq!(
        old.admit().unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert!(new.admit().is_ok());
}

#[tokio::test]
async fn native_ownership_failed_publication_does_not_reactivate_old_generation() {
    let temp = temporary_directory();
    let old = claim(temp.path()).await.unwrap().finish();
    drop(claim(temp.path()).await.unwrap());
    assert_eq!(
        old.admit().unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
}

#[tokio::test]
async fn native_ownership_corrupt_or_missing_generation_denies_admission() {
    let temp = temporary_directory();
    let owner = claim(temp.path()).await.unwrap().finish();
    let writer = directory(temp.path());
    writer.write(GENERATION_FILE, b"not json").unwrap();
    assert!(owner.admit().is_err());
    std::fs::remove_file(temp.path().join(GENERATION_FILE)).unwrap();
    assert!(owner.admit().is_err());
}

#[tokio::test]
async fn native_ownership_never_recreates_a_missing_live_lock() {
    let temp = temporary_directory();
    let owner = claim(temp.path()).await.unwrap().finish();
    let _active = owner.admit().unwrap();
    std::fs::remove_file(temp.path().join(ACTIVITY_FILE)).unwrap();
    assert!(owner.admit().is_err());
    assert!(claim(temp.path()).await.is_err());
    assert!(!temp.path().join(ACTIVITY_FILE).exists());
}

#[tokio::test]
async fn native_ownership_detached_rpc_keeps_lease_after_http_waiter_is_aborted() {
    let temp = temporary_directory();
    let owner = claim(temp.path()).await.unwrap().finish();
    let guard = owner.admit().unwrap();
    let (release, wait) = tokio::sync::oneshot::channel();
    let (started, ready) = tokio::sync::oneshot::channel();
    let (finished, done) = tokio::sync::oneshot::channel();
    let waiter = tokio::spawn(async move {
        tokio::spawn(async move {
            let _guard = guard;
            started.send(()).unwrap();
            let _ = wait.await;
            drop(_guard);
            let _ = finished.send(());
        })
        .await
    });
    ready.await.unwrap();
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    assert!(claim(temp.path()).await.is_err());
    release.send(()).unwrap();
    done.await.unwrap();
    assert!(claim(temp.path()).await.is_ok());
}

#[tokio::test]
async fn native_ownership_blocking_upload_keeps_lease_after_async_waiter_is_aborted() {
    let temp = temporary_directory();
    let owner = claim(temp.path()).await.unwrap().finish();
    let guard = owner.admit().unwrap();
    let (release, wait) = std::sync::mpsc::channel();
    let (started, ready) = tokio::sync::oneshot::channel();
    let (finished, done) = tokio::sync::oneshot::channel();
    let waiter = tokio::spawn(async move {
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            started.send(()).unwrap();
            let _ = wait.recv();
            drop(_guard);
            let _ = finished.send(());
        })
        .await
    });
    ready.await.unwrap();
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    assert!(claim(temp.path()).await.is_err());
    release.send(()).unwrap();
    done.await.unwrap();
    assert!(claim(temp.path()).await.is_ok());
}
