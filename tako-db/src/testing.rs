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

/// An operator session on a local test database, sharing the host's native owner.
pub struct TestSql {
    connection: std::sync::Arc<turso_core::Connection>,
    _owner: std::sync::Arc<crate::mvcc::Owner>,
}

impl TestSql {
    pub fn open(path: &std::path::Path) -> Self {
        let owner = crate::mvcc::Owner::local(path, std::time::Duration::from_secs(5)).unwrap();
        let connection = owner.connect().unwrap();
        connection.set_busy_timeout(std::time::Duration::from_secs(5));
        Self {
            connection,
            _owner: owner,
        }
    }
    /// Bulk-load a fixture whose counters have already been calculated.
    pub fn load_fixture(&self, sql: &str) -> turso_core::Result<()> {
        self.connection.set_triggers_enabled(false);
        let result = self.connection.execute(sql);
        self.connection.set_triggers_enabled(true);
        result
    }
    pub fn execute_batch(&self, sql: &str) -> turso_core::Result<()> {
        self.connection.execute(sql)
    }
    pub fn integers<const N: usize>(
        &self,
        sql: &str,
        args: &[&str],
    ) -> turso_core::Result<[i64; N]> {
        let mut stmt = self.connection.prepare(sql)?;
        for (index, arg) in args.iter().enumerate() {
            stmt.bind_at(
                std::num::NonZeroUsize::new(index + 1).unwrap(),
                turso_core::Value::from_text((*arg).to_owned()),
            )?;
        }
        let rows = stmt.run_collect_rows()?;
        let [row] = rows.as_slice() else {
            panic!("expected one operator query row")
        };
        assert_eq!(row.len(), N);
        Ok(std::array::from_fn(|i| row[i].as_int().unwrap()))
    }
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
        let path = self.dir.path().join(format!("{id}.db"));
        DatabaseHost::create_file(&path).expect("create test database");
        self.host
            .register(id.clone(), path.clone())
            .expect("register test database");
        let token = self.bind(scope, &id);
        TestDatabase { id, path, token }
    }

    /// Open the same single-voter Turso host used by a deployed app.
    /// Call this from a blocking test thread.
    pub fn replicated_database(&self, scope: &str) -> TestDatabase {
        use std::{collections::BTreeMap, time::Duration};
        let id = DatabaseId::generate();
        let path = self.dir.path().join(format!("{id}.db"));
        DatabaseHost::create_file(&path).unwrap();
        let runtime = self.runtime.as_ref().unwrap();
        let group = self.dir.path().join(id.as_str());
        let resource = runtime.block_on(async {
            let (node, replica) = tako_raft::Peer::default()
                .open(id.as_str(), [1; 32], 1, &group, Default::default())
                .await
                .unwrap();
            node.initialize(BTreeMap::from([(1, "127.0.0.1:1".parse().unwrap())]))
                .await
                .unwrap();
            node.wait(Some(Duration::from_secs(5)))
                .current_leader(1, "test group")
                .await
                .unwrap();
            tako_raft::Resource::new(node, replica, &group)
        });
        self.host
            .register_replicated(id.clone(), path.clone(), resource)
            .unwrap();
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
