use std::sync::atomic::{AtomicU64, Ordering};

use super::*;

struct Fixture {
    dir: tempfile::TempDir,
    path: PathBuf,
    store: Arc<Store>,
    now: Arc<AtomicU64>,
}

fn config(now: &Arc<AtomicU64>, max_bytes: u64) -> StoreConfig {
    let now = now.clone();
    StoreConfig {
        max_bytes,
        cache_bytes: 1024 * 1024,
        clock: Arc::new(move || now.load(Ordering::SeqCst)),
    }
}

fn fixture(max_bytes: u64) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.redb");
    let now = Arc::new(AtomicU64::new(1_000_000));
    let store = Arc::new(Store::open(&path, config(&now, max_bytes)).unwrap());
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
    let f = fixture(1 << 30);
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
    let f = fixture(1 << 30);
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
fn full_stores_refuse_only_writes_that_would_grow_them() {
    let limit = 2 * weight("a", 100);
    let f = fixture(limit);
    let value = format!("\"{}\"", "x".repeat(98));
    f.put("a", &value, None).unwrap();
    f.put("b", &value, None).unwrap();
    assert_eq!(f.store.stats().unwrap().bytes, limit);
    assert!(matches!(f.put("c", "1", None), Err(StoreError::Full { .. })));
    assert!(matches!(
        f.put("a", &format!("\"{}\"", "x".repeat(99)), None),
        Err(StoreError::Full { .. })
    ));
    // Overwrites no larger, and deletes, still work.
    f.put("a", &value, None).unwrap();
    f.put("a", "1", None).unwrap();
    f.store.delete("b").unwrap();
    f.put("c", &value, None).unwrap();
    assert!(f.store.stats().unwrap().bytes <= limit);
}

#[test]
fn concurrent_writers_share_commits_and_lose_nothing() {
    let f = fixture(1 << 30);
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
    let f = fixture(1 << 30);
    f.put("k", r#""v""#, Some(100_000)).unwrap();
    f.store.close();
    assert!(matches!(f.store.get("k"), Err(StoreError::Closed)));
    let reopened = Store::open(&f.path, config(&f.now, 1 << 30)).unwrap();
    assert_eq!(reopened.get("k").unwrap().as_deref(), Some(&b"\"v\""[..]));
    assert_eq!(reopened.stats().unwrap().keys, 1);
}

#[test]
fn exports_round_trip_into_another_store_and_skip_expired_keys() {
    let f = fixture(1 << 30);
    for i in 0..2_500 {
        f.put(&format!("key:{i}"), &format!("{{\"n\":{i}}}"), None)
            .unwrap();
    }
    f.put("short", "1", Some(1_000)).unwrap();
    f.put("long", "2", Some(100_000)).unwrap();
    let export = f.dir.path().join("export.redb");
    let exported = f.store.export_to(&export).unwrap();
    assert_eq!(exported.keys, 2_502);

    let other = fixture(1 << 30);
    other.put("old", "1", None).unwrap();
    other.advance(5_000);
    let imported = other.store.replace_from(&export).unwrap();
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
fn imports_reject_other_files_and_oversized_exports_without_changes() {
    let f = fixture(1 << 30);
    f.put("kept", "1", None).unwrap();
    let not_export = f.dir.path().join("plain.txt");
    std::fs::write(&not_export, "hello").unwrap();
    assert!(matches!(
        f.store.replace_from(&not_export),
        Err(StoreError::Invalid(_))
    ));
    // An export that does not fit this store's limit is refused whole.
    let big = fixture(1 << 30);
    for i in 0..100 {
        big.put(&format!("k{i}"), &format!("\"{}\"", "x".repeat(100)), None)
            .unwrap();
    }
    let export = f.dir.path().join("big.redb");
    big.store.export_to(&export).unwrap();
    let small = fixture(weight("k0", 102) * 10);
    small.put("kept", "1", None).unwrap();
    assert!(small.store.replace_from(&export).is_err());
    assert_eq!(small.get("kept").as_deref(), Some("1"));
    assert_eq!(f.get("kept").as_deref(), Some("1"));
}

#[test]
fn replaced_generations_are_freed_in_the_background() {
    let f = fixture(1 << 30);
    for i in 0..3_000 {
        f.put(&format!("k{i}"), "1", Some(1_000_000)).unwrap();
    }
    let export = f.dir.path().join("empty.redb");
    fixture(1 << 30).store.export_to(&export).unwrap();
    f.store.replace_from(&export).unwrap();
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
        assert!(Instant::now() < deadline, "the old generation was not freed");
        drop(txn);
        std::thread::sleep(Duration::from_millis(50));
    }
}
