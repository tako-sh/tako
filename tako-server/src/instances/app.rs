use crate::socket::{AppState, InstanceState};
use dashmap::DashMap;
use parking_lot::RwLock;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use super::AppLogHandle;
use super::config::AppConfig;
use super::instance::{HealthyInstance, Instance, generate_instance_id, now_unix_millis};
use super::recovery::Recovery;

/// Manages all instances of an app
pub struct App {
    /// App configuration
    pub config: RwLock<AppConfig>,
    /// Running instances
    instances: DashMap<String, Arc<Instance>>,
    /// Instances currently eligible for request routing, with immutable
    /// request-path data captured when each instance becomes healthy.
    healthy_instances: RwLock<Vec<HealthyInstance>>,
    instance_transitions: parking_lot::Mutex<()>,
    /// Healthy instances that should not receive public traffic yet, such as
    /// a new rollout batch inside its stability window.
    routing_suppressed_instances: RwLock<HashSet<String>>,
    /// Current app state
    state: RwLock<AppState>,
    /// Serializes stop/restore against cold starts and health replacements.
    pub(crate) lifecycle_gate: Arc<tokio::sync::RwLock<()>>,

    /// Serializes health replacement and the periodic reconcile for this app.
    /// Only ever tried, never awaited, and taken after every other gate.
    pub(crate) recovery_gate: tokio::sync::Mutex<()>,
    /// True while a backup holds the deploy lock. A backup leaves the
    /// instance set alone, so recovery may claim instance slots meanwhile,
    /// holding this lock for the claim; see `BackupDeployLock`.
    pub(crate) recovery_claims_open: parking_lot::Mutex<bool>,
    /// Which background work holds the deploy lock, and whether an operator
    /// operation has priority over it; see `BackgroundDeployLock`.
    pub(crate) deploy_lock_access: parking_lot::Mutex<crate::runtime_events::DeployLockAccess>,
    /// Crash backoff and the last lost instance, for recovery and status.
    recovery: parking_lot::Mutex<Recovery>,

    /// Most recent error message (if any)
    last_error: RwLock<Option<String>>,
    /// Channel to notify about instance changes
    pub(super) instance_tx: mpsc::Sender<InstanceEvent>,
    /// Shared log handle for all instances of this app
    pub(super) log_handle: AppLogHandle,
    pub(super) compose_logs:
        parking_lot::Mutex<Option<(std::collections::BTreeSet<String>, tokio::process::Child)>>,
    /// Replicas per Compose service in the last planned rollout: the Compose
    /// default or the operator override. Status reads it, so a worker the
    /// Compose file declares with zero replicas runs as desired.
    compose_desired_replicas: RwLock<std::collections::BTreeMap<String, u32>>,
}

/// Events for instance lifecycle
#[derive(Debug)]
pub enum InstanceEvent {
    Started { app: String, instance_id: String },
    Ready { app: String, instance_id: String },
}

impl App {
    pub fn new(
        mut config: AppConfig,
        instance_tx: mpsc::Sender<InstanceEvent>,
        log_handle: AppLogHandle,
    ) -> Self {
        config.apply_internal_defaults();
        Self {
            config: RwLock::new(config),
            instances: DashMap::new(),
            healthy_instances: RwLock::new(Vec::new()),
            instance_transitions: parking_lot::Mutex::new(()),
            routing_suppressed_instances: RwLock::new(HashSet::new()),
            state: RwLock::new(AppState::Stopped),
            lifecycle_gate: Arc::new(tokio::sync::RwLock::new(())),
            recovery_gate: tokio::sync::Mutex::new(()),
            recovery_claims_open: parking_lot::Mutex::new(false),
            deploy_lock_access: parking_lot::Mutex::default(),
            recovery: parking_lot::Mutex::new(Recovery::default()),
            last_error: RwLock::new(None),
            instance_tx,
            log_handle,
            compose_logs: parking_lot::Mutex::new(None),
            compose_desired_replicas: RwLock::new(std::collections::BTreeMap::new()),
        }
    }

    pub(crate) fn set_compose_desired_replicas(
        &self,
        replicas: std::collections::BTreeMap<String, u32>,
    ) {
        *self.compose_desired_replicas.write() = replicas;
    }

    pub(crate) fn compose_desired_replicas(&self, service: &str) -> Option<u32> {
        self.compose_desired_replicas.read().get(service).copied()
    }

