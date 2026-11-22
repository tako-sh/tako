use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::command::{self, Limits};

struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
    store: Arc<Store>,
    now: Arc<AtomicU64>,
}

fn config(now: &Arc<AtomicU64>, max_bytes: u64) -> StoreConfig {
    let now = now.clone();
    StoreConfig {
        max_bytes,
        limits: Limits {
            max_key_bytes: 1024,
            max_value_bytes: 1024 * 1024,
        },
        cache_bytes: 1024 * 1024,
        max_reply_bytes: 1024 * 1024,
        clock: Arc::new(move || now.load(Ordering::SeqCst)),
    }
}

fn fixture(max_bytes: u64) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kv.redb");
    let now = Arc::new(AtomicU64::new(1_000_000));
    let store = Arc::new(Store::open(&path, config(&now, max_bytes)).unwrap());
    Fixture {
        _dir: dir,
        path,
        store,
        now,
    }
}

fn parse(text: &str) -> Cmd {
    let args: Vec<Bytes> = text
        .split(' ')
        .map(|a| Bytes::copy_from_slice(a.as_bytes()))
        .collect();
    let spec = command::lookup(&args).unwrap();
    command::parse(
        spec,
        &args,
        Limits {
            max_key_bytes: 1024,
            max_value_bytes: 1024 * 1024,
        },
    )
    .unwrap()
}

impl Fixture {
    async fn run(&self, text: &str) -> Reply {
        self.store.run(vec![parse(text)]).await.unwrap().remove(0)
    }

    fn advance(&self, ms: u64) {
        self.now.fetch_add(ms, Ordering::SeqCst);
    }

    async fn stats(&self) -> Stats {
        self.store.stats().await.unwrap()
    }
}

fn bulk(text: &str) -> Reply {
    Reply::bulk_str(text)
}

#[tokio::test]
async fn strings_expire_on_time() {
    let f = fixture(1 << 30);
    assert_eq!(f.run("SET k v EX 10").await, Reply::Ok);
    assert_eq!(f.run("TTL k").await, Reply::Int(10));
    assert_eq!(f.run("PTTL k").await, Reply::Int(10_000));
    assert_eq!(f.run("EXPIRETIME k").await, Reply::Int(1_010));
    f.advance(9_999);
    assert_eq!(f.run("GET k").await, bulk("v"));
    f.advance(1);
    assert_eq!(f.run("GET k").await, Reply::Nil);
    assert_eq!(f.run("EXISTS k").await, Reply::Int(0));
    assert_eq!(f.run("TTL k").await, Reply::Int(-2));
    // A write finds the expired key absent.
    assert_eq!(f.run("SET k v2 NX").await, Reply::Ok);
    assert_eq!(f.run("TTL k").await, Reply::Int(-1));
    assert_eq!(f.stats().await.keys, 1);
}

#[tokio::test]
async fn expire_options_follow_redis() {
    let f = fixture(1 << 30);
    f.run("SET k v").await;
    assert_eq!(f.run("EXPIRE k 100 XX").await, Reply::Int(0));
    assert_eq!(f.run("EXPIRE k 100 GT").await, Reply::Int(0));
    assert_eq!(f.run("EXPIRE k 100 LT").await, Reply::Int(1));
    assert_eq!(f.run("EXPIRE k 200 NX").await, Reply::Int(0));
    assert_eq!(f.run("EXPIRE k 200 GT").await, Reply::Int(1));
    assert_eq!(f.run("EXPIRE k 300 XX LT").await, Reply::Int(0));
    assert_eq!(f.run("PEXPIRE k 150000 XX LT").await, Reply::Int(1));
    assert_eq!(f.run("TTL k").await, Reply::Int(150));
    assert_eq!(f.run("PERSIST k").await, Reply::Int(1));
    assert_eq!(f.run("PERSIST k").await, Reply::Int(0));
    assert_eq!(f.run("EXPIRE missing 10").await, Reply::Int(0));
    // A time in the past deletes the key.
    assert_eq!(f.run("EXPIRE k -1").await, Reply::Int(1));
    assert_eq!(f.run("EXISTS k").await, Reply::Int(0));
    assert_eq!(f.run("SET k v EXAT 1").await, Reply::Ok);
    assert_eq!(f.run("EXISTS k").await, Reply::Int(0));
    assert_eq!(
        f.stats().await,
        Stats {
            seq: f.stats().await.seq,
            ..Stats::default()
        }
    );
}

