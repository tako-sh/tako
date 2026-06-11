//! Rust SDK for Tako applications.
//!
//! The HTTP helper binds to Tako's `HOST`/`PORT` contract and writes the
//! selected port to fd 4 so `tako dev` and `tako-server` can route traffic.

pub mod http;
pub mod rpc;
pub mod runtime;
pub mod storage;
pub mod workflow;

pub use http::{
    BindOptions, Bootstrap, INTERNAL_STATUS_PATH, INTERNAL_TOKEN_HEADER, InternalStatusResponse,
    bind_listener, internal_status_response, is_internal_status_request, read_bootstrap,
    report_ready,
};
pub use rpc::{
    APP_NAME_ENV, Client, EnqueueOpts, EnqueueResult, INTERNAL_SOCKET_ENV, Run, ScheduleSpec,
    WORKFLOW_SOCKET_ENV, enqueue, publish_channel, signal,
};
pub use runtime::Runtime;
pub use storage::{Storage, StorageBag, StorageBinding, UrlOptions};
pub use workflow::{StepApi, Worker, WorkflowContext, WorkflowError, WorkflowOptions};
