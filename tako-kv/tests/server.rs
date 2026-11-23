//! The server, driven by a Redis client and by raw protocol bytes.

use std::collections::{BTreeSet, HashMap};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use redis::AsyncCommands;
use tako_kv::{Access, Binding, Home, HostConfig, KvHost, StoreId, hash_token};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

struct Server {
    _dir: tempfile::TempDir,
    host: KvHost,
    addr: SocketAddr,
}

async fn listen(host: &KvHost, access: Access) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(tako_kv::serve(host.clone(), listener, access));
    addr
}

/// A host serving store `main` to an app holding `token`.
async fn server(token: &str) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let host = KvHost::new(HostConfig::default());
    let id = StoreId::parse("main").unwrap();
    host.register(id.clone(), dir.path().join("main.redb"))
        .unwrap();
    host.set_bindings(
        "app/production",
        vec![Binding {
            token_hash: hash_token(token),
            store: id,
            home: Home::Local,
        }],
    );
    let addr = listen(&host, Access::Apps).await;
    Server {
        _dir: dir,
        host,
        addr,
    }
}

async fn client(addr: SocketAddr, token: &str, resp3: bool) -> redis::aio::MultiplexedConnection {
    let protocol = if resp3 { "?protocol=resp3" } else { "" };
    let url = format!("redis://:{token}@{addr}/{protocol}");
    redis::Client::open(url)
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap()
}

/// Sends raw bytes and reads until `expected` bytes arrive or the server
/// stops sending.
async fn raw(stream: &mut TcpStream, send: &[u8], expected: usize) -> Vec<u8> {
    stream.write_all(send).await.unwrap();
    let mut got = Vec::new();
    while got.len() < expected {
        let mut chunk = [0u8; 4096];
        match tokio::time::timeout(Duration::from_secs(2), stream.read(&mut chunk)).await {
            Ok(Ok(n)) if n > 0 => got.extend_from_slice(&chunk[..n]),
            _ => break,
        }
    }
    got
}

#[tokio::test]
async fn redis_clients_work_over_both_protocols() {
    let s = server("secret").await;
    for resp3 in [false, true] {
        let mut kv = client(s.addr, "secret", resp3).await;
        let _: () = kv.set_ex("challenge:1", "abc", 300).await.unwrap();
        let value: Option<String> = kv.get("challenge:1").await.unwrap();
        assert_eq!(value.as_deref(), Some("abc"));
        let ttl: i64 = kv.ttl("challenge:1").await.unwrap();
        assert_eq!(ttl, 300);
        // Consuming a challenge is atomic: only one GETDEL sees it.
        let first: Option<String> = kv.get_del("challenge:1").await.unwrap();
        let second: Option<String> = kv.get_del("challenge:1").await.unwrap();
        assert_eq!((first.as_deref(), second), (Some("abc"), None));
        let set: bool = redis::cmd("SET")
            .arg("lock")
            .arg("me")
            .arg("NX")
            .arg("PX")
            .arg(1000)
            .query_async::<Option<String>>(&mut kv)
            .await
            .unwrap()
            .is_some();
        assert!(set);
        let _: () = kv
            .hset_multiple("user:1", &[("name", "Ada"), ("role", "admin")])
            .await
            .unwrap();
        let user: HashMap<String, String> = kv.hgetall("user:1").await.unwrap();
        assert_eq!(user["name"], "Ada");
        let (count, missing): (i64, Option<String>) = redis::pipe()
            .incr("hits", 1)
            .get("nothing")
            .query_async(&mut kv)
            .await
            .unwrap();
        assert!(count >= 1);
        assert_eq!(missing, None);
        let (a, b): (i64, i64) = redis::pipe()
            .atomic()
            .incr("tx", 1)
            .incr("tx", 1)
            .query_async(&mut kv)
            .await
            .unwrap();
        assert_eq!(b, a + 1);
        let _: () = redis::cmd("FLUSHDB").query_async(&mut kv).await.unwrap();
    }
}