#[tokio::test]
async fn set_options_and_variants() {
    let f = fixture(1 << 30);
    assert_eq!(f.run("SET k v XX").await, Reply::Nil);
    assert_eq!(f.run("SET k v NX GET").await, Reply::Nil);
    assert_eq!(f.run("SET k w NX").await, Reply::Nil);
    assert_eq!(f.run("SET k w GET PX 5000").await, bulk("v"));
    assert_eq!(f.run("SET k x KEEPTTL").await, Reply::Ok);
    assert_eq!(f.run("PTTL k").await, Reply::Int(5000));
    assert_eq!(f.run("GETSET k y").await, bulk("x"));
    assert_eq!(f.run("TTL k").await, Reply::Int(-1));
    assert_eq!(f.run("SETNX k z").await, Reply::Int(0));
    assert_eq!(f.run("GETEX k EX 30").await, bulk("y"));
    assert_eq!(f.run("TTL k").await, Reply::Int(30));
    assert_eq!(f.run("GETEX k PERSIST").await, bulk("y"));
    assert_eq!(f.run("TTL k").await, Reply::Int(-1));
    assert_eq!(f.run("GETDEL k").await, bulk("y"));
    assert_eq!(f.run("GETDEL k").await, Reply::Nil);
    assert_eq!(f.run("MSET a 1 b 2").await, Reply::Ok);
    assert_eq!(f.run("MSETNX b 3 c 4").await, Reply::Int(0));
    assert_eq!(f.run("MSETNX c 3 d 4").await, Reply::Int(1));
    assert_eq!(
        f.run("MGET a b c missing").await,
        Reply::Array(vec![bulk("1"), bulk("2"), bulk("3"), Reply::Nil])
    );
    assert_eq!(f.run("APPEND a 23").await, Reply::Int(3));
    assert_eq!(f.run("STRLEN a").await, Reply::Int(3));
    assert_eq!(f.run("DEL a b missing").await, Reply::Int(2));
}

#[tokio::test]
async fn counters_check_types_and_overflow() {
    let f = fixture(1 << 30);
    assert_eq!(f.run("INCR n").await, Reply::Int(1));
    assert_eq!(f.run("INCRBY n 41").await, Reply::Int(42));
    assert_eq!(f.run("DECRBY n 2").await, Reply::Int(40));
    assert_eq!(f.run("DECR n").await, Reply::Int(39));
    f.run("EXPIRE n 60").await;
    assert_eq!(f.run("INCR n").await, Reply::Int(40));
    assert_eq!(
        f.run("TTL n").await,
        Reply::Int(60),
        "INCR keeps the expiry"
    );
    f.run("SET n 9223372036854775807").await;
    assert_eq!(f.run("INCR n").await, Reply::error(OVERFLOW));
    f.run("SET s abc").await;
    assert_eq!(f.run("INCR s").await, Reply::error(NOT_INTEGER));
    assert_eq!(f.run("INCRBYFLOAT s 1").await, Reply::error(NOT_FLOAT));
    f.run("SET f 10.5").await;
    assert_eq!(f.run("INCRBYFLOAT f 0.1").await, bulk("10.6"));
    assert_eq!(f.run("INCRBYFLOAT f -0.6").await, bulk("10"));
    assert_eq!(
        f.run("INCRBYFLOAT f 1e308").await,
        bulk(&format!("{}", 10.0 + 1e308))
    );
    f.run("HSET h a 1").await;
    assert_eq!(f.run("INCR h").await, Reply::error(WRONGTYPE));
    assert_eq!(f.run("GET h").await, Reply::error(WRONGTYPE));
    assert_eq!(f.run("SET h x GET").await, Reply::error(WRONGTYPE));
    assert_eq!(f.run("TYPE h").await, Reply::Simple("hash".into()));
    // SET replaces a hash outright.
    assert_eq!(f.run("SET h x").await, Reply::Ok);
    assert_eq!(f.run("TYPE h").await, Reply::Simple("string".into()));
}

