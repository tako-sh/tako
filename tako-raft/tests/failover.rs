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
    let any = "127.0.0.1:0".parse().unwrap();
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
    let path = first.dir.join("orders.sqlite");
    tako_db::DatabaseHost::create_file(&path).unwrap();
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
    let sql = tako_db::DatabaseHost::new(Default::default());
    let db_id = tako_db::DatabaseId::parse("orders").unwrap();
    sql.register_replicated(
        db_id.clone(),
        path,
        tako_raft::Resource::new(first.db.0.clone(), first.db.1.clone(), &first.dir),
    )
    .unwrap();
    sql.set_bindings(
        "bench/production",
        vec![tako_db::Binding {
            tokens: tako_core::BindingTokenHashes::new("sql"),
            database: db_id,
            home: tako_db::Home::Local,
        }],
    );
    let listener = TcpListener::bind(any).await.unwrap();
    let url = format!("http://{}/v2/pipeline", listener.local_addr().unwrap());
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
    kv.register_replicated(
        kv_id,
        first.dir.join("sessions.redb"),
        tako_raft::Resource::new(first.kv.0.clone(), first.kv.1.clone(), &first.dir),
    )
    .unwrap();
    kv.set_bindings(
        "bench/production",
        vec![tako_kv::Binding {
            tokens: tako_core::BindingTokenHashes::new("kv"),
            store: tako_kv::StoreId::parse("sessions").unwrap(),
            home: tako_kv::Home::Local,
        }],
    );
    execute("CREATE TABLE t(n INTEGER)").await;
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
    for mode in ["single", "110/160 ms followers"] {
        if mode != "single" {
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
        }
        execute("INSERT INTO t VALUES(0)").await;
        put(0).await.unwrap();
        let mut db_ms = Vec::new();
        let mut kv_ms = Vec::new();
        for i in 1..=24 {
            let start = std::time::Instant::now();
            execute("INSERT INTO t VALUES(1)").await;
            db_ms.push(start.elapsed().as_secs_f64() * 1000.0);
            let start = std::time::Instant::now();
            put(i).await.unwrap();
            kv_ms.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        for (engine, mut ms) in [("SQLite", db_ms), ("KV", kv_ms)] {
            ms.sort_by(f64::total_cmp);
            eprintln!(
                "{mode}: {engine} median {:.2} ms, p95 {:.2} ms (24 warm writes)",
                ms[11], ms[22]
            );
        }
    }
    first.stop().await;
    second.stop().await;
    third.stop().await;
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
