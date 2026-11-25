use openraft::async_runtime::WatchReceiver;
use openraft::{Config, Raft};
use redb::{ReadableDatabase, ReadableTable, TableDefinition};
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};
use tako_raft::{Peer, RedbFile, Replica, Types, Writer};
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
    let (sql, dbid) = sql_host(&first);
    let create = "CREATE TABLE orders(id INTEGER PRIMARY KEY, item TEXT)";
    let migration = tako_db::Migration {
        name: "0001_orders.sql".into(),
        sql: format!(
            "{create}; SAVEPOINT trial; INSERT INTO orders(item) VALUES ('discard'); ROLLBACK TO trial; RELEASE trial;"
        ),
    };
    let report = sql
        .migrate(&dbid, "shop".into(), vec![migration.clone()], false)
        .await
        .unwrap();
    assert_eq!(report.applied, [migration.name.clone()]);
    assert!(
        sql.migrate(&dbid, "shop".into(), vec![migration], false)
            .await
            .unwrap()
            .applied
            .is_empty()
    );
    let row = sql
        .admin_query(
            &dbid,
            "INSERT INTO orders(item) VALUES(hex(randomblob(16))) RETURNING item".into(),
            1,
        )
        .await
        .unwrap();
    let tako_db::Value::Text(value) = row.rows[0][0].clone() else {
        panic!("expected generated item")
    };
    sql.close_all().await;
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
    let (sql, dbid) = sql_host(db_server);
    let result = sql
        .admin_query(&dbid, "SELECT item FROM orders".into(), 10)
        .await
        .unwrap();
    assert!(
        matches!(result.rows.as_slice(),[r] if matches!(r.as_slice(),[tako_db::Value::Text(t)] if t==&value))
    );
    sql.admin_query(
        &dbid,
        "INSERT INTO orders(item) VALUES('after election')".into(),
        1,
    )
    .await
    .unwrap();
    sql.close_all().await;
    let kv = writer(&kv_server.kv, &kv_server.dir).await;
    tokio::task::spawn_blocking(move || {
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
    tako_db::materialize_recovery_image(local.path()).unwrap();
    assert_eq!(
        tako_db::testing::TestSql::open(local.path())
            .integers("SELECT count(*) FROM orders", &[])
            .unwrap(),
        [2]
    );
    first.stop().await;
    second.stop().await;
    third.stop().await;
}

fn sql_host(follower: &Server) -> (tako_db::DatabaseHost, tako_db::DatabaseId) {
    let host = tako_db::DatabaseHost::new(Default::default());
    let database = tako_db::DatabaseId::parse("orders").unwrap();
    let path = follower.dir.join("orders.db");
    if !path.exists() {
        tako_db::DatabaseHost::create_file(&path).unwrap();
    }
    host.register_replicated(
        database.clone(),
        path,
        tako_raft::Resource::new(follower.db.0.clone(), follower.db.1.clone(), &follower.dir),
    )
    .unwrap();
    (host, database)
}

async fn follower_read_only_sql(follower: &Server, expected: &str) {
    let (host, database) = sql_host(follower);
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
        "orders.db"
    } else {
        "baseline.db"
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
    for (engine, mut ms) in [("SQL", db_ms), ("KV", kv_ms)] {
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
    let (catalog, replica) = node
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
    let publish = |candidate: tako_raft::Claim| tako_raft::Proposal::PublishClaim {
        shared: candidate.generation.as_bytes().to_vec(),
        candidate,
        expected: 0,
    };
    let (a, b) = tokio::join!(
        node.peer.catalog_write(publish(candidate("a"))),
        node.peer.catalog_write(publish(candidate("b")))
    );
    let winner = a.unwrap();
    assert_eq!(winner, b.unwrap());
    let tako_raft::Applied::Claimed(winner) = winner else {
        panic!("no winner");
    };
    let publication = replica.catalog_document(true).await.unwrap();
    assert_eq!(publication, (1, winner.generation.as_bytes().to_vec()));
    let mut collision = candidate("c");
    collision.engine = tako_core::DatabaseEngine::Kv;
    assert_eq!(
        catalog.client_write(publish(collision)).await.unwrap().data,
        tako_raft::Applied::EngineConflict
    );
    assert_eq!(replica.catalog_document(true).await.unwrap(), publication);
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catalog_recovers_after_founder_loss_and_demotes_before_removal() {
    async fn run() {
        let dir = tempfile::tempdir().unwrap();
        let mut replicas = Vec::new();
        for id in 1..=3 {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let peer = Peer::with_peer_keys(|ip| ip.is_loopback().then_some([37; 32]));
            let config = Config {
                heartbeat_interval: 50,
                election_timeout_min: 220,
                election_timeout_max: 450,
                ..Default::default()
            };
            let (node, replica) = peer
                .open(
                    "catalog",
                    [0; 32],
                    id,
                    &dir.path().join(id.to_string()),
                    config,
                )
                .await
                .unwrap();
            let server = peer.clone();
            let task =
                tokio::spawn(async move { server.serve(listener, |ip| ip.is_loopback()).await });
            replicas.push((peer, node, replica, address, task));
        }
        let (peer, leader, _, addr, _) = &replicas[0];
        leader
            .initialize(BTreeMap::from([(1, *addr)]))
            .await
            .unwrap();
        leader
            .wait(Some(Duration::from_secs(4)))
            .current_leader(1, "founder")
            .await
            .unwrap();
        let members = replicas
            .iter()
            .zip(1..)
            .map(|(replica, id)| (id, replica.3))
            .collect();
        tako_raft::reconcile(leader, &members).await.unwrap();
        peer.wait_ready("catalog", replicas[1].3, |_| {})
            .await
            .unwrap();
        let update = |expected, bytes: &[u8]| tako_raft::Proposal::CatalogUpdate {
            shared: false,
            expected,
            value: bytes.to_vec(),
        };
        assert_eq!(
            replicas[1]
                .0
                .catalog_write(update(0, b"first"))
                .await
                .unwrap(),
            tako_raft::Applied::Committed(1)
        );
        assert_eq!(
            replicas[1].2.catalog_document(false).await.unwrap(),
            (1, b"first".to_vec())
        );
        // An enrolled overlay IP still needs its catalog credential.
        let untrusted = Peer::with_peer_keys(|_| Some([38; 32]));
        untrusted
            .open(
                "catalog",
                [0; 32],
                4,
                &dir.path().join("unauthorized"),
                Config::default(),
            )
            .await
            .unwrap();
        assert!(untrusted.announcement(*addr).await.is_err());
        untrusted.close("catalog").await.unwrap();
        replicas[0].4.abort();
        replicas[0].0.close("catalog").await.unwrap();
        let id = await_leader(&replicas[1].1).await;
        let live = &replicas[id as usize - 1];
        assert_eq!(
            live.0
                .catalog_write(update(1, b"after election"))
                .await
                .unwrap(),
            tako_raft::Applied::Committed(2)
        );
        let mut survivors = members;
        survivors.remove(&1);
        tako_raft::reconcile(&live.1, &survivors).await.unwrap();
        // Two voting nodes must both acknowledge their transition to one.
        let other = if id == 2 { 3 } else { 2 };
        let retiring = &replicas[other - 1];
        survivors.remove(&(other as u128));
        tako_raft::reconcile_voters(&live.1, &survivors)
            .await
            .unwrap();
        assert!(
            live.1
                .metrics()
                .borrow_watched()
                .membership_config
                .membership()
                .get_node(&(other as u128))
                .is_some()
        );
        assert_eq!(
            live.0.catalog_write(update(2, b"revoked")).await.unwrap(),
            tako_raft::Applied::Committed(3)
        );
        live.0
            .wait_ready("catalog", retiring.3, |_| {})
            .await
            .unwrap();
        assert_eq!(
            retiring.2.catalog_document(false).await.unwrap(),
            (3, b"revoked".to_vec())
        );
        tako_raft::reconcile(&live.1, &survivors).await.unwrap();
        retiring.4.abort();
        retiring.0.close("catalog").await.unwrap();
        assert!(live.0.committed_position("catalog").await.is_ok());
        live.4.abort();
        live.0.close("catalog").await.unwrap();
    }
    tokio::time::timeout(Duration::from_secs(30), run())
        .await
        .expect("catalog failed to converge");
}