#[tokio::test]
async fn wrong_tokens_are_refused() {
    let s = server("secret").await;
    let result = redis::Client::open(format!("redis://:nope@{}/", s.addr))
        .unwrap()
        .get_multiplexed_async_connection()
        .await;
    assert!(result.is_err());
    let mut stream = TcpStream::connect(s.addr).await.unwrap();
    let reply = raw(
        &mut stream,
        b"PING\r\nGET k\r\nAUTH nope\r\nHELLO 3\r\n",
        60,
    )
    .await;
    let text = String::from_utf8(reply).unwrap();
    assert!(text.starts_with(
        "-NOAUTH Authentication required.\r\n-NOAUTH Authentication required.\r\n-WRONGPASS"
    ));
    assert!(text.contains("-NOAUTH HELLO must be called"));
}

#[tokio::test]
async fn raw_protocol_matches_redis() {
    let s = server("secret").await;
    let mut stream = TcpStream::connect(s.addr).await.unwrap();
    let cases: &[(&str, &str)] = &[
        ("AUTH default secret\r\n", "+OK\r\n"),
        ("SET \"a b\" 1\r\n", "+OK\r\n"),
        ("*2\r\n$3\r\nGET\r\n$3\r\na b\r\n", "$1\r\n1\r\n"),
        (
            "MULTI\r\nSET x 1\r\nGET\r\nEXEC\r\n",
            "+OK\r\n+QUEUED\r\n-ERR wrong number of arguments for 'get' command\r\n-EXECABORT Transaction discarded because of previous errors.\r\n",
        ),
        (
            "MULTI\r\nINCR x\r\nSET x 1 NX XX\r\nPING\r\nEXEC\r\n",
            "+OK\r\n+QUEUED\r\n+QUEUED\r\n+QUEUED\r\n*3\r\n:1\r\n-ERR syntax error\r\n+PONG\r\n",
        ),
        (
            "EXEC\r\nDISCARD\r\n",
            "-ERR EXEC without MULTI\r\n-ERR DISCARD without MULTI\r\n",
        ),
        (
            "SELECT 1\r\nSELECT 0\r\n",
            "-ERR DB index is out of range\r\n+OK\r\n",
        ),
        (
            "FOO bar\r\n",
            "-ERR unknown command 'FOO', with args beginning with: 'bar' \r\n",
        ),
        (
            "COMMAND INFO get nosuch\r\n",
            "*2\r\n*10\r\n$3\r\nget\r\n:2\r\n*1\r\n+readonly\r\n:1\r\n:1\r\n:1\r\n*2\r\n+@read\r\n+@string\r\n*0\r\n*0\r\n*0\r\n$-1\r\n",
        ),
        ("HELLO 3\r\n", "%7\r\n"),
    ];
    for (send, expected) in cases {
        let got = raw(&mut stream, send.as_bytes(), expected.len()).await;
        assert_eq!(
            String::from_utf8_lossy(&got[..expected.len().min(got.len())]),
            *expected,
            "{send}"
        );
        // Drain the rest of a longer reply such as HELLO's.
        if got.len() == expected.len() && send.starts_with("HELLO") {
            let _ = raw(&mut stream, b"", 1000).await;
        }
    }
    // RESP3 now: nulls and maps change shape.
    let got = raw(
        &mut stream,
        b"GET missing\r\nHSET h f v\r\nHGETALL h\r\n",
        30,
    )
    .await;
    assert_eq!(got, b"_\r\n:1\r\n%1\r\n$1\r\nf\r\n$1\r\nv\r\n");
    // A protocol error closes the connection after its reply.
    let got = raw(&mut stream, b"*1\r\n$x\r\n", 100).await;
    assert!(String::from_utf8_lossy(&got).starts_with("-ERR Protocol error"));
    assert!(raw(&mut stream, b"PING\r\n", 1).await.is_empty());
}

