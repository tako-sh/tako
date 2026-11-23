//! A hosted workflow store served from an in-process database host, for
//! tests of every backend-generic contract.

use std::path::PathBuf;

use tako_db::testing::TestServer;

use crate::enqueue::{RunsDb, WorkflowStoreConfig};

/// One hosted database, served for as long as the fixture lives.
pub(crate) struct HostedFixture {
    server: TestServer,
    token: String,
    /// The database file, for tests that inspect it directly.
    #[allow(dead_code)]
    pub(crate) path: PathBuf,
}

impl HostedFixture {
    pub(crate) fn start() -> Self {
        let server = TestServer::start();
        let database = server.database("workflow-test/production#workflows");
        Self {
            server,
            token: database.token,
            path: database.path,
        }
    }

    pub(crate) fn config(&self, app_id: &str) -> WorkflowStoreConfig {
        WorkflowStoreConfig::hosted(self.server.url.clone(), self.token.clone(), app_id)
    }

    /// A new handle for `app_id`, as another server would open it.
    pub(crate) fn open(&self, app_id: &str) -> RunsDb {
        RunsDb::open_config(self.config(app_id)).unwrap()
    }

    /// A handle on a server whose clock is `clock`.
    #[allow(dead_code)]
    pub(crate) fn open_with_clock(&self, app_id: &str, clock: fn() -> i64) -> RunsDb {
        RunsDb::open_hosted_with_clock(&self.server.url, &self.token, app_id, clock).unwrap()
    }
}
