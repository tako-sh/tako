//! Load balancer - routes requests to healthy instances
//!
//! Features:
//! - Round-robin load balancing
//! - Health-aware routing
//! - On-demand instance spawning

use crate::instances::{App, AppManager, Instance};
use dashmap::DashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Load balancer for a single app
pub struct AppLoadBalancer {
    /// App reference
    app: Arc<App>,
    /// Round-robin counter
    rr_counter: AtomicUsize,
}

impl AppLoadBalancer {
    pub fn new(app: Arc<App>) -> Self {
        Self {
            app,
            rr_counter: AtomicUsize::new(0),
        }
    }

    /// Get an instance to handle a request
    pub fn get_instance(&self) -> Option<Arc<Instance>> {
        let healthy = self.app.get_healthy_instances();
        if healthy.is_empty() {
            return None;
        }
        let idx = self.rr_counter.fetch_add(1, Ordering::Relaxed) % healthy.len();
        Some(healthy[idx].clone())
    }
}

/// Global load balancer managing all apps
pub struct LoadBalancer {
    /// Per-app load balancers
    app_lbs: DashMap<String, Arc<AppLoadBalancer>>,
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
        let lb = Arc::new(AppLoadBalancer::new(app));
        self.app_lbs.insert(name, lb);
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
            instance_id: instance.id.clone(),
            endpoint: instance.endpoint(),
        })
    }

    /// Mark request completed
    pub fn request_completed(&self, _app_name: &str, _instance_id: &str) {
        // Retained for proxy request lifecycle symmetry; round-robin does not
        // need completion bookkeeping.
    }

    /// Get app manager
    pub fn app_manager(&self) -> &Arc<AppManager> {
        &self.app_manager
    }
}

/// A selected backend for a request
#[derive(Debug, Clone)]
pub struct Backend {
    /// App name
    pub app_name: String,
    /// Instance ID
    pub instance_id: String,
    /// Optional TCP endpoint for upstream proxying
    pub endpoint: Option<SocketAddr>,
}

impl Backend {
    pub fn endpoint(&self) -> Option<SocketAddr> {
        self.endpoint
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
        assert_eq!(backend.instance_id, instance.id);
        assert_eq!(backend.endpoint(), None);
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
        for _ in 0..50_000 {
            let backend = lb.get_backend("test-app").expect("backend should exist");
            lb.request_completed("test-app", &backend.instance_id);
        }
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "get_backend perf smoke threshold exceeded: {:?}",
            start.elapsed()
        );
    }
}
