use std::sync::atomic::{AtomicU64, Ordering};

use super::*;

struct Fixture {
    dir: tempfile::TempDir,
    path: PathBuf,
    store: Arc<Store>,
    now: Arc<AtomicU64>,
}

fn config(now: &Arc<AtomicU64>) -> StoreConfig {
    let now = now.clone();
    StoreConfig {
        cache_bytes: 1024 * 1024,
        clock: Arc::new(move || now.load(Ordering::SeqCst)),
    }
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.redb");
    let now = Arc::new(AtomicU64::new(1_000_000));
    let store = Arc::new(Store::open(&path, config(&now)).unwrap());
    Fixture {
        dir,
        path,
        store,
        now,
    }
}

impl Fixture {
    fn advance(&self, ms: u64) {
        self.now.fetch_add(ms, Ordering::SeqCst);
    }

    fn get(&self, key: &str) -> Option<String> {
        self.store
            .get(key)
            .unwrap()
            .map(|json| String::from_utf8(json).unwrap())
    }

    fn put(&self, key: &str, json: &str, ttl_ms: Option<u64>) -> Result<()> {
        self.store.put(key, json.as_bytes().to_vec(), ttl_ms)
    }
}

#[test]
fn values_round_trip_and_are_replaced_and_deleted() {
    let f = fixture();
    assert_eq!(f.get("user:1"), None);
    f.put("user:1", r#"{"name":"Ada"}"#, None).unwrap();
    assert_eq!(f.get("user:1").as_deref(), Some(r#"{"name":"Ada"}"#));
    f.put("user:1", "2", None).unwrap();
    assert_eq!(f.get("user:1").as_deref(), Some("2"));
    let stats = f.store.stats().unwrap();
    assert_eq!((stats.keys, stats.expires), (1, 0));
    assert_eq!(stats.bytes, weight("user:1", 1));
    assert!(f.store.delete("user:1").unwrap());
    assert!(!f.store.delete("user:1").unwrap());
    assert_eq!(f.get("user:1"), None);
    assert_eq!(f.store.stats().unwrap(), Stats::default());
}

#[test]
fn keys_expire_on_time_and_are_swept_without_reads() {
    let f = fixture();
    f.put("challenge", r#""abc""#, Some(1_000)).unwrap();
    f.put("kept", "1", None).unwrap();
    assert_eq!(f.store.stats().unwrap().expires, 1);
    f.advance(999);
    assert!(f.get("challenge").is_some());
    f.advance(1);
    assert_eq!(f.get("challenge"), None);
    // A put without a TTL clears an earlier one.
    f.put("session", "1", Some(10)).unwrap();
    f.put("session", "2", None).unwrap();
    f.advance(20);
    assert_eq!(f.get("session").as_deref(), Some("2"));
    // The periodic sweep removes expired keys nobody reads.
    let deadline = Instant::now() + Duration::from_secs(5);
    while f.store.stats().unwrap().keys != 2 {
        assert!(Instant::now() < deadline, "expired keys were not swept");
        std::thread::sleep(Duration::from_millis(50));
    }
    let stats = f.store.stats().unwrap();
    assert_eq!(stats.expires, 0);
    assert_eq!(stats.bytes, weight("kept", 1) + weight("session", 1));
}

#[test]
fn concurrent_writers_share_commits_and_lose_nothing() {
    let f = fixture();
    std::thread::scope(|scope| {
        for writer in 0..16 {
            let store = f.store.clone();
            scope.spawn(move || {
                for i in 0..50 {
                    store
                        .put(&format!("w{writer}:{i}"), b"1".to_vec(), None)
                        .unwrap();
                }
            });
        }
    });
    assert_eq!(f.store.stats().unwrap().keys, 800);
}

#[test]
fn data_survives_reopening() {
    let f = fixture();
    f.put("k", r#""v""#, Some(100_000)).unwrap();
    f.store.close();
    assert!(matches!(f.store.get("k"), Err(StoreError::Closed)));
    let reopened = Store::open(&f.path, config(&f.now)).unwrap();
    assert_eq!(reopened.get("k").unwrap().as_deref(), Some(&b"\"v\""[..]));
    assert_eq!(reopened.stats().unwrap().keys, 1);
}

#[test]
fn exports_round_trip_into_another_store_and_skip_expired_keys() {
    let f = fixture();
    for i in 0..2_500 {
        f.put(&format!("key:{i}"), &format!("{{\"n\":{i}}}"), None)
            .unwrap();
    }
    f.put("short", "1", Some(1_000)).unwrap();
    f.put("long", "2", Some(100_000)).unwrap();
    let export = f.dir.path().join("export.redb");
    let exported = f.store.export_to(&export).unwrap();
    assert_eq!(exported.keys, 2_502);

    let other = fixture();
    other.put("old", "1", None).unwrap();
    other.advance(5_000);
    let imported = other.store.replace_from(&export, None).unwrap().stats;
    // `short` expired before the import; `old` was replaced.
    assert_eq!(imported.keys, 2_501);
    assert_eq!(other.get("old"), None);
    assert_eq!(other.get("key:2499").as_deref(), Some(r#"{"n":2499}"#));
    assert_eq!(other.get("long").as_deref(), Some("2"));
    assert_eq!(other.store.stats().unwrap(), imported);
    // Expiry times are absolute, so `long` keeps its original deadline.
    other.advance(1_000_000 + 100_000 - 1_005_000);
    assert_eq!(other.get("long"), None);
}

#[test]
fn imports_reject_other_files_without_changes() {
    let f = fixture();
    f.put("kept", "1", None).unwrap();
    let not_export = f.dir.path().join("plain.txt");
    std::fs::write(&not_export, "hello").unwrap();
    assert!(matches!(
        f.store.replace_from(&not_export, None),
        Err(StoreError::Invalid(_))
    ));
    assert_eq!(f.get("kept").as_deref(), Some("1"));
}

#[test]
fn replaced_generations_are_freed_in_the_background() {
    let f = fixture();
    for i in 0..3_000 {
        f.put(&format!("k{i}"), "1", Some(1_000_000)).unwrap();
    }
    let export = f.dir.path().join("empty.redb");
    fixture().store.export_to(&export).unwrap();
    f.store.replace_from(&export, None).unwrap();
    assert_eq!(f.store.stats().unwrap(), Stats::default());
    let db = f.store.db().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let txn = db.begin_read().unwrap();
        let (generation, trash) = read_generations(&txn.open_table(META).unwrap()).unwrap();
        if trash.is_none() {
            assert_eq!(generation, 1);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the old generation was not freed"
        );
        drop(txn);
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn snapshots_track_changes_and_replaces_keep_what_they_replace() {
    let f = fixture();
    // A store counts as changed when it opens, so a restart snapshots it.
    assert!(f.store.has_changes());
    let snapshot = f.dir.path().join("snapshot.redb");
    f.store.snapshot_to(&snapshot).unwrap();
    assert!(!f.store.has_changes());
    // Reads and deletes of missing keys change nothing.
    f.get("k");
    f.store.delete("missing").unwrap();
    assert!(!f.store.has_changes());

    f.put("kept", "1", None).unwrap();
    assert!(f.store.has_changes());
    f.store.snapshot_to(&snapshot).unwrap();
    f.put("new", "2", None).unwrap();

    // Restoring the snapshot exports the exact contents it replaces.
    let previous = f.dir.path().join("previous.redb");
    let replaced = f.store.replace_from(&snapshot, Some(&previous)).unwrap();
    assert!(
        replaced.previous_changed,
        "`new` was written after the snapshot"
    );
    assert!(f.store.has_changes());
    assert_eq!(f.get("new"), None);
    assert_eq!(f.get("kept").as_deref(), Some("1"));
    let other = fixture();
    other
        .store
        .snapshot_to(&other.dir.path().join("s.redb"))
        .unwrap();
    // Unchanged since its snapshot, so its contents need no backup copy.
    let replaced = other
        .store
        .replace_from(&previous, Some(&other.dir.path().join("p.redb")))
        .unwrap();
    assert!(!replaced.previous_changed);
    assert_eq!(other.get("new").as_deref(), Some("2"));
    assert_eq!(other.get("kept").as_deref(), Some("1"));
}