#[tokio::test]
async fn requests_are_limited_before_and_after_authentication() {
    let s = server("secret").await;
    // Before AUTH, requests stay as small as Redis keeps them.
    let mut stream = TcpStream::connect(s.addr).await.unwrap();
    let got = raw(&mut stream, b"*11\r\n", 100).await;
    assert_eq!(
        String::from_utf8_lossy(&got),
        "-ERR Protocol error: unauthenticated multibulk length\r\n"
    );
    let mut stream = TcpStream::connect(s.addr).await.unwrap();
    let got = raw(&mut stream, b"*2\r\n$3\r\nGET\r\n$20000\r\n", 100).await;
    assert_eq!(
        String::from_utf8_lossy(&got),
        "-ERR Protocol error: unauthenticated bulk length\r\n"
    );
    // A large request pipelined right after AUTH is read under the full limit.
    let mut stream = TcpStream::connect(s.addr).await.unwrap();
    let mut send = b"AUTH secret\r\n".to_vec();
    tako_kv::encode_request(&[b"SET".as_slice(), b"k", &[b'v'; 100_000]], &mut send);
    tako_kv::encode_request(&["STRLEN", "k"], &mut send);
    let got = raw(&mut stream, &send, 18).await;
    assert_eq!(String::from_utf8_lossy(&got), "+OK\r\n+OK\r\n:100000\r\n");
    // A request whose arguments need more than the base allowance, all in
    // one read, waits for the budget and is then parsed again.
    let pairs: Vec<String> = (0..5000)
        .flat_map(|i| [format!("m{i}"), "v".into()])
        .collect();
    let mut mset = vec!["MSET".to_owned()];
    mset.extend(pairs);
    let mut request = Vec::new();
    tako_kv::encode_request(&mset, &mut request);
    tako_kv::encode_request(&["EXISTS", "m0", "m4999"], &mut request);
    let got = raw(&mut stream, &request, 9).await;
    assert_eq!(String::from_utf8_lossy(&got), "+OK\r\n:2\r\n");
    // Arguments count against the limit even when they are empty.
    let got = raw(&mut stream, b"*1000000\r\n", 100).await;
    assert_eq!(
        String::from_utf8_lossy(&got),
        "-ERR Protocol error: request exceeds 33554432 bytes\r\n"
    );
}

#[tokio::test]
async fn transactions_are_limited_in_size() {
    let dir = tempfile::tempdir().unwrap();
    let host = KvHost::new(HostConfig {
        max_request_bytes: 4096,
        ..HostConfig::default()
    });
    let id = StoreId::parse("main").unwrap();
    host.register(id.clone(), dir.path().join("main.redb"))
        .unwrap();
    host.set_bindings(
        "app/production",
        vec![Binding {
            token_hash: hash_token("secret"),
            store: id,
            home: Home::Local,
        }],
    );
    let addr = listen(&host, Access::Apps).await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    // Each SET is charged about 1,069 bytes: three fit.
    let value = "x".repeat(1000);
    let mut send = String::from("AUTH secret\r\nMULTI\r\n");
    for _ in 0..5 {
        send.push_str(&format!("SET k {value}\r\n"));
    }
    send.push_str("EXEC\r\nEXISTS k\r\n");
    let got = String::from_utf8(raw(&mut stream, send.as_bytes(), 200).await).unwrap();
    assert!(got.starts_with("+OK\r\n+OK\r\n+QUEUED\r\n+QUEUED\r\n+QUEUED\r\n-ERR transactions are limited to 4096 bytes\r\n"), "{got}");
    assert!(
        got.ends_with("-EXECABORT Transaction discarded because of previous errors.\r\n:0\r\n"),
        "{got}"
    );
}

/// A host serving `stores`, each to an app holding the token of the same
/// name.
async fn host_with(config: HostConfig, stores: &[&str]) -> (tempfile::TempDir, SocketAddr) {
    let dir = tempfile::tempdir().unwrap();
    let host = KvHost::new(config);
    for name in stores {
        let id = StoreId::parse(name).unwrap();
        host.register(id.clone(), dir.path().join(format!("{name}.redb")))
            .unwrap();
        host.set_bindings(
            &format!("{name}/production"),
            vec![Binding {
                token_hash: hash_token(name),
                store: id,
                home: Home::Local,
            }],
        );
    }
    (dir, listen(&host, Access::Apps).await)
}