    pub fn name(&self) -> String {
        self.config.read().deployment_id()
    }

    pub fn version(&self) -> String {
        self.config.read().version.clone()
    }

    pub fn state(&self) -> AppState {
        *self.state.read()
    }

    pub fn set_state(&self, state: AppState) -> AppState {
        std::mem::replace(&mut *self.state.write(), state)
    }

    /// A cold start of the committed release can finish during a deployment.
    pub(crate) fn set_state_unless_deploying(&self, state: AppState) {
        let mut current = self.state.write();
        if *current != AppState::Deploying {
            *current = state;
        }
    }

    pub fn set_last_error(&self, message: impl Into<String>) {
        *self.last_error.write() = Some(message.into());
    }

    pub fn clear_last_error(&self) {
        *self.last_error.write() = None;
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.read().clone()
    }

    /// Record a lost instance and schedule the next replacement.
    pub(crate) fn record_instance_exit(&self, exit: tako_core::InstanceExit) {
        self.recovery.lock().record_exit(
            exit,
            Instant::now(),
            (now_unix_millis() / 1000) as i64,
            unit_jitter(),
        );
    }

    /// Time left before recovery may start another replacement.
    pub(crate) fn restart_backoff_remaining(&self, now: Instant) -> Option<Duration> {
        self.recovery.lock().backoff_remaining(now)
    }

    pub(crate) fn record_restart(&self) {
        self.recovery.lock().record_restart();
    }

    pub(crate) fn observe_recovery_health(&self, healthy: bool, now: Instant) {
        self.recovery.lock().observe_health(healthy, now);
    }

    pub(crate) fn has_recovery_history(&self) -> bool {
        self.recovery.lock().has_history()
    }

    /// Forget crash history, e.g. after a successful deploy or scale.
    pub(crate) fn reset_recovery(&self) {
        self.recovery.lock().reset();
    }

    pub(crate) fn recovery_status(&self) -> Option<tako_core::RecoveryStatus> {
        self.recovery.lock().status(Instant::now())
    }

    #[cfg(test)]
    pub(crate) fn healthy_instances(&self) -> Vec<Arc<Instance>> {
        self.healthy_instances
            .read()
            .iter()
            .map(|healthy| healthy.instance.clone())
            .collect()
    }

    pub(crate) fn set_instance_state(
        &self,
        instance: &Arc<Instance>,
        state: InstanceState,
    ) -> InstanceState {
        let _transition = self.instance_transitions.lock();
        self.set_instance_state_locked(instance, state)
    }

    pub(crate) fn set_instance_health_state(
        &self,
        instance: &Arc<Instance>,
        state: InstanceState,
    ) -> bool {
        let _transition = self.instance_transitions.lock();
        if matches!(
            instance.state(),
            InstanceState::Draining | InstanceState::Stopped
        ) {
            return false;
        }
        self.set_instance_state_locked(instance, state);
        true
    }

    fn set_instance_state_locked(
        &self,
        instance: &Arc<Instance>,
        state: InstanceState,
    ) -> InstanceState {
        let previous = instance.set_state(state);
        if previous == state {
            if state == InstanceState::Healthy {
                self.refresh_healthy_instance(instance);
            }
            return previous;
        }

        match (previous, state) {
            (InstanceState::Healthy, InstanceState::Healthy) => {}
            (InstanceState::Healthy, _) => self.remove_healthy_instance(&instance.id),
            (_, InstanceState::Healthy) => self.add_healthy_instance(instance),
            _ => {}
        }

        previous
    }

    pub(crate) fn suppress_instance_routing(&self, instance_id: &str) {
        let _transition = self.instance_transitions.lock();
        self.routing_suppressed_instances
            .write()
            .insert(instance_id.to_string());
        self.remove_healthy_instance(instance_id);
    }

    pub(crate) fn enable_instance_routing(&self, instance: &Arc<Instance>) -> bool {
        let _transition = self.instance_transitions.lock();
        self.enable_instance_routing_locked(instance)
    }

    fn enable_instance_routing_locked(&self, instance: &Arc<Instance>) -> bool {
        self.routing_suppressed_instances
            .write()
            .remove(&instance.id);
        if instance.state() == InstanceState::Healthy {
            self.add_healthy_instance(instance);
            return true;
        }
        false
    }

