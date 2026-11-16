//! Protocol behavior through the HTTP handler.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{Request, StatusCode};
use http_body_util::{BodyExt, Full};
use serde_json::{Value, json};
use tako_database::http::{Access, DATABASE_HEADER, SCOPE_HEADER, handle};
use tako_database::{
    Binding, DatabaseHost, DatabaseId, Home, HostConfig, generate_token, hash_token,
};

struct Harness {
    _dir: tempfile::TempDir,
    host: DatabaseHost,
    id: DatabaseId,
    token: String,
}

fn harness(config: HostConfig) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.sqlite");
    DatabaseHost::create_file(&path).unwrap();
    let host = DatabaseHost::new(config);
    let id = DatabaseId::generate();
    host.register(id.clone(), path).unwrap();
    let token = generate_token();
    host.set_bindings(
        "shop/production",
        vec![Binding {
            token_hash: hash_token(&token),
            database: id.clone(),
            home: Home::Local,
        }],
    );
    Harness {
        _dir: dir,
        host,
        id,
        token,
    }
}

const PEER: &str = "127.0.0.1:40000";

async fn post(h: &Harness, path: &str, body: Value) -> (StatusCode, Value) {
    let request = Request::post(path)
        .header("authorization", format!("Bearer {}", h.token))
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap();
    send(&h.host, &Access::Apps, request).await
}

async fn send(
    host: &DatabaseHost,
    access: &Access,
    request: Request<Full<Bytes>>,
) -> (StatusCode, Value) {
    let peer: SocketAddr = PEER.parse().unwrap();
    let response = handle(host, access, peer, request).await;
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()))
    };
    (status, value)
}

fn execute(sql: &str) -> Value {
    json!({"type": "execute", "stmt": {"sql": sql}})
}

