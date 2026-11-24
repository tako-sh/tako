/// Who sent a management command. A deploy client may not add a route that
/// shares requests with another app environment's route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Caller {
    Full,
    Deploy,
}
mod compose;
mod deploy;
mod dispatch;
pub(crate) use dispatch::command_is_read_only;
mod drain;
mod lifecycle;
pub(crate) mod logs;
mod queries;
mod release_artifacts;
pub(crate) use release_artifacts::{RecoveryMode, available_bytes, sweep_release_staging_dirs};
mod releases;
mod retired_archives;
#[cfg(test)]
pub(crate) use releases::ReleaseRun;
pub(crate) use retired_archives::sweep_retired_archive_state;
mod tls;
mod workflows;