    pub(super) fn enable_rollout_routing(
        &self,
        candidates: &[Arc<Instance>],
        predecessors: &[Arc<Instance>],
        commit: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        // Health and routing cannot change between the final readiness check
        // and durable activation. A failed write leaves every predecessor live.
        let _transition = self.instance_transitions.lock();
        for instance in candidates {
            let state = instance.state();
            if state != InstanceState::Healthy {
                return Err(format!(
                    "Health check failed: Instance {} became {state} before rollout cutover",
                    instance.id
                ));
            }
        }
        let mut healthy = self.healthy_instances.write();
        commit()?;
        for old in predecessors {
            old.set_state(InstanceState::Draining);
        }
        let mut suppressed = self.routing_suppressed_instances.write();
        *healthy = candidates
            .iter()
            .map(|instance| {
                suppressed.remove(&instance.id);
                HealthyInstance {
                    instance: instance.clone(),
                    endpoint: instance.endpoint(),
                    service: instance.compose_service(),
                }
            })
            .collect();
        Ok(())
    }

    pub(crate) fn is_instance_routing_suppressed(&self, instance_id: &str) -> bool {
        self.routing_suppressed_instances
            .read()
            .contains(instance_id)
    }

    #[cfg(test)]
    pub(crate) fn healthy_instance_for_request(
        &self,
        request_index: usize,
    ) -> Option<Arc<Instance>> {
        self.healthy_backend_for_request(request_index)
            .map(|healthy| healthy.instance)
    }

    pub(crate) fn healthy_backend_for_request(
        &self,
        request_index: usize,
    ) -> Option<HealthyInstance> {
        let service = {
            let config = self.config.read();
            match &config.compose {
                Some(compose) => Some(compose.default_service().ok()?.to_owned()),
                None => None,
            }
        };
        self.healthy_service_backend_for_request(request_index, service.as_deref())
    }

    pub(crate) fn healthy_service_backend_for_request(
        &self,
        request_index: usize,
        service: Option<&str>,
    ) -> Option<HealthyInstance> {
        let instances = self.healthy_instances.read();
        let mut eligible = instances.iter().filter(|i| i.service.as_deref() == service);
        let count = eligible.clone().count();
        if count == 0 {
            return None;
        }
        eligible.nth(request_index % count).cloned()
    }

    fn add_healthy_instance(&self, instance: &Arc<Instance>) {
        if self
            .routing_suppressed_instances
            .read()
            .contains(&instance.id)
        {
            return;
        }

        let mut healthy_instances = self.healthy_instances.write();
        if healthy_instances
            .iter()
            .any(|healthy| healthy.instance.id == instance.id)
        {
            return;
        }

        healthy_instances.push(HealthyInstance {
            instance: instance.clone(),
            endpoint: instance.endpoint(),
            service: instance.compose_service(),
        });
    }

    fn refresh_healthy_instance(&self, instance: &Arc<Instance>) {
        let mut healthy_instances = self.healthy_instances.write();
        if let Some(healthy) = healthy_instances
            .iter_mut()
            .find(|healthy| healthy.instance.id == instance.id)
        {
            healthy.endpoint = instance.endpoint();
        }
    }

    fn remove_healthy_instance(&self, instance_id: &str) {
        self.healthy_instances
            .write()
            .retain(|healthy| healthy.instance.id != instance_id);
    }

    /// Get all healthy instances
    #[cfg(test)]
    pub(crate) fn get_healthy_instances(&self) -> Vec<Arc<Instance>> {
        self.healthy_instances()
    }

    pub(crate) fn healthy_instance_count(&self) -> usize {
        self.healthy_instances.read().len()
    }

    #[cfg(test)]
    pub(crate) fn healthy_instance_at(&self, healthy_index: usize) -> Option<Arc<Instance>> {
        self.healthy_instances
            .read()
            .get(healthy_index)
            .map(|healthy| healthy.instance.clone())
    }

    pub(crate) fn has_starting_instance(&self) -> bool {
        if !self.routing_suppressed_instances.read().is_empty() {
            return true;
        }

        self.instances.iter().any(|entry| {
            matches!(
                entry.value().state(),
                InstanceState::Starting | InstanceState::Ready
            )
        })
    }

    /// Get instance by ID
    pub fn get_instance(&self, id: &str) -> Option<Arc<Instance>> {
        self.instances.get(id).map(|entry| entry.value().clone())
    }

