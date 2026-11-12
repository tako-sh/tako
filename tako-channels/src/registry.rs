use crate::{ChannelDefinitionMeta, ChannelError};
use parking_lot::RwLock;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::Arc;
use tokio::sync::OnceCell;

type Definitions = HashMap<String, ChannelDefinitionMeta>;
type RegistryCell = Arc<OnceCell<Definitions>>;

/// What an already loaded registry says about one channel.
#[derive(Debug, Clone, PartialEq)]
pub enum CachedDefinition {
    /// No complete registry of this build is loaded: nothing is known.
    NotLoaded,
    /// The loaded registry of this build does not define the channel.
    Absent,
    Defined(ChannelDefinitionMeta),
}

/// Complete registries, including absent channels, cached by app and build.
/// Keep both sides of a rolling deploy without retaining every historical build.
#[derive(Default)]
pub struct ChannelRegistry {
    by_app: RwLock<HashMap<String, VecDeque<(String, RegistryCell)>>>,
}

impl ChannelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn get_or_load<F, Fut>(
        &self,
        app: &str,
        build: &str,
        channel: &str,
        load: F,
    ) -> Result<ChannelDefinitionMeta, ChannelError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Vec<ChannelDefinitionMeta>, ChannelError>>,
    {
        let existing = self.by_app.read().get(app).and_then(|builds| {
            builds
                .iter()
                .find(|(version, _)| version == build)
                .map(|(_, cell)| cell.clone())
        });
        let cell = existing.unwrap_or_else(|| {
            let mut cache = self.by_app.write();
            let builds = cache.entry(app.to_owned()).or_default();
            if let Some((_, cell)) = builds.iter().find(|(version, _)| version == build) {
                return cell.clone();
            }
            let cell = Arc::new(OnceCell::new());
            if builds.len() == 2 {
                builds.pop_front();
            }
            builds.push_back((build.to_owned(), cell.clone()));
            cell
        });
        // Invalidation detaches the cell: an older fetch may finish for its
        // existing caller but cannot repopulate the current app registry.
        let definitions = cell
            .get_or_try_init(|| async {
                Ok::<_, ChannelError>(
                    load()
                        .await?
                        .into_iter()
                        .map(|definition| (definition.channel.clone(), definition))
                        .collect(),
                )
            })
            .await?;
        definitions
            .get(channel)
            .cloned()
            .ok_or(ChannelError::NotDefined)
    }

    /// A definition from an already loaded registry of exactly this build.
    /// Never loads: publishers must not wait for, or wake, an app instance.
    /// Distinguishes a registry not loaded yet from one that lacks the channel.
    pub fn cached(&self, app: &str, build: &str, channel: &str) -> CachedDefinition {
        let cell = self.by_app.read().get(app).and_then(|builds| {
            builds
                .iter()
                .find(|(version, _)| version == build)
                .map(|(_, cell)| cell.clone())
        });
        match cell.as_ref().and_then(|cell| cell.get()) {
            None => CachedDefinition::NotLoaded,
            Some(definitions) => definitions
                .get(channel)
                .cloned()
                .map_or(CachedDefinition::Absent, CachedDefinition::Defined),
        }
    }

    pub fn invalidate(&self, app: &str) {
        self.by_app.write().remove(app);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChannelAuthScheme;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn meta(name: &str) -> ChannelDefinitionMeta {
        ChannelDefinitionMeta {
            channel: name.into(),
            params_schema: serde_json::json!({"type":"object"}),
            auth: ChannelAuthScheme::Public,
            transport: None,
            lifecycle: Default::default(),
        }
    }

    #[tokio::test]
    async fn concurrent_missing_channels_load_registry_only_once() {
        let registry = ChannelRegistry::new();
        let loads = AtomicUsize::new(0);
        let results = futures_util::future::join_all((0..32).map(|_| {
            registry.get_or_load("app", "v1", "missing", || async {
                loads.fetch_add(1, Ordering::SeqCst);
                tokio::task::yield_now().await;
                Ok(vec![meta("chat")])
            })
        }))
        .await;
        assert!(
            results
                .iter()
                .all(|result| matches!(result, Err(ChannelError::NotDefined)))
        );
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        assert_eq!(
            registry
                .get_or_load("app", "v1", "chat", || async {
                    panic!("complete registry must be reused")
                })
                .await
                .unwrap()
                .channel,
            "chat"
        );
    }

    #[tokio::test]
    async fn builds_and_apps_have_separate_negative_caches() {
        let registry = ChannelRegistry::new();
        assert!(
            registry
                .get_or_load("app", "v1", "new", || async { Ok(vec![]) })
                .await
                .is_err()
        );
        assert!(
            registry
                .get_or_load("app", "v2", "new", || async { Ok(vec![meta("new")]) })
                .await
                .is_ok()
        );
        assert!(
            registry
                .get_or_load("app", "v1", "new", || async {
                    panic!("old build stays cached")
                })
                .await
                .is_err()
        );
        assert!(
            registry
                .get_or_load("other", "v1", "new", || async { Ok(vec![meta("new")]) })
                .await
                .is_ok()
        );
        registry
            .get_or_load("app", "v3", "new", || async { Ok(vec![meta("new")]) })
            .await
            .unwrap();
        assert_eq!(registry.by_app.read()["app"].len(), 2);
    }

    #[tokio::test]
    async fn cached_definitions_are_exact_to_a_loaded_build() {
        let registry = ChannelRegistry::new();
        assert_eq!(
            registry.cached("app", "v1", "chat"),
            CachedDefinition::NotLoaded
        );
        registry
            .get_or_load("app", "v1", "chat", || async { Ok(vec![meta("chat")]) })
            .await
            .unwrap();
        assert_eq!(
            registry.cached("app", "v1", "chat"),
            CachedDefinition::Defined(meta("chat"))
        );
        assert_eq!(
            registry.cached("app", "v1", "other"),
            CachedDefinition::Absent
        );
        assert_eq!(
            registry.cached("app", "v2", "chat"),
            CachedDefinition::NotLoaded
        );
        assert_eq!(
            registry.cached("other", "v1", "chat"),
            CachedDefinition::NotLoaded
        );
        registry.invalidate("app");
        assert_eq!(
            registry.cached("app", "v1", "chat"),
            CachedDefinition::NotLoaded
        );
    }

    #[tokio::test]
    async fn failed_fetch_can_retry() {
        let registry = ChannelRegistry::new();
        assert!(
            registry
                .get_or_load("app", "v1", "chat", || async {
                    Err(ChannelError::AuthUnavailable)
                })
                .await
                .is_err()
        );
        assert!(
            registry
                .get_or_load("app", "v1", "chat", || async { Ok(vec![meta("chat")]) })
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn invalidation_during_fetch_does_not_restore_old_registry() {
        let registry = ChannelRegistry::new();
        registry
            .get_or_load("app", "v1", "old", || async {
                registry.invalidate("app");
                Ok(vec![meta("old")])
            })
            .await
            .unwrap();
        assert!(
            registry
                .get_or_load("app", "v1", "new", || async { Ok(vec![meta("new")]) })
                .await
                .is_ok()
        );
    }
}