async fn pipeline(h: &Harness, baton: Value, requests: Vec<Value>) -> Value {
    let (status, body) = post(
        h,
        "/v3/pipeline",
        json!({"baton": baton, "requests": requests}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

fn rows(result: &Value) -> &Value {
    &result["response"]["result"]["rows"]
}

#[tokio::test]
async fn version_probes_and_unknown_paths() {
    let h = harness(HostConfig::default());
    for path in ["/v2", "/v3"] {
        let request = Request::get(path).body(Full::new(Bytes::new())).unwrap();
        assert_eq!(
            send(&h.host, &Access::Apps, request).await.0,
            StatusCode::OK
        );
    }
    let request = Request::get("/v3-protobuf")
        .body(Full::new(Bytes::new()))
        .unwrap();
    assert_eq!(
        send(&h.host, &Access::Apps, request).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn requires_a_valid_bearer_token() {
    let h = harness(HostConfig::default());
    let body = json!({"baton": null, "requests": [execute("SELECT 1")]}).to_string();
    let missing = Request::post("/v2/pipeline")
        .body(Full::new(Bytes::from(body.clone())))
        .unwrap();
    assert_eq!(
        send(&h.host, &Access::Apps, missing).await.0,
        StatusCode::UNAUTHORIZED
    );
    let wrong = Request::post("/v2/pipeline")
        .header("authorization", "Bearer nope")
        .body(Full::new(Bytes::from(body)))
        .unwrap();
    assert_eq!(
        send(&h.host, &Access::Apps, wrong).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn stateless_pipelines_close_their_stream() {
    let h = harness(HostConfig::default());
    let body = pipeline(
        &h,
        Value::Null,
        vec![
            execute("CREATE TABLE t (x)"),
            execute("INSERT INTO t VALUES (1)"),
            execute("SELECT x FROM t"),
            execute("SELECT * FROM missing"),
        ],
    )
    .await;
    assert_eq!(body["baton"], Value::Null);
    let results = body["results"].as_array().unwrap();
    assert_eq!(results[1]["response"]["result"]["affected_row_count"], 1);
    assert_eq!(
        rows(&results[2]),
        &json!([[{"type": "integer", "value": "1"}]])
    );
    assert_eq!(results[3]["type"], "error");
    assert_eq!(results[3]["error"]["code"], "SQLITE_ERROR");
    assert_eq!(h.host.open_streams(), 0);
}

#[tokio::test]
async fn interactive_transactions_span_requests_and_batons_are_single_use() {
    let h = harness(HostConfig::default());
    pipeline(&h, Value::Null, vec![execute("CREATE TABLE t (x)")]).await;
    let begin = pipeline(
        &h,
        Value::Null,
        vec![execute("BEGIN"), execute("INSERT INTO t VALUES (1)")],
    )
    .await;
    let baton = begin["baton"].clone();
    assert!(baton.is_string());
    assert_eq!(h.host.open_streams(), 1);

    let next = pipeline(&h, baton.clone(), vec![execute("INSERT INTO t VALUES (2)")]).await;
    let second = next["baton"].clone();
    assert_ne!(second, baton);

    // A baton from an older response is no longer valid.
    let (status, _) = post(
        &h,
        "/v3/pipeline",
        json!({"baton": baton, "requests": [execute("SELECT 1")]}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let commit = pipeline(
        &h,
        second,
        vec![execute("COMMIT"), json!({"type": "close"})],
    )
    .await;
    assert_eq!(commit["baton"], Value::Null);
    assert_eq!(h.host.open_streams(), 0);
    let count = pipeline(&h, Value::Null, vec![execute("SELECT count(*) FROM t")]).await;
    assert_eq!(
        rows(&count["results"][0]),
        &json!([[{"type": "integer", "value": "2"}]])
    );
}

#[tokio::test]
async fn idle_streams_expire_and_roll_back() {
    let h = harness(HostConfig {
        stream_idle_timeout: Duration::from_millis(50),
        ..HostConfig::default()
    });
    pipeline(&h, Value::Null, vec![execute("CREATE TABLE t (x)")]).await;
    let begin = pipeline(
        &h,
        Value::Null,
        vec![
            execute("BEGIN IMMEDIATE"),
            execute("INSERT INTO t VALUES (1)"),
        ],
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    h.host.expire_idle_streams();
    let (status, _) = post(
        &h,
        "/v3/pipeline",
        json!({"baton": begin["baton"], "requests": [execute("COMMIT")]}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // The write lock was released and the insert rolled back.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let write = pipeline(
        &h,
        Value::Null,
        vec![
            execute("INSERT INTO t VALUES (2)"),
            execute("SELECT count(*) FROM t"),
        ],
    )
    .await;
    assert_eq!(
        rows(&write["results"][1]),
        &json!([[{"type": "integer", "value": "1"}]])
    );
}

#[tokio::test]
async fn stored_sql_keeps_the_stream_open() {
    let h = harness(HostConfig::default());
    let stored = pipeline(
        &h,
        Value::Null,
        vec![json!({"type": "store_sql", "sql_id": 7, "sql": "SELECT ? * 2"})],
    )
    .await;
    assert!(stored["baton"].is_string());
    let used = pipeline(
        &h,
        stored["baton"].clone(),
        vec![
            json!({"type": "execute", "stmt": {"sql_id": 7, "args": [{"type": "integer", "value": "21"}]}}),
            json!({"type": "close_sql", "sql_id": 7}),
            json!({"type": "execute", "stmt": {"sql_id": 7}}),
        ],
    )
    .await;
    assert_eq!(
        rows(&used["results"][0]),
        &json!([[{"type": "integer", "value": "42"}]])
    );
    assert_eq!(used["results"][2]["error"]["code"], "SQL_NOT_FOUND");
    assert_eq!(used["baton"], Value::Null);
}

#[tokio::test]
async fn version_two_rejects_version_three_requests_and_close_must_be_last() {
    let h = harness(HostConfig::default());
    let (status, _) = post(
        &h,
        "/v2/pipeline",
        json!({"baton": null, "requests": [{"type": "get_autocommit"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let body = pipeline(&h, Value::Null, vec![json!({"type": "get_autocommit"})]).await;
    assert_eq!(body["results"][0]["response"]["is_autocommit"], true);
    let (status, _) = post(
        &h,
        "/v3/pipeline",
        json!({"baton": null, "requests": [{"type": "close"}, execute("SELECT 1")]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = post(&h, "/v3/pipeline", json!({"nope": true})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn batches_follow_their_conditions() {
    let h = harness(HostConfig::default());
    pipeline(&h, Value::Null, vec![execute("CREATE TABLE t (x UNIQUE)")]).await;
    // The transaction pattern libSQL clients send for `client.batch()`.
    let batch = json!({"type": "batch", "batch": {"steps": [
        {"stmt": {"sql": "BEGIN"}},
        {"condition": {"type": "ok", "step": 0}, "stmt": {"sql": "INSERT INTO t VALUES (1)"}},
        {"condition": {"type": "ok", "step": 1}, "stmt": {"sql": "INSERT INTO t VALUES (1)"}},
        {"condition": {"type": "ok", "step": 2}, "stmt": {"sql": "COMMIT"}},
        {"condition": {"type": "not", "cond": {"type": "ok", "step": 3}}, "stmt": {"sql": "ROLLBACK"}},
    ]}});
    let body = pipeline(&h, Value::Null, vec![batch]).await;
    let result = &body["results"][0]["response"]["result"];
    assert!(result["step_results"][1].is_object());
    assert_eq!(
        result["step_errors"][2]["extended_code"],
        "SQLITE_CONSTRAINT_UNIQUE"
    );
    assert_eq!(result["step_results"][3], Value::Null);
    assert_eq!(result["step_errors"][3], Value::Null);
    assert!(result["step_results"][4].is_object());
    assert_eq!(body["baton"], Value::Null);
    let count = pipeline(&h, Value::Null, vec![execute("SELECT count(*) FROM t")]).await;
    assert_eq!(
        rows(&count["results"][0]),
        &json!([[{"type": "integer", "value": "0"}]])
    );

    let forward = json!({"type": "batch", "batch": {"steps": [
        {"condition": {"type": "ok", "step": 0}, "stmt": {"sql": "SELECT 1"}},
    ]}});
    let (status, _) = post(
        &h,
        "/v3/pipeline",
        json!({"baton": null, "requests": [forward]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn cursors_stream_entries_and_keep_their_stream() {
    let h = harness(HostConfig::default());
    pipeline(
        &h,
        Value::Null,
        vec![
            execute("CREATE TABLE t (x)"),
            execute("INSERT INTO t VALUES (1), (2)"),
        ],
    )
    .await;
    let request = Request::post("/v3/cursor")
        .header("authorization", format!("Bearer {}", h.token))
        .body(Full::new(Bytes::from(
            json!({"baton": null, "batch": {"steps": [
                {"stmt": {"sql": "SELECT x FROM t ORDER BY x"}},
                {"stmt": {"sql": "SELECT * FROM missing"}},
                {"condition": {"type": "ok", "step": 1}, "stmt": {"sql": "SELECT 3"}},
            ]}})
            .to_string(),
        )))
        .unwrap();
    let response = handle(&h.host, &Access::Apps, PEER.parse().unwrap(), request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let lines: Vec<Value> = std::str::from_utf8(&bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let baton = lines[0]["baton"].clone();
    assert!(baton.is_string());
    let kinds: Vec<&str> = lines[1..]
        .iter()
        .map(|l| l["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "step_begin",
            "row",
            "row",
            "step_end",
            "step_error",
            "replication_index"
        ]
    );
    assert_eq!(lines[1]["cols"][0]["name"], "x");
    assert_eq!(lines[3]["row"][0]["value"], "2");
    assert_eq!(lines[5]["step"], 1);

    // The stream stays open after the cursor and continues with its baton.
    let next = pipeline(
        &h,
        baton,
        vec![execute("SELECT 1"), json!({"type": "close"})],
    )
    .await;
    assert_eq!(next["baton"], Value::Null);
    assert_eq!(h.host.open_streams(), 0);
}

#[tokio::test]
async fn paused_databases_are_unavailable_until_resumed() {
    let h = harness(HostConfig::default());
    let begin = pipeline(&h, Value::Null, vec![execute("BEGIN")]).await;
    let guard = h
        .host
        .pause(&h.id, "database is being restored")
        .await
        .unwrap();
    assert_eq!(h.host.open_streams(), 0, "pausing closes open streams");
    let (status, body) = post(
        &h,
        "/v3/pipeline",
        json!({"baton": null, "requests": [execute("SELECT 1")]}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], "database is being restored");
    drop(guard);
    let (status, _) = post(
        &h,
        "/v3/pipeline",
        json!({"baton": begin["baton"], "requests": [execute("COMMIT")]}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    pipeline(&h, Value::Null, vec![execute("SELECT 1")]).await;
}

#[tokio::test]
async fn new_streams_beyond_the_limit_are_refused() {
    let h = harness(HostConfig {
        max_streams_per_database: 1,
        ..HostConfig::default()
    });
    pipeline(&h, Value::Null, vec![execute("BEGIN")]).await;
    let (status, _) = post(
        &h,
        "/v3/pipeline",
        json!({"baton": null, "requests": [execute("BEGIN")]}),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    // Stateless requests still run.
    pipeline(&h, Value::Null, vec![execute("SELECT 1")]).await;
}

#[tokio::test]
async fn oversized_requests_are_refused() {
    let h = harness(HostConfig {
        max_request_bytes: 64,
        ..HostConfig::default()
    });
    let (status, _) = post(
        &h,
        "/v3/pipeline",
        json!({"baton": null, "requests": [execute(&"x".repeat(200))]}),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn snapshots_track_changes() {
    let h = harness(HostConfig::default());
    let dir = tempfile::tempdir().unwrap();
    assert!(
        h.host.has_changes(&h.id),
        "a fresh host does not know the history"
    );
    h.host
        .snapshot(&h.id, &dir.path().join("a.sqlite"))
        .await
        .unwrap();
    assert!(!h.host.has_changes(&h.id));
    pipeline(&h, Value::Null, vec![execute("SELECT 1")]).await;
    assert!(!h.host.has_changes(&h.id), "reads do not count as changes");
    pipeline(&h, Value::Null, vec![execute("CREATE TABLE t (x)")]).await;
    assert!(h.host.has_changes(&h.id));
}

#[tokio::test]
async fn forwards_requests_to_the_server_that_hosts_the_database() {
    // The home server hosts the database and grants it to shop/production.
    let home = harness(HostConfig::default());
    home.host
        .set_remote_grants("shop/production", BTreeSet::from([home.id.clone()]));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = listener.local_addr().unwrap();
    let allow: Arc<dyn Fn(std::net::IpAddr) -> bool + Send + Sync> =
        Arc::new(|ip| ip.is_loopback());
    let server = tokio::spawn(tako_database::http::serve(
        home.host.clone(),
        listener,
        Access::Peers { allow },
    ));

    // The app's server only knows its binding and where the database lives.
    let edge = DatabaseHost::new(HostConfig::default());
    let token = generate_token();
    edge.set_bindings(
        "shop/production",
        vec![Binding {
            token_hash: hash_token(&token),
            database: home.id.clone(),
            home: Home::Remote { upstream },
        }],
    );
    let request = |body: Value| {
        Request::post("/v3/pipeline")
            .header("authorization", format!("Bearer {token}"))
            .body(Full::new(Bytes::from(body.to_string())))
            .unwrap()
    };
    let (status, begin) = send(
        &edge,
        &Access::Apps,
        request(json!({"baton": null, "requests": [execute("CREATE TABLE t (x)"), execute("BEGIN"), execute("INSERT INTO t VALUES (1)")]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{begin}");
    assert!(begin["baton"].is_string());
    let (status, commit) = send(
        &edge,
        &Access::Apps,
        request(json!({"baton": begin["baton"], "requests": [execute("COMMIT"), execute("SELECT count(*) FROM t"), {"type": "close"}]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{commit}");
    assert_eq!(
        rows(&commit["results"][1]),
        &json!([[{"type": "integer", "value": "1"}]])
    );

    // Without a grant the home server refuses forwarded requests.
    home.host
        .set_remote_grants("shop/production", BTreeSet::new());
    let (status, _) = send(
        &edge,
        &Access::Apps,
        request(json!({"baton": null, "requests": [execute("SELECT 1")]})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Peers that are not servers of the network are refused outright.
    let deny: Arc<dyn Fn(std::net::IpAddr) -> bool + Send + Sync> = Arc::new(|_| false);
    let forwarded = Request::post("/v3/pipeline")
        .header(SCOPE_HEADER, "shop/production")
        .header(DATABASE_HEADER, home.id.as_str())
        .body(Full::new(Bytes::from(
            json!({"baton": null, "requests": []}).to_string(),
        )))
        .unwrap();
    assert_eq!(
        send(&home.host, &Access::Peers { allow: deny }, forwarded)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    server.abort();
}

#[tokio::test]
async fn unreachable_home_servers_return_bad_gateway() {
    let edge = DatabaseHost::new(HostConfig::default());
    let token = generate_token();
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let upstream = closed.local_addr().unwrap();
    drop(closed);
    edge.set_bindings(
        "shop/production",
        vec![Binding {
            token_hash: hash_token(&token),
            database: DatabaseId::generate(),
            home: Home::Remote { upstream },
        }],
    );
    let request = Request::post("/v3/pipeline")
        .header("authorization", format!("Bearer {token}"))
        .body(Full::new(Bytes::from(
            json!({"baton": null, "requests": []}).to_string(),
        )))
        .unwrap();
    assert_eq!(
        send(&edge, &Access::Apps, request).await.0,
        StatusCode::BAD_GATEWAY
    );
}
