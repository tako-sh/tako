//! An in-process database host for tests of the code that uses it.
//!
//! [`TestServer`] serves a fresh host on a loopback port from its own
//! runtime, so blocking and async tests can both call it through the same
//! HTTP path production uses.

use std::collections::HashMap;
use std::path::PathBuf;

use parking_lot::Mutex;

use crate::http::{Access, serve};
use crate::{Binding, DatabaseHost, DatabaseId, Home, HostConfig, generate_token};

pub struct TestServer {
    /// The endpoint, `http://127.0.0.1:<port>`.
    pub url: String,
    pub host: DatabaseHost,
    dir: tempfile::TempDir,
    bindings: Mutex<HashMap<String, Vec<Binding>>>,
    runtime: Option<tokio::runtime::Runtime>,
}

/// A database registered with a [`TestServer`], and the token of its
/// binding.
pub struct TestDatabase {
    pub id: DatabaseId,
    pub path: PathBuf,
    pub token: String,
}

impl TestServer {
    /// Serves an empty host with app access.
    pub fn start() -> Self {
        Self::start_with(HostConfig::default())
    }

    pub fn start_with(config: HostConfig) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("test database runtime");
        let host = DatabaseHost::new(config);
        // Bound here rather than in the runtime, so a test that is itself
        // async does not block its own runtime waiting for the port.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test database");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let url = format!(
            "http://{}",
            listener.local_addr().expect("listener address")
        );
        let served = host.clone();
        runtime.spawn(async move {
            let listener = tokio::net::TcpListener::from_std(listener).expect("tokio listener");
            serve(served, listener, Access::Apps).await;
        });
        Self {
            url,
            host,
            dir: tempfile::tempdir().expect("test database directory"),
            bindings: Mutex::default(),
            runtime: Some(runtime),
        }
    }

    /// Creates a database and binds a new token to it in `scope`.
    pub fn database(&self, scope: &str) -> TestDatabase {
        let id = DatabaseId::generate();
        let path = self.dir.path().join(format!("{id}.sqlite"));
        DatabaseHost::create_file(&path).expect("create test database");
        self.host
            .register(id.clone(), path.clone())
            .expect("register test database");
        let token = self.bind(scope, &id);
        TestDatabase { id, path, token }
    }

    /// Adds a binding with a new token for `database` in `scope`.
    pub fn bind(&self, scope: &str, database: &DatabaseId) -> String {
        let token = generate_token();
        let mut bindings = self.bindings.lock();
        let scoped = bindings.entry(scope.to_string()).or_default();
        scoped.push(Binding {
            tokens: tako_core::BindingTokenHashes::new(&token),
            database: database.clone(),
            home: Home::Local,
        });
        self.host.set_bindings(scope, scoped.clone());
        token
    }

    /// Removes every binding of `scope`.
    pub fn revoke(&self, scope: &str) {
        self.bindings.lock().remove(scope);
        self.host.set_bindings(scope, Vec::new());
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}
