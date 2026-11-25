use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use tako_core::{SourceIpMode, compose::ComposeConfig};

use super::{StateStore, StateStoreError};

/// Server-owned bindings committed with a release. Uploaded manifests cannot
/// grant route ownership or change the source-IP trust policy of an old release.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ReleaseRouting {
    pub routes: Vec<String>,
    pub source_ip: SourceIpMode,
    pub source_ips: BTreeMap<String, SourceIpMode>,
}

impl ReleaseRouting {
    pub(crate) fn new(
        routes: Vec<String>,
        source_ip: SourceIpMode,
        compose: Option<&ComposeConfig>,
        environment: &str,
    ) -> Self {
        let mut source_ips: BTreeMap<_, _> = routes
            .iter()
            .map(|route| (route.clone(), source_ip))
            .collect();
        if let Some(compose) = compose {
            for (_, settings) in compose.routed_services(environment) {
                for route in settings.routes() {
                    source_ips.insert(route, settings.source_ip.unwrap_or(source_ip));
                }
            }
        }
        Self {
            routes,
            source_ip,
            source_ips,
        }
    }
}

impl StateStore {
    pub(crate) fn release_routing(
        &self,
        app: &str,
        version: &str,
    ) -> Result<Option<ReleaseRouting>, StateStoreError> {
        self.read(|r| super::get(r, ("app_release_routing", app, version)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instances::AppConfig;

    #[test]
    fn release_bindings_survive_private_deploy_restart_and_failed_commit() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().join("state.redb"), [0; 32]);
        store.init().unwrap();
        let public = AppConfig {
            name: "demo".into(),
            environment: "production".into(),
            version: "public".into(),
            source_ip: SourceIpMode::Direct,
            ..Default::default()
        };
        store.upsert_app(&public, &["demo.example".into()]).unwrap();

        let private = AppConfig {
            version: "private".into(),
            ..public.clone()
        };
        store.commit_deployment(&private, &[], None, None).unwrap();
        let expected = ReleaseRouting::new(
            vec!["demo.example".into()],
            SourceIpMode::Direct,
            None,
            "production",
        );
        assert_eq!(
            store.release_routing("demo/production", "public").unwrap(),
            Some(expected.clone())
        );
        drop(store);
        let store = StateStore::new(dir.path().join("state.redb"), [0; 32]);
        store.init().unwrap();
        store
            .reject_writes
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(
            store
                .commit_deployment(&public, &["changed.example".into()], None, None)
                .is_err()
        );
        assert_eq!(
            store.release_routing("demo/production", "public").unwrap(),
            Some(expected)
        );
        assert!(
            store
                .release_routing("demo/production", "private")
                .unwrap()
                .unwrap()
                .routes
                .is_empty()
        );
        store
            .reject_writes
            .store(false, std::sync::atomic::Ordering::Relaxed);
        store.delete_app("demo", "production").unwrap();
        assert!(
            store
                .release_routing("demo/production", "public")
                .unwrap()
                .is_none()
        );
    }
}