#[tokio::test]
async fn transactions_and_watches_share_the_request_budget() {
    let config = HostConfig {
        max_buffered_bytes: 256 * 1024,
        ..HostConfig::default()
    };
    let (_dir, addr) = host_with(config, &["main"]).await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    assert_eq!(raw(&mut stream, b"AUTH main\r\n", 5).await, b"+OK\r\n");
    let short = "-ERR the server is short of request buffer space; retry\r\n";
    // Queued commands beyond 64 KiB draw on the shared budget; once it is
    // used up, queuing fails and EXEC aborts.
    let set = |i: usize| {
        let mut out = Vec::new();
        tako_kv::encode_request(
            &[
                b"SET".as_slice(),
                format!("k{i}").as_bytes(),
                &[b'v'; 100_000],
            ],
            &mut out,
        );
        out
    };
    raw(&mut stream, b"MULTI\r\n", 5).await;
    let mut replies = String::new();
    for i in 0..5 {
        let got = raw(&mut stream, &set(i), 9).await;
        replies.push_str(&String::from_utf8_lossy(&got));
    }
    assert!(
        replies.starts_with("+QUEUED\r\n") && replies.ends_with(short),
        "{replies}"
    );
    let got = raw(&mut stream, b"EXEC\r\n", 10).await;
    assert!(String::from_utf8_lossy(&got).starts_with("-EXECABORT"));
    // The budget came back: a smaller transaction runs.
    raw(&mut stream, b"MULTI\r\n", 5).await;
    raw(&mut stream, &set(0), 9).await;
    raw(&mut stream, &set(1), 9).await;
    let got = raw(&mut stream, b"EXEC\r\n", 15).await;
    assert_eq!(String::from_utf8_lossy(&got), "*2\r\n+OK\r\n+OK\r\n");
    // Watched keys are charged the same way, three copies each: the
    // request fits the budget, what watching its keys keeps does not.
    let keys: Vec<String> = (0..1000).map(|i| format!("{i:0>100}")).collect();
    let mut watch = vec!["WATCH".to_owned()];
    watch.extend(keys);
    let mut request = Vec::new();
    tako_kv::encode_request(&watch, &mut request);
    let got = raw(&mut stream, &request, short.len()).await;
    assert_eq!(String::from_utf8_lossy(&got), short);
    assert_eq!(raw(&mut stream, b"WATCH k0 k0 k1\r\n", 5).await, b"+OK\r\n");
}

#[tokio::test]
async fn transactions_cap_the_connections_own_replies() {
    let config = HostConfig {
        max_reply_bytes: 64 * 1024,
        ..HostConfig::default()
    };
    let (_dir, addr) = host_with(config, &["main"]).await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let text = "e".repeat(1000);
    let mut send = b"AUTH main\r\nMULTI\r\n".to_vec();
    for _ in 0..200 {
        tako_kv::encode_request(&["ECHO", text.as_str()], &mut send);
    }
    send.extend_from_slice(b"EXEC\r\n");
    // OK, OK, 200 QUEUED, then the EXEC array.
    // Read until the server stops sending.
    let got = raw(&mut stream, &send, 1 << 20).await;
    let got = String::from_utf8_lossy(&got);
    let exec = &got[got.find("*200\r\n").expect("an EXEC array of 200")..];
    // About 64 KiB of echoes fit; the rest become errors.
    let echoed = exec.matches("$1000\r\n").count();
    let refused = exec.matches("-ERR reply would exceed 65536 bytes").count();
    assert!((50..70).contains(&echoed), "{echoed}");
    assert_eq!(echoed + refused, 200);
}

#[tokio::test]
async fn watching_a_key_again_keeps_its_first_version() {
    let (_dir, addr) = host_with(HostConfig::default(), &["main"]).await;
    let mut a = client(addr, "main", false).await;
    let mut b = client(addr, "main", false).await;
    let _: () = redis::cmd("WATCH")
        .arg("k")
        .query_async(&mut a)
        .await
        .unwrap();
    let _: () = b.set("k", 2).await.unwrap();
    let _: () = redis::cmd("WATCH")
        .arg("k")
        .query_async(&mut a)
        .await
        .unwrap();
    // The change after the first WATCH aborts the transaction.
    let result: redis::Value = redis::pipe()
        .atomic()
        .set("k", 3)
        .query_async(&mut a)
        .await
        .unwrap();
    assert_eq!(result, redis::Value::Nil);
    let value: i64 = b.get("k").await.unwrap();
    assert_eq!(value, 2);
}

