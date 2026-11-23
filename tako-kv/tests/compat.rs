//! Replays one command script against Redis and against this server and
//! compares the replies byte for byte. Needs `redis-server` on `PATH`:
//!
//! ```text
//! cargo test -p tako-kv --test compat -- --ignored
//! ```

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use tako_kv::{Access, Binding, Home, HostConfig, KvHost, StoreId, hash_token};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Commands whose replies must match. A leading `~` compares an array reply
/// (or a `SCAN` page's keys) without regard to order; `%` a field and value
/// list (or an `HSCAN` page's) as pairs without regard to order; `?` only
/// the reply's type, for values that depend on timing.
const SCRIPT: &str = r#"
PING
PING hello
ECHO "a b"
SET k v
GET k
SET k v2 XX GET
SET k v3 NX
SET k v4 NX GET
SET n1 x NX GET
GET n1
SET k v EX 100
TTL k
?PTTL k
SET at v PXAT 4102444800700
EXPIRETIME at
PEXPIRETIME at
DEL at
SET k v KEEPTTL
TTL k
SET k v PX 100000 KEEPTTL
SET k v EX 0
SET k v EX -1
SET k v EX abc
SET k v EX 9223372036854775807
SET k v PX 9223372036854775807
SET k v NX XX
SET k v GET GET
SET k v NX NX
SET k v NX XX
SET k v EX 10 EX 20
TTL k
SET k v EX 10 PX 10
SET k v KEEPTTL KEEPTTL
SET k v
SET k
SETNX k x
SETNX k2 x
SETEX k3 10 v
SETEX k3 0 v
PSETEX k3 1000 v
?PTTL k3
GETSET k3 new
TTL k3
GETDEL k3
GETDEL k3
GETEX k
GETEX k EX 50
TTL k
GETEX k PERSIST
TTL k
GETEX k PX 0
GETEX k EX 1 PX 1
MSET a 1 b 2 c 3
MSET a 1 b
MGET a b c nothing
MSETNX a 1 z 2
MSETNX y 1 z 2
APPEND a 23
APPEND newkey abc
STRLEN a
STRLEN nothing
INCR counter
INCRBY counter 10
DECR counter
DECRBY counter 5
INCRBY counter abc
INCR k
INCRBY big 9223372036854775807
INCR big
DECRBY small 9223372036854775807
DECRBY small 10
INCRBYFLOAT f 1.5
INCRBYFLOAT f 0.25
INCRBYFLOAT f -1.75
INCRBYFLOAT f abc
INCRBYFLOAT k 1
SET fl 3.0
INCRBYFLOAT fl 1
SET padded " 1"
INCR padded
SET plus +1
INCR plus
SET zero 007
INCR zero
DEL a b nothing
UNLINK c
EXISTS k k nothing
TOUCH k nothing
TYPE k
TYPE nothing
EXPIRE k 100
EXPIRE k 100 NX
EXPIRE k 200 XX
EXPIRE k 50 GT
EXPIRE k 500 GT
EXPIRE k 600 LT
EXPIRE k 60 LT
TTL k
EXPIRE k 10 NX XX
EXPIRE k 10 GT LT
EXPIRE k 10 FOO
EXPIRE k abc
EXPIRE k 9223372036854775807
EXPIRE nothing 10
PERSIST k
PERSIST k
PERSIST nothing
PEXPIRE k 100000
?PTTL k
EXPIREAT k 99999999999
EXPIRETIME k
PEXPIREAT k 99999999999999
PEXPIRETIME k
EXPIRETIME nothing
TTL nothing
PTTL nothing
EXPIRE k -5
EXISTS k
SET r1 v EX 1000
RENAME r1 r2
TTL r2
RENAME nothing r3
RENAMENX r2 r2
SET r4 x
RENAMENX r2 r4
RENAME r2 r4
GET r4
HSET h a 1 b 2
HSET h b 3 c 4
HSET h a
HMSET h d 5
HSETNX h a 9
HSETNX h e 6
HGET h a
HGET h nothing
HGET nothing a
HMGET h a nothing c
%HGETALL h
HGETALL nothing
~HKEYS h
~HVALS h
%HSCAN h 0
%HSCAN h 0 MATCH [ab]
HSCAN nothing 0
HSCAN h abc
HSCAN h 0 COUNT 0
HLEN h
HLEN nothing
HEXISTS h a
HEXISTS h nothing
HSTRLEN h a
HINCRBY h a 10
HINCRBY h nothing 5
HINCRBY h a abc
HSET h t text
HINCRBY h t 1
HINCRBYFLOAT h fl 1.5
HINCRBYFLOAT h t 1
HDEL h a nothing
HDEL nothing a
GET h
SET h x GET
INCR h
APPEND h x
HSET k a b
HGET counter a
RENAME h h2
HLEN h2
DEL h2
EXISTS h2
SELECT 0
SELECT 16
SELECT abc
DBSIZE
MULTI
SET tx 1
INCR tx
HSET tx a b
GET tx
EXEC
MULTI
SET tx 1
GET
EXEC
EXEC
DISCARD
MULTI
MULTI
DISCARD
WATCH tx
UNWATCH
WAIT 0 0
CLIENT SETNAME app-1
CLIENT GETNAME
CLIENT SETNAME "bad name"
HELLO 3 SETNAME "bad name"
CLIENT GETNAME
CONFIG GET maxmemory-policy
FOO a b
GET a b
get k
GeT counter
~KEYS *
~KEYS h*
~KEYS [rk]*
~SCAN 0 COUNT 1000
~SCAN 0 MATCH r* COUNT 1000
~SCAN 0 COUNT 1000 TYPE hash
SCAN abc
SCAN 0 COUNT 0
FLUSHDB
DBSIZE
KEYS *
SET a 1
HSET h f v
FLUSHALL ASYNC
DBSIZE
GET a
HGET h f
SET a 2
FLUSHDB SYNC
GET a
FLUSHDB LAZY
FLUSHDB ASYNC SYNC
"#;

