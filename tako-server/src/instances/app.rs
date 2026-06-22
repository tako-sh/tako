use crate::socket::{AppState, InstanceState};
use dashmap::DashMap;
use parking_lot::RwLock;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::mpsc;

use super::AppLogHandle;
use super::config::AppConfig;
use super::instance::{HealthyInstance, Instance, generate_instance_id};

/// Manages all instances of an app
pub struct App {
    /// App configuration
    pub config: RwLock<AppConfig>,
    /// Running instances
    instances: DashMap<String, Arc<Instance>>,
    /// Instances currently eligible for request routing, with immutable
    /// request-path data captured when each instance becomes healthy.
    healthy_instances: RwLock<Vec<HealthyInstance>>,
    /// Healthy instances that should not receive public traffic yet, such as
    /// a new rollout batch inside its stability window.
    routing_suppressed_instances: RwLock<HashSet<String>>,
    /// Current app state
    state: RwLock<AppState>,

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
            routing_suppressed_instances: RwLock::new(HashSet::new()),
            state: RwLock::new(AppState::Stopped),
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
        self.routing_suppressed_instances
            .write()
            .insert(instance_id.to_string());
        self.remove_healthy_instance(instance_id);
    }

    pub(crate) fn enable_instance_routing(&self, instance: &Arc<Instance>) -> bool {
        self.routing_suppressed_instances
            .write()
            .remove(&instance.id);
        if instance.state() == InstanceState::Healthy {
            self.add_healthy_instance(instance);
            return true;
        }
        false
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
