/// Errors that can occur during instance management
#[derive(Debug, thiserror::Error)]
pub enum InstanceError {
    #[error("App not found: {0}")]
    AppNotFound(String),

    #[error("Failed to spawn instance: {0}")]
    SpawnError(std::io::Error),

    #[error("Failed to stop instance: {0}")]
    StopError(std::io::Error),

    #[error("Instance startup timeout")]
    StartupTimeout,

    #[error("Instance startup timeout: {0}")]
    StartupTimeoutWithDetail(String),

    #[error("Health check failed: {0}")]
    HealthCheckFailed(String),
}