#[tokio::test]
async fn client_list_shows_only_connections_to_the_same_store() {
    let (_dir, addr) = host_with(HostConfig::default(), &["main", "other"]).await;
    let mut one = client(addr, "main", false).await;
    let _two = client(addr, "main", false).await;
    let _elsewhere = client(addr, "other", false).await;
    let list: String = redis::cmd("CLIENT")
        .arg("LIST")
        .query_async(&mut one)
        .await
        .unwrap();
    assert_eq!(list.lines().count(), 2, "{list}");
    let id: i64 = redis::cmd("CLIENT")
        .arg("ID")
        .query_async(&mut one)
        .await
        .unwrap();
    let mine: String = redis::cmd("CLIENT")
        .arg("LIST")
        .arg("ID")
        .arg(id)
        .query_async(&mut one)
        .await
        .unwrap();
    assert!(mine.starts_with(&format!("id={id} ")) && mine.lines().count() == 1);
}

#[tokio::test]
async fn watch_aborts_transactions_on_change() {
    let s = server("secret").await;
    let mut a = TcpStream::connect(s.addr).await.unwrap();
    let mut b = client(s.addr, "secret", false).await;
    raw(&mut a, b"AUTH secret\r\nSET n 1\r\nWATCH n\r\n", 15).await;
    let _: () = b.set("n", "2").await.unwrap();
    let got = raw(&mut a, b"MULTI\r\nINCR n\r\nEXEC\r\nGET n\r\n", 29).await;
    assert_eq!(got, b"+OK\r\n+QUEUED\r\n*-1\r\n$1\r\n2\r\n");
    let got = raw(&mut a, b"WATCH n\r\nMULTI\r\nINCR n\r\nEXEC\r\n", 27).await;
    assert_eq!(got, b"+OK\r\n+OK\r\n+QUEUED\r\n*1\r\n:3\r\n");
    // A key absent at WATCH that another client creates and deletes again.
    raw(&mut a, b"WATCH lock\r\n", 5).await;
    let _: () = b.set("lock", "b").await.unwrap();
    let _: () = b.del("lock").await.unwrap();
    let got = raw(&mut a, b"MULTI\r\nSET lock a\r\nEXEC\r\n", 22).await;
    assert_eq!(got, b"+OK\r\n+QUEUED\r\n*-1\r\n");
}

#[tokio::test]
async fn large_values_and_pipelines_round_trip() {
    let s = server("secret").await;
    let mut kv = client(s.addr, "secret", false).await;
    let value = vec![7u8; 4 * 1024 * 1024];
    let _: () = kv.set("big", value.clone()).await.unwrap();
    let back: Vec<u8> = kv.get("big").await.unwrap();
    assert_eq!(back, value);
    let mut pipe = redis::pipe();
    for i in 0..5000 {
        pipe.set(format!("k{i}"), i).ignore();
    }
    pipe.get("k4999");
    let (last,): (i64,) = pipe.query_async(&mut kv).await.unwrap();
    assert_eq!(last, 4999);
    let size: i64 = redis::cmd("DBSIZE").query_async(&mut kv).await.unwrap();
    assert_eq!(size, 5001);
    let too_big = vec![0u8; 17 * 1024 * 1024];
    let refused: redis::RedisResult<()> = kv.set("huge", too_big).await;
    assert!(refused.is_err());
}

#[tokio::test]
async fn command_info_describes_every_command() {
    let s = server("secret").await;
    let mut kv = client(s.addr, "secret", true).await;
    let count: i64 = redis::cmd("COMMAND")
        .arg("COUNT")
        .query_async(&mut kv)
        .await
        .unwrap();
    let all: Vec<redis::Value> = redis::cmd("COMMAND").query_async(&mut kv).await.unwrap();
    assert_eq!(all.len() as i64, count);
    let info: Vec<redis::Value> = redis::cmd("COMMAND")
        .arg("INFO")
        .arg("MSET")
        .arg("nosuch")
        .query_async(&mut kv)
        .await
        .unwrap();
    let redis::Value::Array(mset) = &info[0] else {
        panic!("{info:?}")
    };
    assert_eq!(mset.len(), 10);
    assert_eq!(mset[1], redis::Value::Int(-3));
    // Keys start at 1, run to the end, and come every other argument.
    assert_eq!(
        mset[3..6],
        [
            redis::Value::Int(1),
            redis::Value::Int(-1),
            redis::Value::Int(2)
        ]
    );
    assert!(matches!(&mset[2], redis::Value::Set(flags) if flags.len() == 2));
    assert_eq!(info[1], redis::Value::Nil);
}

