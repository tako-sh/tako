use super::*;
use std::sync::{
    Barrier,
    atomic::{AtomicUsize, Ordering},
};

fn tracked(registry: &Arc<RequestDrain>, calls: &Arc<AtomicUsize>) -> RequestGuard {
    let calls = calls.clone();
    registry
        .register_transport(Some(Transport::Fake(Box::new(move || {
            calls.fetch_add(1, Ordering::SeqCst);
        }))))
        .unwrap()
}

#[test]
fn begin_closes_only_long_lived_and_accounts_until_drop() {
    let registry = RequestDrain::default();
    let short_calls = Arc::new(AtomicUsize::new(0));
    let long_calls = Arc::new(AtomicUsize::new(0));
    let short = tracked(&registry, &short_calls);
    let long = tracked(&registry, &long_calls);
    long.mark_long_lived();
    registry.begin_drain();
    registry.begin_drain();
    assert_eq!(long_calls.load(Ordering::SeqCst), 1);
    assert_eq!(short_calls.load(Ordering::SeqCst), 0);
    assert_eq!(registry.active_count(), 2);
    assert!(registry.register(None).is_none());
    assert!(!registry.resume());
    drop(long);
    assert_eq!(registry.active_count(), 1);
    drop(short);
    assert_eq!(registry.active_count(), 0);
    assert!(registry.resume());
    assert!(registry.register(None).is_some());
}

#[test]
fn late_upgrade_or_sse_classification_closes_immediately() {
    let registry = RequestDrain::default();
    let calls = Arc::new(AtomicUsize::new(0));
    let guard = tracked(&registry, &calls);
    registry.begin_drain();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    guard.mark_long_lived();
    guard.mark_long_lived();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(registry.active_count(), 1);
    assert!(!registry.resume());
    drop(guard);
    assert!(registry.resume());
}

#[test]
fn force_closes_all_transports_once_without_claiming_completion() {
    let registry = RequestDrain::default();
    let calls = Arc::new(AtomicUsize::new(0));
    let a = tracked(&registry, &calls);
    let b = tracked(&registry, &calls);
    let socketless = registry.register(None).unwrap();
    a.mark_long_lived();
    registry.begin_drain();
    assert_eq!(registry.force_drain(), 1);
    assert_eq!(registry.force_drain(), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(registry.active_count(), 3);
    assert!(registry.register(None).is_none());
    drop((a, b));
    assert_eq!(registry.active_count(), 1);
    assert!(!registry.resume());
    drop(socketless);
    assert!(registry.resume());
}

#[test]
fn force_without_begin_also_prevents_admission() {
    let registry = RequestDrain::default();
    let calls = Arc::new(AtomicUsize::new(0));
    let guard = tracked(&registry, &calls);
    assert_eq!(registry.force_drain(), 1);
    assert!(registry.register(None).is_none());
    guard.mark_long_lived();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    drop(guard);
    assert!(registry.resume());
}

#[test]
fn capacity_is_bounded_and_drop_releases_admission_slot() {
    let registry = RequestDrain::default();
    let mut guards: Vec<_> = (0..MAX_ACTIVE_REQUESTS)
        .map(|_| registry.register(None).unwrap())
        .collect();
    assert_eq!(registry.active_count(), MAX_ACTIVE_REQUESTS);
    assert!(registry.register(None).is_none());
    guards.pop();
    let replacement = registry.register(None).unwrap();
    assert_eq!(registry.active_count(), MAX_ACTIVE_REQUESTS);
    drop((guards, replacement));
    assert_eq!(registry.active_count(), 0);
}

#[test]
fn transport_and_registry_remain_owned_until_guard_drop() {
    let registry = RequestDrain::default();
    let weak_registry = Arc::downgrade(&registry);
    let resource = Arc::new(());
    let captured = resource.clone();
    let guard = registry
        .register_transport(Some(Transport::Fake(Box::new(move || {
            let _ = &captured;
        }))))
        .unwrap();
    registry.force_drain();
    assert_eq!(Arc::strong_count(&resource), 2);
    drop(registry);
    assert!(weak_registry.upgrade().is_some());
    drop(guard);
    assert!(weak_registry.upgrade().is_none());
    assert_eq!(Arc::strong_count(&resource), 1);
}

#[test]
fn ordinary_completion_does_not_shutdown_the_reusable_connection() {
    let registry = RequestDrain::default();
    let calls = Arc::new(AtomicUsize::new(0));
    drop(tracked(&registry, &calls));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(registry.active_count(), 0);
}

#[test]
fn real_owned_descriptor_survives_shutdown_error_until_guard_drop() {
    // A non-socket exercises the actual libc error path without live sockets. Errors must not
    // remove accounting or drop the retained descriptor prematurely.
    let registry = RequestDrain::default();
    let file = std::fs::File::open("/dev/null").unwrap();
    let socket: OwnedFd = file.into();
    let guard = registry.register(Some(socket)).unwrap();
    assert_eq!(registry.force_drain(), 1);
    assert_eq!(registry.active_count(), 1);
    let state = registry.state();
    let Some(Transport::Socket(socket)) = &state.active[&guard.id].transport else {
        panic!("socket")
    };
    assert!(socket.try_clone().is_ok());
    drop(state);
    assert!(!registry.resume());
    drop(guard);
    assert_eq!(registry.active_count(), 0);
}

#[test]
fn concurrent_begin_and_mark_cannot_miss_shutdown() {
    for _ in 0..32 {
        let registry = RequestDrain::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let guard = tracked(&registry, &calls);
        let barrier = Barrier::new(2);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                barrier.wait();
                registry.begin_drain();
            });
            scope.spawn(|| {
                barrier.wait();
                guard.mark_long_lived();
            });
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(registry.active_count(), 1);
        assert!(!registry.resume());
        drop(guard);
        assert!(registry.resume());
    }
}

#[test]
fn concurrent_registration_and_begin_have_one_admission_order() {
    for _ in 0..32 {
        let registry = RequestDrain::default();
        let barrier = Barrier::new(2);
        let guard = std::thread::scope(|scope| {
            scope.spawn(|| {
                barrier.wait();
                registry.begin_drain();
            });
            let register = scope.spawn(|| {
                barrier.wait();
                registry.register(None)
            });
            register.join().unwrap()
        });
        assert!(registry.register(None).is_none());
        assert_eq!(registry.active_count(), usize::from(guard.is_some()));
        if guard.is_some() {
            assert!(!registry.resume());
        }
        drop(guard);
        assert!(registry.resume());
    }
}

#[test]
fn resumed_requests_have_distinct_ids_and_fresh_shutdown_state() {
    let registry = RequestDrain::default();
    let calls = Arc::new(AtomicUsize::new(0));
    let old = tracked(&registry, &calls);
    let old_id = old.id;
    registry.force_drain();
    drop(old);
    assert!(registry.resume());
    let new = tracked(&registry, &calls);
    assert!(new.id > old_id);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(registry.force_drain(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(registry.active_count(), 1);
}

#[test]
fn exhausted_identity_space_rejects_without_reusing_guard_identity() {
    let registry = RequestDrain::default();
    registry.state().next_id = u64::MAX;
    assert!(registry.register(None).is_none());
    assert_eq!(registry.active_count(), 0);
}
