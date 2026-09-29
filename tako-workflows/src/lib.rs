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

mod blocking;
pub mod cron;
pub mod dispatcher;
pub mod enqueue;
pub mod enqueue_socket;
pub mod in_flight;
pub mod manager;
mod postgres_store;
pub mod schema;
pub mod supervisor;

#[allow(unused_imports)]
pub use dispatcher::{DispatchSignal, WorkDispatcher};
#[allow(unused_imports)]
pub use enqueue::{DEFAULT_RETENTION_MS, POSTGRES_WORKFLOWS_SCHEMA, RunsDb, WorkflowStoreConfig};
#[allow(unused_imports)]
pub use enqueue_socket::{
    AppHandlers, AppLookup, ChannelPublishFn, EnqueueSocketHandle, HealthCheck, OnClaimed,
    OnEnqueue, PeerAuthFn, spawn as spawn_enqueue_socket,
};
#[allow(unused_imports)]
pub use in_flight::InFlightLimiter;
#[allow(unused_imports)]
pub use manager::{
    WorkflowManager, WorkflowManagerError, internal_socket_path, worker_spec_for_bun,
    worker_spec_for_command,
};
#[allow(unused_imports)]
pub use supervisor::{
    WorkerLane, WorkerLogSink, WorkerSpec, WorkerSupervisor, workflow_lanes_from_dir,
};