#[tokio::test]
async fn hashes_track_fields_and_sizes() {
    let f = fixture(1 << 30);
    assert_eq!(f.run("HSET h a 1 b 2").await, Reply::Int(2));
    assert_eq!(f.run("HSET h b 3 c 4").await, Reply::Int(1));
    assert_eq!(f.run("HLEN h").await, Reply::Int(3));
    assert_eq!(f.run("HGET h b").await, bulk("3"));
    assert_eq!(
        f.run("HGETALL h").await,
        Reply::Map(vec![
            (bulk("a"), bulk("1")),
            (bulk("b"), bulk("3")),
            (bulk("c"), bulk("4"))
        ])
    );
    assert_eq!(
        f.run("HMGET h a x c").await,
        Reply::Array(vec![bulk("1"), Reply::Nil, bulk("4")])
    );
    assert_eq!(f.run("HSETNX h a 9").await, Reply::Int(0));
    assert_eq!(f.run("HINCRBY h a 5").await, Reply::Int(6));
    assert_eq!(f.run("HINCRBYFLOAT h z 1.5").await, bulk("1.5"));
    assert_eq!(f.run("HSET h t text").await, Reply::Int(1));
    assert_eq!(f.run("HINCRBY h t 1").await, Reply::error(HASH_NOT_INTEGER));
    assert_eq!(f.run("HSTRLEN h t").await, Reply::Int(4));
    assert_eq!(f.run("HEXISTS h t").await, Reply::Int(1));
    assert_eq!(
        f.run("HSCAN h 0 MATCH [ab] NOVALUES").await,
        Reply::Array(vec![bulk("0"), Reply::Array(vec![bulk("a"), bulk("b")])])
    );
    f.run("EXPIRE h 100").await;
    assert_eq!(f.run("RENAME h g").await, Reply::Ok);
    assert_eq!(f.run("HLEN g").await, Reply::Int(5));
    assert_eq!(f.run("TTL g").await, Reply::Int(100));
    assert_eq!(f.run("HDEL g a b c z").await, Reply::Int(4));
    assert_eq!(f.run("HDEL g t").await, Reply::Int(1));
    // The last field takes the key with it, and every byte is returned.
    assert_eq!(f.run("EXISTS g").await, Reply::Int(0));
    let stats = f.stats().await;
    assert_eq!((stats.keys, stats.expires, stats.bytes), (0, 0, 0));
}

