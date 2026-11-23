//! Durable workflow/task engine.
//!
//! Each app's runs live in its Tako-hosted workflow database (the
//! `@workflows` binding), shared by every server that runs the app. See
//! [`enqueue`] for the store.
//!
//! Module layout:
//! - `enqueue` — the run store and its lifecycle transitions
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
pub mod monitoring;
pub mod supervisor;

#[allow(unused_imports)]
pub use dispatcher::{DispatchSignal, WorkDispatcher};
#[allow(unused_imports)]
pub use enqueue::{DEFAULT_RETENTION_MS, RunsDb, WorkflowStoreConfig};
#[allow(unused_imports)]
pub use enqueue_socket::{
    AppHandlers, AppLookup, ChannelPublishFn, ChannelPublishRequest, ChannelWrite,
    EnqueueSocketHandle, HealthCheck, KvFn, OnClaimed, OnEnqueue, PeerAuthFn,
    spawn as spawn_enqueue_socket,
};
#[allow(unused_imports)]
pub use in_flight::InFlightLimiter;
#[allow(unused_imports)]
pub use manager::{
    ClosedStore, StoreResolver, WorkflowManager, WorkflowManagerError, internal_socket_path,
    worker_spec_for_bun, worker_spec_for_command,
};
#[allow(unused_imports)]
pub use supervisor::{
    WorkerLane, WorkerLogSink, WorkerSpec, WorkerSupervisor, workflow_lanes_from_dir,
};
pub mod network;
