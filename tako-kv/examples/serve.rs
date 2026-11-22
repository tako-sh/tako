//! Serves one temporary store for client conformance tests and benchmarks.
//!
//! Prints the store's `redis://` URL on stdout once listening, then serves
//! until stdin closes.

use std::io::Read;

use tako_kv::{Access, Binding, Home, HostConfig, KvHost, StoreId, generate_token, hash_token};

#[tokio::main]
async fn main() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let host = KvHost::new(HostConfig::default());
    let id = StoreId::parse("conformance").unwrap();
    host.register(id.clone(), dir.path().join("store.redb"))
        .expect("open store");
    let token = generate_token();
    host.set_bindings(
        "conformance/test",
        vec![Binding {
            token_hash: hash_token(&token),
            store: id,
            home: Home::Local,
        }],
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind listener");
    let address = listener.local_addr().expect("listener address");
    println!("redis://:{token}@{address}");
    let server = tokio::spawn(tako_kv::serve(host, listener, Access::Apps));
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