#[tokio::test]
async fn replies_draw_on_a_shared_budget() {
    let dir = tempfile::tempdir().unwrap();
    let host = KvHost::new(HostConfig {
        max_buffered_reply_bytes: 3 * 1024 * 1024,
        stall_timeout: Duration::from_millis(300),
        ..HostConfig::default()
    });
    let id = StoreId::parse("main").unwrap();
    host.register(id.clone(), dir.path().join("main.redb"))
        .unwrap();
    host.set_bindings(
        "app/production",
        vec![Binding {
            token_hash: hash_token("secret"),
            store: id,
            home: Home::Local,
        }],
    );
    let addr = listen(&host, Access::Apps).await;
    let mut kv = client(addr, "secret", false).await;
    let big = vec![1u8; 2 * 1024 * 1024];
    let _: () = kv.set("big", big.clone()).await.unwrap();
    // Each large reply takes budget until it is sent, then returns it.
    for _ in 0..3 {
        let back: Vec<u8> = kv.get("big").await.unwrap();
        assert_eq!(back, big);
    }
    // Many replies together go out in turns.
    let value = vec![2u8; 20 * 1024];
    let _: () = kv.set("medium", value.clone()).await.unwrap();
    let mut pipe = redis::pipe();
    for _ in 0..300 {
        pipe.get("medium");
    }
    let all: Vec<Vec<u8>> = pipe.query_async(&mut kv).await.unwrap();
    assert!(all.len() == 300 && all.iter().all(|v| *v == value));
    // A reply larger than the whole budget fails, and a write that would
    // return it changes nothing.
    let huge = vec![3u8; 4 * 1024 * 1024];
    let _: () = kv.set("huge", huge).await.unwrap();
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let short = "-ERR the server is short of reply buffer space; retry\r\n";
    let send =
        "AUTH secret\r\nGET huge\r\nGETDEL huge\r\nMULTI\r\nGET huge\r\nEXEC\r\nEXISTS huge\r\n";
    let expected = format!("+OK\r\n{short}{short}+OK\r\n+QUEUED\r\n{short}:1\r\n");
    let got = raw(&mut stream, send.as_bytes(), expected.len()).await;
    assert_eq!(String::from_utf8_lossy(&got), expected);
}

/// Two hosts: `home` holds the store; apps on `edge` reach it through
/// their own server, which relays to `home`'s peer listener.
#[tokio::test]
async fn edge_servers_relay_to_the_home_server() {
    let dir = tempfile::tempdir().unwrap();
    let home = KvHost::new(HostConfig::default());
    let id = StoreId::parse("sessions").unwrap();
    home.register(id.clone(), dir.path().join("sessions.redb"))
        .unwrap();
    home.set_remote_grants("shop/production", BTreeSet::from([id.clone()]), "grant-key");
    let peer = listen(
        &home,
        Access::Peers {
            allow: Arc::new(|ip| ip.is_loopback()),
        },
    )
    .await;
    let edge = KvHost::new(HostConfig::default());
    let bind = |key: &str| {
        vec![Binding {
            token_hash: hash_token("edge-token"),
            store: id.clone(),
            home: Home::Remote {
                upstream: peer,
                key: key.into(),
            },
        }]
    };
    // A release command's temporary scope forwards as its environment.
    edge.set_bindings("shop/production#release", bind("grant-key"));
    let edge_addr = listen(&edge, Access::Apps).await;

    for resp3 in [false, true] {
        let mut kv = client(edge_addr, "edge-token", resp3).await;
        let _: () = kv.set_ex("challenge", "xyz", 60).await.unwrap();
        let value: String = kv.get("challenge").await.unwrap();
        assert_eq!(value, "xyz");
        let user: HashMap<String, String> = {
            let _: () = kv.hset("h", "f", "v").await.unwrap();
            kv.hgetall("h").await.unwrap()
        };
        assert_eq!(user["f"], "v");
    }
    // The data lives on the home server.
    let reply = home
        .admin(&id, vec!["GET".into(), "challenge".into()])
        .await
        .unwrap();
    assert_eq!(reply, tako_kv::Reply::Bulk("xyz".into()));

    // A wrong grant key fails authentication; the connection stays usable.
    edge.set_bindings("shop/production#release", bind("wrong"));
    let mut stream = TcpStream::connect(edge_addr).await.unwrap();
    let got = raw(&mut stream, b"AUTH edge-token\r\nPING\r\n", 60).await;
    let text = String::from_utf8(got).unwrap();
    assert!(
        text.starts_with("-ERR the store's server refused this app"),
        "{text}"
    );
    assert!(
        text.ends_with("-NOAUTH Authentication required.\r\n"),
        "{text}"
    );

    // A HELLO 3 whose relay fails keeps the connection on RESP2, as a
    // failed HELLO does in Redis.
    let local = StoreId::parse("local").unwrap();
    edge.register(local.clone(), dir.path().join("local.redb"))
        .unwrap();
    let mut stream = TcpStream::connect(edge_addr).await.unwrap();
    let got = raw(&mut stream, b"HELLO 3 AUTH default edge-token\r\n", 30).await;
    assert!(got.starts_with(b"-ERR the store's server refused this app"));
    edge.set_bindings(
        "shop/production#release",
        vec![Binding {
            token_hash: hash_token("edge-token"),
            store: local,
            home: Home::Local,
        }],
    );
    assert_eq!(
        raw(&mut stream, b"AUTH edge-token\r\nGET missing\r\n", 10).await,
        b"+OK\r\n$-1\r\n"
    );

    // Peers the listener does not allow are dropped before any handshake.
    let strict = listen(
        &home,
        Access::Peers {
            allow: Arc::new(|_| false),
        },
    )
    .await;
    let mut stream = TcpStream::connect(strict).await.unwrap();
    assert!(raw(&mut stream, b"PING\r\n", 1).await.is_empty());
}

