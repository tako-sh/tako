//! Offline state-machine tests: no installed helper, Docker, WG or DNS port53.
use super::*;
use crate::native_agent::{
    lifecycle::{BrokerOwner, Control},
    tests::Fixture,
};
use std::{
    io,
    os::unix::fs::PermissionsExt,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

fn prepared(fixture: &Fixture) -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    let root = fixture.config.network_dir(temp.path());
    std::fs::create_dir_all(&root).unwrap();
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).unwrap();
    temp
}
fn apps() -> BTreeSet<AppEnv> {
    ["api/prod".parse().unwrap()].into()
}
fn snapshot(f: &Fixture, time: u64) -> ServiceSnapshot {
    ServiceSnapshot {
        observed_at: time,
        registry: f.config.bootstrap.clone(),
        leases: vec![
            ServiceLease::sign(&f.keys, f.config.network.id.clone(), apps(), time).unwrap(),
        ],
    }
}

#[test]
fn lease_retry_survives_restart_without_same_second_fork() {
    let f = Fixture::new();
    let temp = prepared(&f);
    let mut publisher = Publisher::open(&f.config, temp.path()).unwrap();
    let mut attempted = None;
    assert!(
        publisher
            .publish_with(&f.config, &f.keys, &apps(), 10, |lease| {
                attempted = Some(lease.clone());
                Err(AgentError::Timeout)
            })
            .is_err()
    );
    let mut publisher = Publisher::open(&f.config, temp.path()).unwrap();
    publisher
        .publish_with(&f.config, &f.keys, &apps(), 10, |lease| {
            assert_eq!(Some(lease), attempted.as_ref());
            Ok(())
        })
        .unwrap();
    publisher
        .publish_with(&f.config, &f.keys, &BTreeSet::new(), 10, |_| {
            panic!("forked same second")
        })
        .unwrap();
    publisher
        .publish_with(&f.config, &f.keys, &BTreeSet::new(), 11, |lease| {
            assert!(lease.apps.is_empty());
            assert_eq!(lease.issued_at, 11);
            Ok(())
        })
        .unwrap();
    assert!(
        publisher
            .publish_with(&f.config, &f.keys, &apps(), 10, |_| Ok(()))
            .is_err()
    );
}

#[test]
fn leases_renew_at_ten_seconds_and_never_extend_failed_envelopes() {
    let f = Fixture::new();
    let temp = prepared(&f);
    let mut publisher = Publisher::open(&f.config, temp.path()).unwrap();
    publisher
        .publish_with(&f.config, &f.keys, &apps(), 10, |lease| {
            assert_eq!(lease.expires_at, 40);
            Ok(())
        })
        .unwrap();
    publisher
        .publish_with(&f.config, &f.keys, &apps(), 19, |_| panic!("early renewal"))
        .unwrap();
    assert!(
        publisher
            .publish_with(&f.config, &f.keys, &apps(), 20, |lease| {
                assert_eq!(lease.expires_at, 50);
                Err(AgentError::Timeout)
            })
            .is_err()
    );
    publisher
        .publish_with(&f.config, &f.keys, &apps(), 21, |lease| {
            assert_eq!((lease.issued_at, lease.expires_at), (20, 50));
            Ok(())
        })
        .unwrap();
}

#[test]
fn missing_stale_malformed_or_unprotected_export_withdraws_apps() {
    let f = Fixture::new();
    let temp = prepared(&f);
    let publisher = Publisher::open(&f.config, temp.path()).unwrap();
    assert!(publisher.local_apps(10).is_empty());
    publisher
        .files
        .write(
            LOCAL_FILE,
            &serde_json::to_vec(&LocalServices {
                observed_at: 10,
                apps: apps(),
            })
            .unwrap(),
        )
        .unwrap();
    assert_eq!(publisher.local_apps(24), apps());
    assert!(publisher.local_apps(25).is_empty());
    assert!(publisher.local_apps(9).is_empty());
    let path = f.config.network_dir(temp.path()).join(LOCAL_FILE);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(publisher.local_apps(10).is_empty());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    publisher.files.write(LOCAL_FILE, b"{broken").unwrap();
    assert!(publisher.local_apps(10).is_empty());
    publisher
        .files
        .write(LOCAL_FILE, &vec![b' '; MAX_LEASE_BYTES + 1])
        .unwrap();
    assert!(publisher.local_apps(10).is_empty());
}

