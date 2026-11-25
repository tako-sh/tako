//! Hosted replay databases for this crate's tests. Each test serves its own
//! database host on a loopback port, as a server serves an app's binding.
//! The hosted client blocks: async tests open and call stores off the
//! executor, through `spawn_blocking`, [`ChannelStore::run`] or the `_async`
//! methods.

use crate::{ChannelStore, ChannelStoreConfig};
use std::path::PathBuf;
use std::sync::Arc;
use tako_db::testing::TestServer;

/// The app a test's stores belong to unless it names another.
pub(crate) const APP: &str = "app";

pub(crate) struct TestDb {
    url: String,
    token: String,
    path: PathBuf,
    _server: TestServer,
}

impl TestDb {
    pub(crate) fn start() -> Self {
        let server = TestServer::start();
        let database = server.database("app#channels");
        Self {
            url: server.url.clone(),
            token: database.token,
            path: database.path,
            _server: server,
        }
    }

    pub(crate) fn config(&self, app: &str) -> ChannelStoreConfig {
        ChannelStoreConfig::hosted(&self.url, &self.token, app)
    }

    /// The app's broker-owned writer. Blocking.
    pub(crate) fn writer(&self) -> ChannelStore {
        self.writer_for(APP)
    }

    pub(crate) fn writer_for(&self, app: &str) -> ChannelStore {
        ChannelStore::open_config(self.config(app)).unwrap()
    }

    /// A reader of the app's history that never mutates it. Blocking.
    pub(crate) fn reader(&self) -> ChannelStore {
        ChannelStore::open_reader_config(self.config(APP)).unwrap()
    }

    /// A writer whose maintenance runs only on [`ChannelStore::prune`]. Blocking.
    pub(crate) fn without_maintenance(&self) -> ChannelStore {
        self.without_maintenance_for(APP)
    }

    pub(crate) fn without_maintenance_for(&self, app: &str) -> ChannelStore {
        ChannelStore::open_without_maintenance(self.config(app))
    }

    /// The app's writer, opened off the async executor.
    pub(crate) async fn open(&self) -> Arc<ChannelStore> {
        self.open_for(APP).await
    }

    pub(crate) async fn open_for(&self, app: &str) -> Arc<ChannelStore> {
        let config = self.config(app);
        tokio::task::spawn_blocking(move || Arc::new(ChannelStore::open_config(config).unwrap()))
            .await
            .unwrap()
    }

    pub(crate) async fn open_reader(&self) -> Arc<ChannelStore> {
        let config = self.config(APP);
        tokio::task::spawn_blocking(move || {
            Arc::new(ChannelStore::open_reader_config(config).unwrap())
        })
        .await
        .unwrap()
    }

    /// Direct access to the database file, as an operator's tools have.
    pub(crate) fn sql(&self) -> tako_db::testing::TestSql {
        tako_db::testing::TestSql::open(&self.path)
    }

    pub(crate) fn execute(&self, sql: &str) {
        self.sql().execute_batch(sql).unwrap();
    }

    pub(crate) fn query_i64(&self, sql: &str) -> i64 {
        self.sql().integers::<1>(sql, &[]).unwrap()[0]
    }

    /// Moves every stored message time and stream activity `ms` into the
    /// past, as if that much database time had passed.
    pub(crate) fn age(&self, ms: i64) {
        self.execute(&format!(
            "UPDATE tako_replay_messages SET created_ms = created_ms - {ms};
             UPDATE tako_replay_streams SET activity_ms = activity_ms - {ms};"
        ));
    }
}
