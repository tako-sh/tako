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
    log_handle: AppLogHandle,
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
        }
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

    pub fn set_state(&self, state: AppState) {
        *self.state.write() = state;
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
        instances: &[Arc<Instance>],
    ) -> Result<(), (String, InstanceState)> {
        // A health transition must either fail this rollout or occur after
        // the instances have been handed to ordinary health recovery.
        let _transition = self.instance_transitions.lock();
        for instance in instances {
            let state = instance.state();
            if state != InstanceState::Healthy {
                return Err((instance.id.clone(), state));
            }
        }
        for instance in instances {
            self.enable_instance_routing_locked(instance);
        }
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
        let instances = self.healthy_instances.read();
        if instances.is_empty() {
            return None;
        }

        Some(instances[request_index % instances.len()].clone())
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

    #[cfg(test)]
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
        let id = generate_instance_id();
        let config = self.config.read();
        let instance = Arc::new(Instance::new_inner(
            id.clone(),
            config.version.clone(),
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

    /// Update configuration (for reloads/deploys)
    pub fn update_config(&self, mut config: AppConfig) {
        config.apply_internal_defaults();
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