#[tokio::test]
async fn sizes_return_to_zero_after_every_kind_of_removal() {
    let f = fixture(1 << 30);
    f.run("SET a 1").await;
    f.run("SET b 2 EX 5").await;
    f.run("HSET h x 1 y 2").await;
    f.run("RENAME a c").await;
    f.run("RENAMENX c b").await;
    assert_eq!(f.stats().await.keys, 3);
    f.run("FLUSHDB").await;
    let stats = f.stats().await;
    assert_eq!((stats.keys, stats.expires, stats.bytes), (0, 0, 0));
    f.run("SET a 1 EX 1").await;
    f.run("HSET h x 1").await;
    f.run("EXPIRE h 1").await;
    f.advance(1_000);
    // The periodic sweep removes them without any further command.
    let deadline = Instant::now() + Duration::from_secs(5);
    while f.stats().await.keys != 0 {
        assert!(Instant::now() < deadline, "expired keys were not swept");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let stats = f.stats().await;
    assert_eq!((stats.expires, stats.bytes), (0, 0));
}

#[tokio::test]
async fn full_stores_refuse_growth_but_allow_removal() {
    let f = fixture(4096);
    let value = "x".repeat(500);
    let mut refused = false;
    for i in 0..20 {
        match f.run(&format!("SET k{i} {value}")).await {
            Reply::Ok => {}
            Reply::Error(message) => {
                assert_eq!(message, OOM);
                refused = true;
                break;
            }
            other => panic!("{other:?}"),
        }
    }
    assert!(refused);
    assert_eq!(f.run("HSET h a b").await, Reply::error(OOM));
    assert!(matches!(f.run("DEL k0 k1 k2").await, Reply::Int(3)));
    assert_eq!(f.run(&format!("SET k0 {value}")).await, Reply::Ok);
}

/// Reads `keys` for a transaction, as `WATCH` does.
async fn watch(f: &Fixture, keys: &[&str]) -> (Vec<Watched>, WatchGuard) {
    f.store
        .watch(
            keys.iter()
                .map(|k| Bytes::copy_from_slice(k.as_bytes()))
                .collect(),
        )
        .await
        .unwrap()
}

fn ops() -> Vec<Op> {
    vec![
        Op::Cmd(parse("INCR k")),
        Op::Failed(Reply::error("ERR x")),
        Op::Cmd(parse("GET k")),
    ]
}

#[tokio::test]
async fn transactions_abort_when_watched_keys_change() {
    let f = fixture(1 << 30);
    f.run("SET k 1").await;
    let (watched, _guard) = watch(&f, &["k", "absent"]).await;
    assert!(watched[0].version.is_some() && watched[1].version.is_none());
    assert_eq!(
        f.store.exec(ops(), watched).await.unwrap(),
        Reply::Array(vec![Reply::Int(2), Reply::error("ERR x"), bulk("2")])
    );
    // The transaction itself changed k: a second one with the same watch aborts.
    let (watched, _guard) = watch(&f, &["k"]).await;
    f.run("INCR k").await;
    assert_eq!(f.store.exec(ops(), watched).await.unwrap(), Reply::NilArray);
    // Creating a key that was absent counts.
    let (watched, _guard) = watch(&f, &["absent"]).await;
    f.run("SET absent now").await;
    assert_eq!(f.store.exec(ops(), watched).await.unwrap(), Reply::NilArray);
    // Expiry counts as a change.
    f.run("SET t v PX 10").await;
    let (watched, _guard) = watch(&f, &["t"]).await;
    f.advance(10);
    assert_eq!(f.store.exec(ops(), watched).await.unwrap(), Reply::NilArray);
}

#[tokio::test]
async fn absent_watched_keys_notice_being_created_and_removed() {
    let f = fixture(1 << 30);
    // Created and deleted again: absent at WATCH and at EXEC, but modified.
    let (watched, guard) = watch(&f, &["lock"]).await;
    f.run("SET lock other").await;
    f.run("DEL lock").await;
    assert_eq!(f.store.exec(ops(), watched).await.unwrap(), Reply::NilArray);
    drop(guard);
    // Created and flushed.
    let (watched, guard) = watch(&f, &["lock"]).await;
    f.run("SET lock other").await;
    f.run("FLUSHDB").await;
    assert_eq!(f.store.exec(ops(), watched).await.unwrap(), Reply::NilArray);
    drop(guard);
    // A removal before the WATCH does not count, and untouched keys run.
    f.run("SET lock other").await;
    f.run("DEL lock").await;
    let (watched, guard) = watch(&f, &["lock"]).await;
    assert!(matches!(
        f.store.exec(ops(), watched).await.unwrap(),
        Reply::Array(_)
    ));
    drop(guard);
    // Nothing is kept once no connection watches.
    let (watched, guard) = watch(&f, &["a"]).await;
    f.run("SET b 1").await;
    f.run("DEL b").await;
    assert!(!f.store.removals.lock().removed.is_empty());
    drop((watched, guard));
    let removals = f.store.removals.lock();
    assert!(removals.removed.is_empty() && removals.watching.is_empty());
}

#[tokio::test]
async fn transactions_cap_their_replies() {
    let f = fixture(1 << 30);
    let value = "x".repeat(300 * 1024);
    f.run(&format!("SET big {value}")).await;
    let mut ops: Vec<Op> = (0..8).map(|_| Op::Cmd(parse("GET big"))).collect();
    ops.push(Op::Cmd(parse("INCR n")));
    let Reply::Array(replies) = f.store.exec(ops, Vec::new()).await.unwrap() else {
        panic!()
    };
    // The 1 MiB reply limit fits three values; later ones become errors,
    // and the writes after them still run.
    assert_eq!(
        replies
            .iter()
            .filter(|r| matches!(r, Reply::Bulk(_)))
            .count(),
        3
    );
    assert!(matches!(&replies[7], Reply::Error(m) if m.contains("exceed")));
    assert_eq!(replies[8], Reply::Int(1));
    assert_eq!(f.run("GET n").await, bulk("1"));
}

#[tokio::test]
async fn scan_pages_stop_at_the_reply_limit() {
    let f = fixture(1 << 30);
    let long = "k".repeat(1000);
    for chunk in 0..30 {
        let pairs: Vec<String> = (0..100)
            .map(|i| format!("{long}{} v", chunk * 100 + i))
            .collect();
        f.run(&format!("MSET {}", pairs.join(" "))).await;
    }
    let Reply::Array(page) = f.run("SCAN 0 COUNT 100000").await else {
        panic!()
    };
    let [Reply::Bulk(cursor), Reply::Array(keys)] = page.as_slice() else {
        panic!()
    };
    assert_ne!(cursor.as_ref(), b"0", "the page ended before every key");
    assert!(keys.len() < 3000 && keys.len() * 1000 <= 1024 * 1024 + 2000);
    // Following the cursor still visits every key.
    let mut seen = keys.len();
    let mut cursor = String::from_utf8(cursor.to_vec()).unwrap();
    while cursor != "0" {
        let Reply::Array(page) = f.run(&format!("SCAN {cursor} COUNT 100000")).await else {
            panic!()
        };
        let [Reply::Bulk(next), Reply::Array(keys)] = page.as_slice() else {
            panic!()
        };
        seen += keys.len();
        cursor = String::from_utf8(next.to_vec()).unwrap();
    }
    assert_eq!(seen, 3000);
}

#[tokio::test]
async fn expensive_patterns_fail_instead_of_stalling() {
    let f = fixture(1 << 30);
    let key = "a".repeat(1000);
    for i in 0..200 {
        f.run(&format!("SET {key}{i} v")).await;
    }
    let pattern = format!("*{}b", "a".repeat(999));
    for command in [
        format!("KEYS {pattern}"),
        format!("SCAN 0 MATCH {pattern} COUNT 1000"),
    ] {
        assert_eq!(
            f.run(&command).await,
            Reply::error(glob::TOO_COMPLEX),
            "{}",
            &command[..10]
        );
    }
    // Cheap patterns over the same keys still work: keys ending in 1.
    assert!(matches!(f.run("KEYS a*1").await, Reply::Array(keys) if keys.len() == 20));
}

#[tokio::test]
async fn concurrent_writers_share_commits_and_lose_nothing() {
    let f = fixture(1 << 30);
    let mut tasks = Vec::new();
    for _ in 0..64 {
        let store = f.store.clone();
        tasks.push(tokio::spawn(async move {
            for _ in 0..20 {
                store.run(vec![parse("INCR n")]).await.unwrap();
            }
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(f.run("GET n").await, bulk("1280"));
}

#[tokio::test]
async fn data_survives_reopening() {
    let f = fixture(1 << 30);
    f.run("SET k v EX 100").await;
    f.run("HSET h a 1").await;
    f.store.close().await;
    assert!(f.store.run(vec![parse("GET k")]).await.is_err());
    let reopened = Store::open(&f.path, config(&f.now, 1 << 30)).unwrap();
    let get = reopened
        .run(vec![parse("GET k"), parse("HGET h a"), parse("TTL k")])
        .await
        .unwrap();
    assert_eq!(get, vec![bulk("v"), bulk("1"), Reply::Int(100)]);
    assert_eq!(reopened.stats().await.unwrap().keys, 2);
}

#[tokio::test]
async fn scan_visits_every_key_once_the_cursor_ends() {
    let f = fixture(1 << 30);
    let mut expected = BTreeSet::new();
    for i in 0..500 {
        f.run(&format!("SET key:{i} v")).await;
        expected.insert(format!("key:{i}"));
    }
    f.run("HSET hash a b").await;
    let mut seen = BTreeSet::new();
    let mut cursor = "0".to_owned();
    let mut calls = 0;
    loop {
        calls += 1;
        let Reply::Array(parts) = f
            .run(&format!("SCAN {cursor} MATCH key:* COUNT 7 TYPE string"))
            .await
        else {
            panic!()
        };
        let [Reply::Bulk(next), Reply::Array(keys)] = parts.as_slice() else {
            panic!()
        };
        for key in keys {
            let Reply::Bulk(key) = key else { panic!() };
            seen.insert(String::from_utf8(key.to_vec()).unwrap());
        }
        cursor = String::from_utf8(next.to_vec()).unwrap();
        if cursor == "0" {
            break;
        }
    }
    assert_eq!(seen, expected);
    assert!(calls > 50, "COUNT bounds each call's work");
    let Reply::Array(all) = f.run("KEYS key:1?").await else {
        panic!()
    };
    assert_eq!(all.len(), 10);
    assert_eq!(f.run("DBSIZE").await, Reply::Int(501));
}

#[tokio::test]
async fn large_replies_are_refused() {
    let f = fixture(1 << 30);
    let value = "x".repeat(300 * 1024);
    for i in 0..4 {
        f.run(&format!("HSET h f{i} {value}")).await;
        f.run(&format!("SET s{i} {value}")).await;
    }
    assert!(matches!(f.run("HGETALL h").await, Reply::Error(m) if m.contains("exceed")));
    assert!(matches!(f.run("MGET s0 s1 s2 s3").await, Reply::Error(m) if m.contains("exceed")));
    assert_eq!(f.run("HLEN h").await, Reply::Int(4));
}
