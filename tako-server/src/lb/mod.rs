//! Load balancer - routes requests to healthy instances
//!
//! Features:
//! - Round-robin load balancing
//! - Health-aware routing
//! - On-demand instance spawning

use crate::instances::{App, AppManager, Instance};
use dashmap::DashMap;
use parking_lot::RwLock;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

struct HealthySnapshot {
    generation: u64,
    instances: Vec<Arc<Instance>>,
}

/// Load balancer for a single app
pub struct AppLoadBalancer {
    /// App reference
    app: Arc<App>,
    /// Round-robin counter
    rr_counter: AtomicUsize,
    /// Cached healthy instances for the current app lifecycle generation.
    healthy_snapshot: RwLock<HealthySnapshot>,
}

impl AppLoadBalancer {
    pub fn new(app: Arc<App>) -> Self {
        Self {
            app,
            rr_counter: AtomicUsize::new(0),
            healthy_snapshot: RwLock::new(HealthySnapshot {
                generation: u64::MAX,
                instances: Vec::new(),
            }),
        }
    }

    /// Get an instance to handle a request
    pub fn get_instance(&self) -> Option<Arc<Instance>> {
        loop {
            let generation = self.app.instance_generation();
            {
                let snapshot = self.healthy_snapshot.read();
                if snapshot.generation == generation {
                    return self.select_cached_instance(&snapshot.instances);
                }
            }

            self.refresh_snapshot();
        }
    }

    fn select_cached_instance(&self, instances: &[Arc<Instance>]) -> Option<Arc<Instance>> {
        if instances.is_empty() {
            return None;
        }

        let idx = self.rr_counter.fetch_add(1, Ordering::Relaxed) % instances.len();
        Some(instances[idx].clone())
    }

    fn refresh_snapshot(&self) {
        let (generation, instances) = self.read_healthy_snapshot();
        let mut snapshot = self.healthy_snapshot.write();
        if snapshot.generation != u64::MAX && snapshot.generation >= generation {
            return;
        }

        snapshot.generation = generation;
        snapshot.instances = instances;
    }

    fn read_healthy_snapshot(&self) -> (u64, Vec<Arc<Instance>>) {
        loop {
            let generation = self.app.instance_generation();
            let instances = self.app.healthy_instances();
            let after = self.app.instance_generation();
            if generation == after {
                return (after, instances);
            }
        }
    }
}

/// Global load balancer managing all apps
pub struct LoadBalancer {
    /// Per-app load balancers
    app_lbs: DashMap<String, AppLoadBalancer>,
    /// App manager reference
    app_manager: Arc<AppManager>,
}

impl LoadBalancer {
    pub fn new(app_manager: Arc<AppManager>) -> Self {
        Self {
            app_lbs: DashMap::new(),
            app_manager,
        }
    }

    /// Register an app with the load balancer
    pub fn register_app(&self, app: Arc<App>) {
        let name = app.name();
        self.app_lbs.insert(name, AppLoadBalancer::new(app));
    }

    /// Remove an app from the load balancer
    pub fn unregister_app(&self, name: &str) {
        self.app_lbs.remove(name);
    }

    /// Get a backend instance for a request
    pub fn get_backend(&self, app_name: &str) -> Option<Backend> {
        let lb = self.app_lbs.get(app_name)?;
        let instance = lb.get_instance()?;

        Some(Backend {
            app_name: app_name.to_string(),
            endpoint: instance.endpoint(),
            instance,
        })
    }

    /// Get app manager
    pub fn app_manager(&self) -> &Arc<AppManager> {
        &self.app_manager
    }
}

/// A selected backend for a request
pub struct Backend {
    /// App name
    pub app_name: String,
    /// Selected instance for request accounting and channel auth.
    instance: Arc<Instance>,
    /// Optional TCP endpoint for upstream proxying
    endpoint: Option<SocketAddr>,
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Backend")
            .field("app_name", &self.app_name)
            .field("instance_id", &self.instance_id())
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

impl Backend {
    pub fn endpoint(&self) -> Option<SocketAddr> {
        self.endpoint
    }

    pub fn instance(&self) -> &Instance {
        &self.instance
    }

    pub fn instance_id(&self) -> &str {
        &self.instance.id
    }

    pub fn request_started(&self) {
        self.instance.request_started();
    }

