use super::*;
use redb::ReadableTableMetadata;
use tempfile::tempdir;

fn put(key: &str, json: &str, ttl_ms: u64) -> CacheRequest {
    CacheRequest::Put {
        key: key.into(),
        json: json.into(),
        ttl_ms,
    }
}
fn get(key: &str) -> CacheRequest {
    CacheRequest::Get { key: key.into() }
}

#[test]
#[ignore = "release-mode interleaved cache probe"]
fn bench_interleaved_stores() {
    for n in [8, 9, 32, 65] {
        let dirs: Vec<_> = (0..n).map(|_| tempdir().unwrap()).collect();
        let caches = Caches::default();
        for dir in &dirs {
            caches
                .execute(dir.path(), put("k", "42", 600_000), false)
                .unwrap();
        }
        let mut samples = Vec::new();
        for dir in dirs.iter().cycle().take(n + 180) {
            let start = std::time::Instant::now();
            let result = caches.execute(dir.path(), get("k"), false).unwrap();
            samples.push(start.elapsed());
            assert_eq!(result.as_deref(), Some("42"));
        }
        let samples = &mut samples[n..]; // one warm-up lap
        samples.sort_unstable();
        eprintln!("{n} stores: p50 {:?}, p95 {:?}", samples[89], samples[170]);
    }
}

fn open_test_writer(path: &Path) -> Database {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .unwrap();
    open_writer(FileBackend::new(file)).unwrap()
}

#[test]
fn eviction_and_expiry_update_both_indexes_and_accounting() {
    let dir = tempdir().unwrap();
    let db = open_test_writer(&dir.path().join(FILE_NAME));
    let run = |r, now| execute(&db, r, || now, 2 * weight("a", "0")).unwrap();
    run(put("a", "1", 1), 10);
    assert_eq!(run(get("a"), 11), None);
    run(put("b", "2", 20), 11); // reclaim unread expired keys
    run(put("c", "3", 10), 11);
    run(put("c", "4", 30), 11); // also removes its old expiry
    run(put("d", "5", 40), 11);
    assert_eq!(run(get("b"), 11), None);
    assert_eq!(run(get("c"), 22).as_deref(), Some("4"));
    run(CacheRequest::Delete { key: "d".into() }, 22);
    let read = db.begin_read().unwrap();
    assert_eq!(read.open_table(ENTRIES).unwrap().len().unwrap(), 1);
    assert_eq!(read.open_table(EXPIRY).unwrap().len().unwrap(), 1);
    assert_eq!(
        read.open_table(USAGE)
            .unwrap()
            .get(())
            .unwrap()
            .unwrap()
            .value(),
        weight("c", "4")
    );
}

#[test]
fn server_restart_shares_commits_but_restore_never_serves_old_inode() {
    let dir = tempdir().unwrap();
    let other = tempdir().unwrap();
    let caches = Caches::default();
    caches
        .execute(dir.path(), put("k", "null", 60_000), false)
        .unwrap();
    assert_eq!(caches.execute(other.path(), get("k"), false).unwrap(), None);
    assert!(!other.path().join(FILE_NAME).exists());
    let successor = Caches::default();
    assert_eq!(
        successor
            .execute(dir.path(), get("k"), false)
            .unwrap()
            .as_deref(),
        Some("null")
    );
    successor
        .execute(dir.path(), put("k", "42", 60_000), false)
        .unwrap();
    assert_eq!(
        caches
            .execute(dir.path(), get("k"), false)
            .unwrap()
            .as_deref(),
        Some("42")
    );
    drop(successor);
    fs::rename(dir.path().join(FILE_NAME), dir.path().join("retired")).unwrap();
    assert_eq!(caches.execute(dir.path(), get("k"), false).unwrap(), None);
}

#[test]
fn invalid_cache_requests_never_open_a_store() {
    let dir = tempdir().unwrap();
    let caches = Caches::default();
    for request in [
        get("  "),
        get(&"k".repeat(1025)),
        put("k", "null", 0),
        put("k", "invalid", 1),
        put("k", &"x".repeat(MAX_JSON + 1), 1),
    ] {
        assert_eq!(
            caches.execute(dir.path(), request, false).unwrap_err().code,
            Some(ErrorCode::InvalidRequest)
        );
    }
    assert!(!dir.path().join(FILE_NAME).exists());
}

#[test]
fn handle_pools_stay_bounded_and_cache_interleaved_reads() {
    let dirs: Vec<_> = (0..32).map(|_| tempdir().unwrap()).collect();
    let caches = Caches::default();
    for dir in &dirs {
        caches
            .execute(dir.path(), put("k", "42", 60_000), false)
            .unwrap();
    }
    for _ in 0..2 {
        for dir in &dirs {
            assert_eq!(
                caches
                    .execute(dir.path(), get("k"), false)
                    .unwrap()
                    .as_deref(),
                Some("42")
            );
        }
    }
    // Reads keep one handle per store, so a round-robin sweep never reopens.
    assert_eq!(
        caches
            .readers
            .opens
            .load(std::sync::atomic::Ordering::Relaxed),
        dirs.len()
    );
    assert_eq!(caches.readers.len(), dirs.len());
    assert_eq!(caches.writers.len(), WRITERS);
}

#[test]
fn disk_pages_are_reused_under_sustained_churn() {
    let dir = tempdir().unwrap();
    let db = open_test_writer(&dir.path().join(FILE_NAME));
    let payload = format!("\"{}\"", "x".repeat(16 * 1024));
    let mut sizes = Vec::new();
    for epoch in 0..4 {
        for i in 0..128 {
            let n = epoch * 128 + i;
            execute(
                &db,
                put(&format!("once:{n}"), &payload, 1000),
                || n,
                256 * 1024,
            )
            .unwrap();
        }
        sizes.push(fs::metadata(dir.path().join(FILE_NAME)).unwrap().len());
    }
    eprintln!("256 KiB logical capacity, 8 MiB churn: file sizes {sizes:?}");
    assert!(sizes[3] <= sizes[0] * 2, "{sizes:?}");
    assert!(sizes[3] < 4 * 1024 * 1024, "{sizes:?}");
}