struct Redis(Child);

impl Drop for Redis {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Reads exactly one reply.
async fn reply(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Vec<u8> {
    loop {
        if let Some(len) = complete(buf, 0) {
            return buf.drain(..len).collect();
        }
        let mut chunk = [0u8; 65536];
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk))
            .await
            .expect("reply timed out")
            .unwrap();
        assert!(n > 0, "connection closed");
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// The length of the complete reply at `at`, if it has arrived.
fn complete(buf: &[u8], at: usize) -> Option<usize> {
    let end = at + buf.get(at..)?.windows(2).position(|w| w == b"\r\n")?;
    let line = std::str::from_utf8(&buf[at + 1..end]).ok()?;
    let after = end + 2;
    match buf[at] {
        b'+' | b'-' | b':' | b'_' | b',' => Some(after - at),
        b'$' => {
            let len: i64 = line.parse().ok()?;
            if len < 0 {
                return Some(after - at);
            }
            let total = after + len as usize + 2;
            (buf.len() >= total).then_some(total - at)
        }
        b'*' | b'%' | b'~' => {
            let count: i64 = line.parse().ok()?;
            let items = if buf[at] == b'%' { count * 2 } else { count };
            let mut pos = after;
            for _ in 0..items.max(0) {
                pos += complete(buf, pos)?;
            }
            Some(pos - at)
        }
        _ => panic!("unexpected reply {:?}", String::from_utf8_lossy(&buf[at..])),
    }
}

/// Splits an array reply's top-level items (or a SCAN page's keys).
fn items(reply: &[u8]) -> Vec<Vec<u8>> {
    if !matches!(reply[0], b'*' | b'%' | b'~') {
        // Not a list, such as an error: compared whole.
        return vec![reply.to_vec()];
    }
    let end = reply.windows(2).position(|w| w == b"\r\n").unwrap();
    let count: usize = std::str::from_utf8(&reply[1..end])
        .unwrap()
        .parse()
        .unwrap();
    let count = if reply[0] == b'%' { count * 2 } else { count };
    let mut pos = end + 2;
    let mut out = Vec::new();
    for _ in 0..count {
        let len = complete(reply, pos).unwrap();
        out.push(reply[pos..pos + len].to_vec());
        pos += len;
    }
    out
}

fn unordered(reply: &[u8]) -> Vec<Vec<u8>> {
    let mut parts = items(reply);
    if parts.len() == 2 && parts[1].first() == Some(&b'*') && parts[0].starts_with(b"$") {
        // A SCAN page: compare its keys.
        parts = items(&parts[1]);
    }
    parts.sort();
    parts
}

/// A field and value list's pairs, sorted, after an `HSCAN` page's cursor.
fn pairs(reply: &[u8]) -> Vec<Vec<u8>> {
    let mut parts = items(reply);
    let mut cursor = Vec::new();
    if parts.len() == 2 && parts[0].starts_with(b"$") && parts[1].first() == Some(&b'*') {
        cursor = parts[0].clone();
        parts = items(&parts[1]);
    }
    let mut pairs: Vec<Vec<u8>> = parts.chunks(2).map(<[Vec<u8>]>::concat).collect();
    pairs.sort();
    pairs.insert(0, cursor);
    pairs
}

fn split(line: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for c in line.chars() {
        match c {
            '"' => quoted = !quoted,
            ' ' if !quoted => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

#[tokio::test]
#[ignore = "needs redis-server"]
async fn replies_match_redis() {
    let port = free_port();
    let child = Command::new("redis-server")
        .args([
            "--port",
            &port.to_string(),
            "--save",
            "",
            "--appendonly",
            "no",
            "--requirepass",
            "secret",
        ])
        .stdout(Stdio::null())
        .spawn()
        .expect("redis-server on PATH");
    let _redis = Redis(child);

    let dir = tempfile::tempdir().unwrap();
    let host = KvHost::new(HostConfig::default());
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
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ours = listener.local_addr().unwrap();
    tokio::spawn(tako_kv::serve(host, listener, Access::Apps));

    let mut redis = None;
    for _ in 0..50 {
        if let Ok(stream) = TcpStream::connect(("127.0.0.1", port)).await {
            redis = Some(stream);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut redis = redis.expect("redis-server started");
    let mut kv = TcpStream::connect(ours).await.unwrap();
    let (mut redis_buf, mut kv_buf) = (Vec::new(), Vec::new());
    let mut differences = Vec::new();
    // The script runs twice: in RESP2, then in RESP3. It ends by emptying
    // the store, so both runs start alike.
    let script = std::iter::once("AUTH secret")
        .chain(SCRIPT.lines())
        .chain(std::iter::once("?HELLO 3"))
        .chain(SCRIPT.lines());
    for line in script.map(str::trim).filter(|l| !l.is_empty()) {
        let (mode, line) = match line.as_bytes()[0] {
            b'~' | b'%' | b'?' => (line.as_bytes()[0], &line[1..]),
            _ => (b'=', line),
        };
        let mut request = Vec::new();
        tako_kv::encode_request(&split(line), &mut request);
        redis.write_all(&request).await.unwrap();
        kv.write_all(&request).await.unwrap();
        let expected = reply(&mut redis, &mut redis_buf).await;
        let got = reply(&mut kv, &mut kv_buf).await;
        let same = match mode {
            b'~' => unordered(&expected) == unordered(&got),
            b'%' => pairs(&expected) == pairs(&got),
            b'?' => expected[0] == got[0],
            _ => expected == got,
        };
        if !same {
            differences.push(format!(
                "{line}\n  redis: {:?}\n  ours:  {:?}",
                String::from_utf8_lossy(&expected),
                String::from_utf8_lossy(&got)
            ));
        }
    }
    assert!(differences.is_empty(), "{}", differences.join("\n"));
}
