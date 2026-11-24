//! Serves one temporary database for client conformance tests and benchmarks.
//!
//! Prints `{"url": "...", "authToken": "...", "path": "..."}` on stdout once
//! listening, then serves until stdin closes.

use std::io::Read;
use std::time::Duration;

use tako_db::{Binding, DatabaseHost, DatabaseId, Home, HostConfig, generate_token, hash_token};

#[tokio::main]
async fn main() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let path = dir.path().join("database.sqlite");
    DatabaseHost::create_file(&path).expect("create database");

    let mut config = HostConfig::default();
    if let Ok(seconds) = std::env::var("TAKO_DATABASE_STREAM_IDLE_SECONDS") {
        config.stream_idle_timeout = Duration::from_secs(seconds.parse().expect("seconds"));
    }
    let host = DatabaseHost::new(config);
    let id = DatabaseId::generate();
    host.register(id.clone(), path.clone())
        .expect("register database");
    let token = generate_token();
    host.set_bindings(
        "conformance/test",
        vec![Binding {
            token_hash: hash_token(&token),
            database: id,
            home: Home::Local,
        }],
    );
    let _reaper = host.spawn_reaper();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind listener");
    let address = listener.local_addr().expect("listener address");
    println!(
        "{}",
        serde_json::json!({
            "url": format!("http://{address}"),
            "authToken": token,
            "path": path,
        })
    );

    let server = tokio::spawn(tako_db::http::serve(
        host,
        listener,
        tako_db::http::Access::Apps,
    ));
    // Exit when the parent closes stdin.
    tokio::task::spawn_blocking(|| {
        let mut sink = Vec::new();
        let _ = std::io::stdin().read_to_end(&mut sink);
    })
    .await
    .ok();
    server.abort();
    drop(dir);
}
