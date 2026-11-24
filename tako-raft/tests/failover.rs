use openraft::async_runtime::WatchReceiver;
use openraft::{Config, Raft};
use redb::{ReadableDatabase, ReadableTable, TableDefinition};
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};
use tako_raft::{Peer, RedbFile, Replica, SqliteWal, Types, Writer};
use tokio::net::TcpListener;

const KV: TableDefinition<&str, &str> = TableDefinition::new("sessions");
type Node = Raft<Types, Replica>;

struct Server {
    peer: Peer,
    addr: SocketAddr,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
    db: (Node, Replica),
    kv: (Node, Replica),
    dir: PathBuf,
}

async fn server(id: u128, dir: &Path, address: SocketAddr) -> Server {
    let config = Config {
        heartbeat_interval: 50,
        election_timeout_min: 220,
        election_timeout_max: 450,
        max_in_snapshot_log_to_keep: 0,
        ..Default::default()
    };
    server_with_config(id, dir, address, config).await
}

async fn server_with_config(id: u128, dir: &Path, address: SocketAddr, config: Config) -> Server {
    let listener = TcpListener::bind(address).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let peer = Peer::default();
    let db = peer
        .open("orders", [7; 32], id, &dir.join("orders"), config.clone())
        .await
        .unwrap();
    let kv = peer
        .open("sessions", [8; 32], id, &dir.join("sessions"), config)
        .await
        .unwrap();
    let listener_peer = peer.clone();
    let task = tokio::spawn(async move {
        listener_peer
            .serve(listener, |addr| addr.is_loopback())
            .await
    });
    Server {
        peer,
        addr,
        task,
        db,
        kv,
        dir: dir.to_owned(),
    }
}
impl Server {
    async fn stop(self) {
        self.task.abort();
        self.peer.close("orders").await.unwrap();
        self.peer.close("sessions").await.unwrap();
    }
}

