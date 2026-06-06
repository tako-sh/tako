//! Durable workflow/task engine.
//!
//! Per-app queue stored through an internal adapter. SQLite currently backs local
//! state at `{data_dir}/apps/{app}/data/tako/workflows.sqlite`; Postgres is the
//! intended shared backend.
//!
//! Module layout:
//! - `schema` — SQLite DDL and connection init
//! - `cron` — cron-tick loop that enqueues scheduled tasks
//! - `supervisor` — per-app worker process lifecycle
//! - `enqueue_socket` — per-app unix socket listener for SDK RPCs

pub mod cron;
pub mod dispatcher;
pub mod enqueue;
pub mod enqueue_socket;
pub mod in_flight;
pub mod manager;
pub mod schema;
pub mod supervisor;

#[allow(unused_imports)]
pub use dispatcher::{DispatchSignal, WorkDispatcher};
#[allow(unused_imports)]
pub use enqueue::{POSTGRES_WORKFLOWS_SCHEMA, RunsDb, WorkflowStoreConfig};
#[allow(unused_imports)]
pub use enqueue_socket::{
    AppHandlers, AppLookup, ChannelPublishFn, EnqueueSocketHandle, HealthCheck, OnClaimed,
    OnEnqueue, spawn as spawn_enqueue_socket,
};
#[allow(unused_imports)]
pub use in_flight::InFlightLimiter;
#[allow(unused_imports)]
pub use manager::{
    WorkflowManager, WorkflowManagerError, internal_socket_path, worker_spec_for_bun,
};
#[allow(unused_imports)]
pub use supervisor::{WorkerLogSink, WorkerSpec, WorkerSupervisor};