    pub fn request_finished(&self) {
        self.instance.request_finished();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instances::AppConfig;
    use crate::instances::logger::noop_log_handle;
    use crate::socket::InstanceState;
    use std::path::PathBuf;
    use tokio::sync::mpsc;

    fn create_test_app() -> Arc<App> {
        let (tx, _rx) = mpsc::channel(16);
        let config = AppConfig {
            name: "test-app".to_string(),
            ..Default::default()
        };
        Arc::new(App::new(config, tx, noop_log_handle()))
    }

    #[test]
    fn test_round_robin() {
        let app = create_test_app();

        // Allocate 3 instances and mark them healthy
        let i1 = app.allocate_instance();
        let i2 = app.allocate_instance();
        let i3 = app.allocate_instance();
        i1.set_state(InstanceState::Healthy);
        i2.set_state(InstanceState::Healthy);
        i3.set_state(InstanceState::Healthy);

        let lb = AppLoadBalancer::new(app);

        // Should cycle through instances
        let mut instance_ids = vec![];
        for _ in 0..6 {
            let instance = lb.get_instance().unwrap();
            instance_ids.push(instance.id.clone());
        }

        assert_eq!(instance_ids.iter().filter(|id| **id == i1.id).count(), 2);
        assert_eq!(instance_ids.iter().filter(|id| **id == i2.id).count(), 2);
        assert_eq!(instance_ids.iter().filter(|id| **id == i3.id).count(), 2);
    }

    #[test]
    fn test_no_healthy_instances() {
        let app = create_test_app();
        let i1 = app.allocate_instance();
        i1.set_state(InstanceState::Starting); // Not healthy yet

        let lb = AppLoadBalancer::new(app);
        assert!(lb.get_instance().is_none());
    }

    #[test]
    fn cached_healthy_instances_refresh_when_state_changes() {
        let app = create_test_app();
        let instance = app.allocate_instance();
        instance.set_state(InstanceState::Healthy);

        let lb = AppLoadBalancer::new(app.clone());
        let selected = lb
            .get_instance()
            .expect("healthy instance should be selected");
        assert_eq!(selected.id, instance.id);

        instance.set_state(InstanceState::Unhealthy);
        assert!(lb.get_instance().is_none());

        let replacement = app.allocate_instance();
        replacement.set_state(InstanceState::Healthy);
        let selected = lb
            .get_instance()
            .expect("replacement instance should be selected");
        assert_eq!(selected.id, replacement.id);
    }

    #[tokio::test]
    async fn test_global_load_balancer() {
        let manager = Arc::new(AppManager::new(PathBuf::from("/tmp/tako-test")));
        let lb = LoadBalancer::new(manager.clone());

        let config = AppConfig {
            name: "my-app".to_string(),
            ..Default::default()
        };
        let app = manager.register_app(config);

        // Allocate and make healthy
        let instance = app.allocate_instance();
        instance.set_state(InstanceState::Healthy);

        lb.register_app(app);

        let backend = lb.get_backend("my-app").unwrap();
        assert_eq!(backend.app_name, "my-app");
        assert_eq!(backend.instance_id(), instance.id);
        assert_eq!(backend.endpoint(), None);
    }

    #[tokio::test]
    async fn backend_tracks_requests_on_selected_instance() {
        let manager = Arc::new(AppManager::new(PathBuf::from("/tmp/tako-test")));
        let lb = LoadBalancer::new(manager.clone());

        let app = manager.register_app(AppConfig {
            name: "my-app".to_string(),
            ..Default::default()
        });
        lb.register_app(app.clone());

        let instance = app.allocate_instance();
        instance.set_state(InstanceState::Healthy);

        let backend = lb.get_backend("my-app").unwrap();
        backend.request_started();
        assert_eq!(instance.requests_total(), 1);
        assert_eq!(instance.in_flight(), 1);

        backend.request_finished();
        assert_eq!(instance.in_flight(), 0);
    }

    #[tokio::test]
    async fn test_global_load_balancer_returns_tcp_backend_when_port_is_bound() {
        let manager = Arc::new(AppManager::new(PathBuf::from("/tmp/tako-test")));
        let lb = LoadBalancer::new(manager.clone());

        let app = manager.register_app(AppConfig {
            name: "my-app".to_string(),
            ..Default::default()
        });
        lb.register_app(app.clone());

        let instance = app.allocate_instance();
        instance.set_port(47_831);
        instance.set_state(InstanceState::Healthy);

        let backend = lb
            .get_backend("my-app")
            .expect("backend should be selected");

        assert_eq!(
            backend.endpoint(),
            Some("127.0.0.1:47831".parse().expect("loopback socket addr"))
        );
    }

    #[tokio::test]
    async fn test_global_load_balancer_keeps_backend_when_port_is_not_bound_yet() {
        let manager = Arc::new(AppManager::new(PathBuf::from("/tmp/tako-test")));
        let lb = LoadBalancer::new(manager.clone());

        let app = manager.register_app(AppConfig {
            name: "test-app".to_string(),
            ..Default::default()
        });
        lb.register_app(app.clone());

        let instance = app.allocate_instance();
        instance.set_state(InstanceState::Healthy);

        let backend = lb
            .get_backend("test-app")
            .expect("backend should be selected");

        assert_eq!(backend.endpoint(), None);
    }

    #[tokio::test]
    async fn perf_smoke_get_backend_hot_path() {
        use std::time::{Duration, Instant};

        let manager = Arc::new(AppManager::new(PathBuf::from("/tmp/tako-test")));
        let lb = LoadBalancer::new(manager.clone());

        let app = manager.register_app(AppConfig {
            name: "test-app".to_string(),
            ..Default::default()
        });
        lb.register_app(app.clone());

        for _ in 0..8 {
            let instance = app.allocate_instance();
            instance.set_state(InstanceState::Healthy);
        }

        let start = Instant::now();
        for _ in 0..5_000 {
            let _backend = lb.get_backend("test-app").expect("backend should exist");
        }
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "get_backend perf smoke threshold exceeded: {:?}",
            start.elapsed()
        );
    }
}