    /// Get all instances
    pub fn get_instances(&self) -> Vec<Arc<Instance>> {
        self.instances
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// Allocate a new instance (doesn't start it yet)
    pub fn allocate_instance(&self) -> Arc<Instance> {
        self.allocate_instance_for(&self.config.read().version)
    }

    /// A Compose HTTP replica. SDK services answer with the group's token,
    /// which stays valid when the server re-attaches running containers.
    pub(crate) fn allocate_compose_instance(
        &self,
        runtime: &tako_compose::ComposeRuntime,
        service: &str,
    ) -> Arc<Instance> {
        let token = runtime
            .sdk()
            .filter(|_| runtime.config.is_sdk(service))
            .map(|sdk| sdk.token.clone());
        let id = generate_instance_id();
        let instance = Arc::new(Instance::new_inner_with_token(
            id.clone(),
            self.config.read().version.clone(),
            self.log_handle.clone(),
            token,
        ));
        self.instances.insert(id, instance.clone());
        instance
    }

    pub(super) fn allocate_instance_for(&self, version: &str) -> Arc<Instance> {
        let id = generate_instance_id();
        let instance = Arc::new(Instance::new_inner(
            id.clone(),
            version.to_string(),
            self.log_handle.clone(),
        ));
        self.instances.insert(id, instance.clone());
        instance
    }

    /// Remove an instance
    pub fn remove_instance(&self, id: &str) -> Option<Arc<Instance>> {
        let removed = self.instances.remove(id).map(|(_, v)| v);
        if removed.is_some() {
            self.routing_suppressed_instances.write().remove(id);
            self.remove_healthy_instance(id);
        }
        removed
    }

    pub(crate) fn stop_compose_logs(&self) {
        // Docker commands use kill_on_drop; this only stops the log follower.
        self.compose_logs.lock().take();
    }

    pub(crate) async fn sync_compose_logs(
        &self,
        runtime: &tako_compose::ComposeRuntime,
        containers: &[tako_compose::ServiceContainer],
    ) -> Result<(), String> {
        let dependencies: Vec<_> = containers
            .iter()
            .filter(|container| !runtime.is_ingress(&container.service))
            .cloned()
            .collect();
        let ids = dependencies
            .iter()
            .map(|container| container.id.clone())
            .collect();
        {
            let mut follower = self.compose_logs.lock();
            if let Some((current, child)) = follower.as_mut()
                && *current == ids
                && matches!(child.try_wait(), Ok(None))
            {
                return Ok(());
            }
        }
        if dependencies.is_empty() {
            self.stop_compose_logs();
            return Ok(());
        }
        let mut child = runtime.logs(&dependencies).await?;
        use super::logger::{LogStream, log_pipe};
        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(log_pipe(
                stdout,
                self.log_handle.clone(),
                "compose".into(),
                LogStream::Stdout,
            ));
        }
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(log_pipe(
                stderr,
                self.log_handle.clone(),
                "compose".into(),
                LogStream::Stderr,
            ));
        }
        *self.compose_logs.lock() = Some((ids, child));
        Ok(())
    }

    /// Update configuration (for reloads/deploys)
    pub fn update_config(&self, mut config: AppConfig) {
        config.apply_internal_defaults();
        if config.compose.is_none() {
            self.stop_compose_logs();
        }
        *self.config.write() = config;
    }
}

/// Unit-interval sample for restart jitter. Not security sensitive: the
/// per-process random hasher seed is enough to spread restarts apart.
fn unit_jitter() -> f64 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(now_unix_millis());
    (hasher.finish() >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_lookup_releases_config_before_waiting_for_cutover() {
        let (tx, _) = mpsc::channel(1);
        let app = Arc::new(App::new(
            AppConfig::default(),
            tx,
            super::super::logger::noop_log_handle(),
        ));
        let healthy = app.healthy_instances.write();
        let reader = app.clone();
        let (started, ready) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            started.send(()).unwrap();
            reader.healthy_backend_for_request(0)
        });
        ready.recv().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let can_commit = app.config.try_write_for(Duration::from_secs(1)).is_some();
        drop(healthy);
        thread.join().unwrap();
        assert!(
            can_commit,
            "Cutover must be able to update config while lookup waits for the healthy pool"
        );
    }
}
