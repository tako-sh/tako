//! Durable workflow/task engine.
//!
//! Per-app queue stored in SQLite at `{data_dir}/apps/{app}/data/tako/workflows.sqlite`. The
//! server writes on enqueue and cron-tick; a separately-supervised per-app
//! worker process reads the DB and executes handlers.
//!
//! Module layout:
//! - `schema` — SQLite DDL and connection init
//! - `enqueue` — insert-with-uniqueness for `Command::EnqueueRun`
//!
//! Future modules (coming in later steps of the v1 plan):
//! - `cron` — cron-tick loop that enqueues scheduled tasks
//! - `supervisor` — per-app worker process lifecycle
//! - `enqueue_socket` — per-app unix socket listener for SDK RPCs
//! - `drain` — graceful stop with in-flight wait

pub mod cron;
pub mod enqueue;
pub mod enqueue_socket;
pub mod in_flight;
pub mod manager;
pub mod schema;
pub mod supervisor;

#[allow(unused_imports)]
pub use enqueue::RunsDb;
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
