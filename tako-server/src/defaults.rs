use std::time::Duration;

pub const HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(1);
/// Fast probe interval used while any instance is still in startup
/// (Starting/Ready, not yet Healthy) or withheld from routing during rollout
/// stability. Once all instances are stable, the loop falls back to
/// `HEALTH_CHECK_INTERVAL`.
pub const HEALTH_STARTUP_CHECK_INTERVAL: Duration = Duration::from_millis(100);
pub const HEALTH_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
pub const IDLE_CHECK_INTERVAL_DEBUG: Duration = Duration::from_secs(1);
pub const IDLE_CHECK_INTERVAL_RELEASE: Duration = Duration::from_secs(30);