#[test]
fn publication_requires_enforcement_and_withdrawal_clears_file_and_memory() {
    let f = Fixture::new();
    let temp = prepared(&f);
    let shared = Shared::open(&f.config, temp.path()).unwrap();
    let (mut control, _, _) = Control::new();
    control.attach_services(shared.clone());
    let value = snapshot(&f, 10);
    assert!(control.publish_services(&value, 10).is_err());
    control.publish_enforced(&value.registry);
    control.publish_services(&value, 10).unwrap();
    assert_eq!(
        shared
            .directory
            .read()
            .placements(&"api/prod".parse().unwrap(), 10)
            .len(),
        1
    );
    let read = || {
        serde_json::from_slice::<Option<ServiceSnapshot>>(
            &shared
                .files
                .read(SNAPSHOT_FILE, MAX_SNAPSHOT_BYTES)
                .unwrap(),
        )
        .unwrap()
    };
    assert!(read().is_some());
    control.withdraw();
    assert!(read().is_none());
    assert!(
        shared
            .directory
            .read()
            .placements(&"api/prod".parse().unwrap(), 10)
            .is_empty()
    );
    assert!(control.publish_services(&value, 10).is_err());
    control.publish_enforced(&value.registry);
    assert!(control.publish_services(&value, 25).is_err());
    assert!(read().is_none());
}

#[test]
fn racing_shutdown_cannot_restore_service_file() {
    let f = Fixture::new();
    let temp = prepared(&f);
    let shared = Shared::open(&f.config, temp.path()).unwrap();
    for _ in 0..8 {
        let (mut control, _, _) = Control::new();
        control.attach_services(shared.clone());
        control.publish_enforced(&f.config.bootstrap);
        let writer = control.clone();
        let value = snapshot(&f, 10);
        let thread = std::thread::spawn(move || {
            let _ = writer.publish_services(&value, 10);
        });
        control.shutdown();
        thread.join().unwrap();
        assert_eq!(&*shared.files.read(SNAPSHOT_FILE, 16).unwrap(), b"null\n");
    }
}

#[tokio::test]
async fn dns_waits_for_enforcement_and_shutdown_cancels_the_wait() {
    let f = Fixture::new();
    let (control, mut membership, shutdown) = Control::new();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(10),
            wait_enforced(&mut membership, shutdown.clone())
        )
        .await
        .is_err()
    );
    control.publish_enforced(&f.config.bootstrap);
    assert!(wait_enforced(&mut membership, shutdown.clone()).await);
    control.shutdown();
    assert!(!wait_enforced(&mut membership, shutdown).await);
}

#[tokio::test]
async fn missing_bridge_retries_while_native_lives_and_drains_on_shutdown() {
    let (control, _, shutdown) = Control::new();
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempted = attempts.clone();
    let retry = retry_compose(shutdown.clone(), move || {
        attempted.fetch_add(1, Ordering::SeqCst);
        async { Err(io::Error::from(io::ErrorKind::AddrNotAvailable)) }
    });
    let native = supervise_dns(
        async {
            stopping(shutdown).await;
            Ok(())
        },
        &control,
    );
    let exercise = async {
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert!(!control.stop.load(Ordering::Acquire));
        control.shutdown();
    };
    let (retry, native, ()) = tokio::join!(retry, native, exercise);
    retry.unwrap();
    native.unwrap();
}

#[tokio::test]
async fn dns_fatal_error_isolates_and_late_failure_holds_database_gate() {
    let (control, members, shutdown) = Control::new();
    control.services_drained.store(false, Ordering::Release);
    let mut owner = BrokerOwner::services(control.clone());
    let result = supervise_dns(
        retry_compose(shutdown, || async {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        }),
        &control,
    )
    .await;
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    assert!(control.stop.load(Ordering::Acquire));
    assert!(members.borrow().is_none());
    assert!(!control.services_drained.load(Ordering::Acquire));
    owner.complete(true);
    assert!(control.services_drained.load(Ordering::Acquire));
}