#[tokio::test]
async fn revoked_bindings_end_their_connections() {
    let s = server("secret").await;
    let mut stream = TcpStream::connect(s.addr).await.unwrap();
    assert_eq!(
        raw(&mut stream, b"AUTH secret\r\nSET k v\r\n", 10).await,
        b"+OK\r\n+OK\r\n"
    );
    s.host.set_bindings("app/production", Vec::new());
    let got = raw(&mut stream, b"GET k\r\n", 100).await;
    assert_eq!(
        got,
        b"-ERR this connection's binding changed; reconnect\r\n"
    );
    assert!(raw(&mut stream, b"PING\r\n", 1).await.is_empty());
}

#[tokio::test]
async fn closing_the_host_disconnects_clients() {
    let s = server("secret").await;
    let mut stream = TcpStream::connect(s.addr).await.unwrap();
    assert_eq!(raw(&mut stream, b"AUTH secret\r\n", 5).await, b"+OK\r\n");
    s.host.close_all().await;
    let mut chunk = [0u8; 16];
    let read = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut chunk)).await;
    assert!(matches!(read, Ok(Ok(0)) | Ok(Err(_))));
}

#[tokio::test]
async fn client_names_are_checked_and_limited() {
    let s = server("secret").await;
    let mut kv = client(s.addr, "secret", false).await;
    let set = |name: String| {
        let mut cmd = redis::cmd("CLIENT");
        cmd.arg("SETNAME").arg(name);
        cmd
    };
    let longest = "n".repeat(256);
    let _: () = set(longest.clone()).query_async(&mut kv).await.unwrap();
    let name: String = redis::cmd("CLIENT")
        .arg("GETNAME")
        .query_async(&mut kv)
        .await
        .unwrap();
    assert_eq!(name, longest);
    let error = set("n".repeat(257))
        .query_async::<()>(&mut kv)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("limited to 256 bytes"),
        "{error}"
    );
    // HELLO checks the name the same way, and changes nothing when it fails.
    let error = redis::cmd("HELLO")
        .arg(2)
        .arg("SETNAME")
        .arg("n".repeat(257))
        .query_async::<redis::Value>(&mut kv)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("limited to 256 bytes"),
        "{error}"
    );
    let _: redis::Value = redis::cmd("HELLO")
        .arg(2)
        .arg("SETNAME")
        .arg("short")
        .query_async(&mut kv)
        .await
        .unwrap();
    let list: String = redis::cmd("CLIENT")
        .arg("LIST")
        .query_async(&mut kv)
        .await
        .unwrap();
    assert!(list.contains(" name=short "), "{list}");
}