async fn writer(group: &(Node, Replica), dir: &Path) -> Writer {
    Writer::checkout(group.0.clone(), group.1.clone(), dir)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acknowledged_sql_and_kv_survive_election_snapshot_and_restart() {
    tokio::time::timeout(Duration::from_secs(35), cluster())
        .await
        .expect("cluster failed to converge");
}

async fn cluster() {
    let dir = tempfile::tempdir().unwrap();
    let any: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let first = server(1, &dir.path().join("one"), any).await;
    let second = server(2, &dir.path().join("two"), any).await;
    let third = server(3, &dir.path().join("three"), any).await;
    let mut placement = BTreeMap::from([(1, first.addr)]);
    for node in [&first.db.0, &first.kv.0] {
        node.initialize(placement.clone()).await.unwrap();
        node.wait(Some(Duration::from_secs(3)))
            .current_leader(1, "bootstrap")
            .await
            .unwrap();
    }
    let sq = writer(&first.db, &first.dir).await;
    let value = tokio::task::spawn_blocking(move || {
        let mut conn = rusqlite::Connection::open(sq.path()).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL").unwrap();
        let wal = SqliteWal::new(sq);
        wal.install(&mut conn).unwrap();
        let _guard = wal.enter().unwrap();
        conn.execute_batch("CREATE TABLE orders(id INTEGER PRIMARY KEY, item TEXT); BEGIN;")
            .unwrap();
        let value: String = conn
            .query_row(
                "INSERT INTO orders(item) VALUES(hex(randomblob(16))) RETURNING item",
                [],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute_batch(
            "SAVEPOINT s; INSERT INTO orders(item) VALUES('discard'); ROLLBACK TO s; COMMIT;",
        )
        .unwrap();
        value
    })
    .await
    .unwrap();
    let mut kv = writer(&first.kv, &first.dir).await;
    tokio::task::spawn_blocking(move || {
        let file = RedbFile::open(&kv).unwrap();
        let db = redb::Database::builder()
            .create_with_backend(file.clone())
            .unwrap();
        let tx = db.begin_write().unwrap();
        tx.open_table(KV)
            .unwrap()
            .insert("token", "issued")
            .unwrap();
        tx.commit().unwrap();
        file.publish(&mut kv).unwrap();
    })
    .await
    .unwrap();
    placement.insert(2, second.addr);
    for node in [&first.db.0, &first.kv.0] {
        node.trigger().snapshot().await.unwrap();
        tako_raft::reconcile(node, &placement).await.unwrap();
        assert_eq!(
            node.metrics()
                .borrow_watched()
                .membership_config
                .membership()
                .voter_ids()
                .count(),
            2
        );
    }
    placement.insert(3, third.addr);
    for node in [&first.db.0, &first.kv.0] {
        tako_raft::reconcile(node, &placement).await.unwrap();
    }
    for target in placement.values() {
        for generation in ["orders", "sessions"] {
            first
                .peer
                .wait_ready(generation, *target, |_| {})
                .await
                .unwrap();
        }
    }
    follower_read_only_sql(&second, &value).await;
    let addr = first.addr;
    first.stop().await;
    let db_leader = await_leader(&second.db.0).await;
    let kv_leader = await_leader(&second.kv.0).await;
    let db_server = if db_leader == 2 { &second } else { &third };
    let kv_server = if kv_leader == 2 { &second } else { &third };
    let sq = writer(&db_server.db, &db_server.dir).await;
    let kv = writer(&kv_server.kv, &kv_server.dir).await;
    let value_after = value.clone();
    tokio::task::spawn_blocking(move || {
        let mut conn = rusqlite::Connection::open(sq.path()).unwrap();
        let wal = SqliteWal::new(sq);
        wal.install(&mut conn).unwrap();
        let _guard = wal.enter().unwrap();
        let (count, item): (i32, String) = conn
            .query_row("SELECT count(*),item FROM orders", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((count, item), (1, value_after));
        conn.execute_batch("INSERT INTO orders(item) VALUES('after election')")
            .unwrap();
        let db = redb::Database::open(kv.path()).unwrap();
        assert_eq!(
            db.begin_read()
                .unwrap()
                .open_table(KV)
                .unwrap()
                .get("token")
                .unwrap()
                .unwrap()
                .value(),
            "issued"
        );
    })
    .await
    .unwrap();
    let first = server(1, &dir.path().join("one"), addr).await;
    let revision = db_server.db.1.revision().await.unwrap();
    while first.db.1.revision().await.unwrap() != revision {
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    let (local, _) = first.db.1.checkout(&first.dir).await.unwrap();
    assert_eq!(
        rusqlite::Connection::open(local.path())
            .unwrap()
            .query_row("SELECT count(*) FROM orders", [], |r| r.get::<_, i32>(0))
            .unwrap(),
        2
    );
    first.stop().await;
    second.stop().await;
    third.stop().await;
}

async fn follower_read_only_sql(follower: &Server, expected: &str) {
    let host = tako_db::DatabaseHost::new(Default::default());
    let database = tako_db::DatabaseId::parse("orders").unwrap();
    let path = follower.dir.join("orders.sqlite");
    tako_db::DatabaseHost::create_file(&path).unwrap();
    host.register_replicated(
        database.clone(),
        path,
        tako_raft::Resource::new(follower.db.0.clone(), follower.db.1.clone(), &follower.dir),
    )
    .unwrap();
    let tokens = tako_core::BindingTokenHashes::new("follower-sql");
    let replicas = [tokens.replica].into();
    host.set_bindings_with_replicas(
        "app/production",
        vec![tako_db::Binding {
            database,
            tokens,
            home: tako_db::Home::Unavailable {
                reason: "no route to leader".into(),
            },
        }],
        &replicas,
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v2/pipeline", listener.local_addr().unwrap());
    let task = tokio::spawn(tako_db::http::serve(
        host,
        listener,
        tako_db::http::Access::Apps,
    ));
    let body: serde_json::Value = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(url)
        .bearer_auth(tako_core::replica_token("follower-sql"))
        .json(&serde_json::json!({"requests":[
            {"type":"execute","stmt":{"sql":"SELECT item FROM orders"}},
            {"type":"execute","stmt":{"sql":"DELETE FROM orders"}},
            {"type":"close"}
        ]}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        body["results"][0]["response"]["result"]["rows"][0][0]["value"], expected,
        "{body}"
    );
    assert_eq!(
        body["results"][1]["error"]["code"], "SQLITE_READONLY",
        "{body}"
    );
    task.abort();
}

async fn await_leader(raft: &Node) -> u128 {
    loop {
        if let Some(id @ (2 | 3)) = raft.current_leader().await {
            return id;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Delay response bytes by a fixed interval, preserving bandwidth and socket
/// reuse. Both requests and heartbeats traverse the same forwarded connection.
async fn delayed_peer(target: SocketAddr, delay: Duration) -> SocketAddr {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            tokio::spawn(async move {
                let upstream = tokio::net::TcpStream::connect(target).await?;
                client.set_nodelay(true)?;
                upstream.set_nodelay(true)?;
                let (mut input, mut output) = client.into_split();
                let (mut reply, mut request) = upstream.into_split();
                let (tx, mut rx) = tokio::sync::mpsc::channel(64);
                let send = async {
                    let mut buf = [0; 8192];
                    loop {
                        let n = reply.read(&mut buf).await?;
                        if n == 0 {
                            break;
                        }
                        if tx
                            .send((tokio::time::Instant::now() + delay, buf[..n].to_vec()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok::<(), std::io::Error>(())
                };
                let receive = async {
                    while let Some((when, bytes)) = rx.recv().await {
                        tokio::time::sleep_until(when).await;
                        output.write_all(&bytes).await?;
                    }
                    Ok::<(), std::io::Error>(())
                };
                tokio::select! {
                    result=tokio::io::copy(&mut input,&mut request)=>result.map(|_|()),
                    result=send=>result,
                    result=receive=>result,
                }
            });
        }
    });
    addr
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
#[ignore = "timed WAN simulation; run explicitly with --ignored --nocapture"]
async fn measure_hosted_mutations() {
    let root = tempfile::tempdir().unwrap();
    let config = Config {
        heartbeat_interval: 350,
        election_timeout_min: 1600,
        election_timeout_max: 3000,
        ..Default::default()
    };
    let any: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let first = server_with_config(1, &root.path().join("a"), any, config.clone()).await;
    let second = server_with_config(2, &root.path().join("b"), any, config.clone()).await;
    let third = server_with_config(3, &root.path().join("c"), any, config).await;
    let members = BTreeMap::from([(1, first.addr)]);
    for node in [&first.db.0, &first.kv.0] {
        node.initialize(members.clone()).await.unwrap();
        node.wait(Some(Duration::from_secs(6)))
            .current_leader(1, "initial leader")
            .await
            .unwrap();
    }
    hosted_trial(&first, false, false, "current single-server host").await;
    hosted_trial(&first, true, false, "one voter").await;
    let mut targets = members.clone();
    targets.insert(
        2,
        delayed_peer(second.addr, Duration::from_millis(110)).await,
    );
    targets.insert(
        3,
        delayed_peer(third.addr, Duration::from_millis(160)).await,
    );
    for node in [&first.db.0, &first.kv.0] {
        tako_raft::reconcile(node, &targets).await.unwrap();
    }
    hosted_trial(&first, true, false, "110/160 ms followers").await;
    hosted_trial(&first, true, true, "150 ms app hop + 110/160 ms followers").await;
    first.stop().await;
    second.stop().await;
    third.stop().await;
}

async fn hosted_trial(first: &Server, replicated: bool, remote_app: bool, mode: &str) {
    let any: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let path = first.dir.join(if replicated {
        "orders.sqlite"
    } else {
        "baseline.sqlite"
    });
    if !path.exists() {
        tako_db::DatabaseHost::create_file(&path).unwrap();
    }
    if replicated && first.db.1.revision().await.unwrap() == 0 {
        let mut seed = writer(&first.db, &first.dir).await;
        let image = std::fs::read(&path).unwrap();
        tokio::task::spawn_blocking(move || {
            seed.publish(
                4096,
                image.len() as u64,
                image
                    .chunks_exact(4096)
                    .enumerate()
                    .map(|(i, p)| (i as u64, p.to_vec()))
                    .collect(),
            )
            .unwrap();
        })
        .await
        .unwrap();
    }
    let sql = tako_db::DatabaseHost::new(Default::default());
    let db_id = tako_db::DatabaseId::parse("orders").unwrap();
    if replicated {
        sql.register_replicated(
            db_id.clone(),
            path,
            tako_raft::Resource::new(first.db.0.clone(), first.db.1.clone(), &first.dir),
        )
        .unwrap();
    } else {
        sql.register(db_id.clone(), path).unwrap();
    }
    sql.set_bindings(
        "bench/production",
        vec![tako_db::Binding {
            tokens: tako_core::BindingTokenHashes::new("sql"),
            database: db_id,
            home: tako_db::Home::Local,
        }],
    );
    let listener = TcpListener::bind(any).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let addr = if remote_app {
        delayed_peer(addr, Duration::from_millis(150)).await
    } else {
        addr
    };
    let url = format!("http://{addr}/v2/pipeline");
    tokio::spawn(tako_db::http::serve(
        sql,
        listener,
        tako_db::http::Access::Apps,
    ));
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let execute = |sql: &'static str| {
        let request=client.post(&url).bearer_auth("sql").json(&serde_json::json!({"requests":[{"type":"execute","stmt":{"sql":sql}},{"type":"close"}]}));
        async move {
            let result: serde_json::Value = request
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(result["results"][0]["type"], "ok", "{result}");
        }
    };
    let kv = tako_kv::KvHost::new(Default::default());
    let kv_id = tako_kv::StoreId::parse("sessions").unwrap();
    if replicated {
        kv.register_replicated(
            kv_id,
            first.dir.join("sessions.redb"),
            tako_raft::Resource::new(first.kv.0.clone(), first.kv.1.clone(), &first.dir),
        )
        .unwrap();
    } else {
        kv.register(kv_id, first.dir.join("baseline.redb")).unwrap();
    }
    kv.set_bindings(
        "bench/production",
        vec![tako_kv::Binding {
            tokens: tako_core::BindingTokenHashes::new("kv"),
            store: tako_kv::StoreId::parse("sessions").unwrap(),
            home: tako_kv::Home::Local,
        }],
    );
    execute("CREATE TABLE IF NOT EXISTS t(n INTEGER)").await;
    let put = |i| {
        let kv = kv.clone();
        tokio::task::spawn_blocking(move || {
            kv.call(
                "bench/production",
                "kv",
                tako_core::KvRequest::Put {
                    key: "k".into(),
                    json: format!("{i}"),
                    ttl_ms: None,
                },
            )
            .unwrap()
        })
    };
    execute("INSERT INTO t VALUES(0)").await;
    if !remote_app {
        put(0).await.unwrap();
    }
    let mut db_ms = Vec::new();
    let mut kv_ms = Vec::new();
    for i in 1..=24 {
        let start = std::time::Instant::now();
        execute("INSERT INTO t VALUES(1)").await;
        db_ms.push(start.elapsed().as_secs_f64() * 1000.0);
        if remote_app {
            continue;
        }
        let start = std::time::Instant::now();
        put(i).await.unwrap();
        kv_ms.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    for (engine, mut ms) in [("SQLite", db_ms), ("KV", kv_ms)] {
        if ms.is_empty() {
            continue;
        }
        ms.sort_by(f64::total_cmp);
        eprintln!(
            "{mode}: {engine} median {:.2} ms, p95 {:.2} ms (24 warm writes)",
            ms[11], ms[22]
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn concurrent_first_deploys_reuse_the_winning_bootstrap() {
    let dir = tempfile::tempdir().unwrap();
    let node = server(1, dir.path(), "127.0.0.1:0".parse().unwrap()).await;
    let (catalog, _) = node
        .peer
        .open(
            "catalog",
            [1; 32],
            1,
            &dir.path().join("catalog"),
            Config::default(),
        )
        .await
        .unwrap();
    catalog
        .initialize(BTreeMap::from([(1, node.addr)]))
        .await
        .unwrap();
    catalog
        .wait(Some(Duration::from_secs(4)))
        .current_leader(1, "catalog election")
        .await
        .unwrap();
    let candidate = |digit: &str| tako_raft::Claim {
        name: "orders".into(),
        engine: tako_core::DatabaseEngine::Sqlite,
        generation: digit.repeat(32),
        created_at_unix_secs: 42,
        channel_owner: None,
        workflows_owner: None,
        founders: [digit.repeat(64)].into_iter().collect(),
    };
    let (a, b) = tokio::join!(
        catalog.client_write(tako_raft::Proposal::Claim(candidate("a"))),
        catalog.client_write(tako_raft::Proposal::Claim(candidate("b")))
    );
    let winner = a.unwrap().data;
    assert_eq!(winner, b.unwrap().data);
    assert!(matches!(winner, tako_raft::Applied::Claimed(_)));
    let mut collision = candidate("c");
    collision.engine = tako_core::DatabaseEngine::Kv;
    assert_eq!(
        catalog
            .client_write(tako_raft::Proposal::Claim(collision))
            .await
            .unwrap()
            .data,
        tako_raft::Applied::EngineConflict
    );
    node.peer.close("catalog").await.unwrap();
    node.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_voters_require_both_and_a_large_commit_is_atomic() {
    let dir = tempfile::tempdir().unwrap();
    let any = "127.0.0.1:0".parse().unwrap();
    // Use the server's election and RPC timeouts while sending real page images.
    let config = Config {
        heartbeat_interval: 450,
        election_timeout_min: 1600,
        election_timeout_max: 3000,
        ..Default::default()
    };
    let one = server_with_config(1, &dir.path().join("one"), any, config.clone()).await;
    let two = server_with_config(2, &dir.path().join("two"), any, config).await;
    let mut members = BTreeMap::from([(1, one.addr)]);
    one.db.0.initialize(members.clone()).await.unwrap();
    one.db
        .0
        .wait(Some(Duration::from_secs(5)))
        .current_leader(1, "initial election")
        .await
        .unwrap();
    members.insert(2, two.addr);
    tako_raft::add_members(&one.db.0, members).await.unwrap();
    let mut w = writer(&one.db, &one.dir).await;
    tokio::task::spawn_blocking(move || {
        let pages = (0..2304)
            .map(|i| (i, vec![(i % 251) as u8; 4096]))
            .collect();
        w.publish(4096, 2304 * 4096, pages).unwrap();
    })
    .await
    .unwrap();
    one.peer
        .wait_ready("orders", two.addr, |_| {})
        .await
        .unwrap();
    let (file, rev) = two.db.1.checkout(&two.dir).await.unwrap();
    let data = std::fs::read(file.path()).unwrap();
    assert_eq!(data.len(), 2304 * 4096);
    assert!(
        data.chunks_exact(4096)
            .enumerate()
            .all(|(i, p)| p.iter().all(|b| *b == (i % 251) as u8))
    );
    follower_read_only_kv(&one, &two).await;
    two.stop().await;
    let pending = one
        .db
        .0
        .client_write(tako_raft::Proposal::Pages(tako_raft::Commit {
            replace: false,
            base_revision: rev,
            captured_at_ms: 1,
            page_size: 4096,
            file_size: data.len() as u64,
            pages: BTreeMap::from([(0, vec![253; 4096])]),
        }));
    if let Ok(Ok(reply)) = tokio::time::timeout(Duration::from_millis(500), pending).await {
        panic!("a single surviving voter acknowledged {reply:?}");
    }
    assert_eq!(one.db.1.revision().await.unwrap(), rev);
    one.stop().await;
}

async fn follower_read_only_kv(leader: &Server, follower: &Server) {
    const APP: &str = "app/production";
    use tako_core::KvRequest;
    use tako_kv::{Binding, Home, KvHost, StoreId};
    leader
        .kv
        .0
        .initialize(BTreeMap::from([(1, leader.addr)]))
        .await
        .unwrap();
    leader
        .kv
        .0
        .wait(Some(Duration::from_secs(5)))
        .current_leader(1, "KV boot")
        .await
        .unwrap();
    tako_raft::add_members(
        &leader.kv.0,
        BTreeMap::from([(1, leader.addr), (2, follower.addr)]),
    )
    .await
    .unwrap();
    let register = |node: &Server, home: Home| {
        let host = KvHost::new(Default::default());
        let store = StoreId::parse("sessions").unwrap();
        host.register_replicated(
            store.clone(),
            node.dir.join("sessions.redb"),
            tako_raft::Resource::new(node.kv.0.clone(), node.kv.1.clone(), &node.dir),
        )
        .unwrap();
        let tokens = tako_core::BindingTokenHashes::new("access");
        let replicas = [tokens.replica].into();
        host.set_bindings_with_replicas(
            APP,
            vec![Binding {
                tokens,
                store,
                home,
            }],
            &replicas,
        );
        host
    };
    let primary = register(leader, Home::Local);
    let local = register(
        follower,
        Home::Unavailable {
            reason: "no route to leader".into(),
        },
    );
    tokio::task::spawn_blocking(move || {
        primary
            .call(
                APP,
                "access",
                KvRequest::Put {
                    key: "session".into(),
                    json: "42".into(),
                    ttl_ms: None,
                },
            )
            .unwrap()
    })
    .await
    .unwrap();
    leader
        .peer
        .wait_ready("sessions", follower.addr, |_| {})
        .await
        .unwrap();
    tokio::task::spawn_blocking(move || {
        let token = tako_core::replica_token("access");
        let result = local
            .call(
                APP,
                &token,
                KvRequest::Get {
                    key: "session".into(),
                },
            )
            .unwrap();
        assert_eq!(result, "42");
        assert!(
            local
                .call(
                    APP,
                    &token,
                    KvRequest::Delete {
                        key: "session".into()
                    }
                )
                .is_err()
        );
    })
    .await
    .unwrap();
}
