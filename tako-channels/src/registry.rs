use crate::ChannelDefinitionMeta;
use parking_lot::RwLock;
use std::collections::HashMap;

/// Cache of channel metadata keyed by `(app, channel)`.
///
/// The proxy hydrates one app's definitions at a time from the app's internal
/// channel registry endpoint, then invalidates them when that app's instance
/// pool turns over.
#[derive(Default)]
pub struct ChannelRegistry {
    by_app: RwLock<HashMap<String, HashMap<String, ChannelDefinitionMeta>>>,
}

impl ChannelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, app: &str, channel: &str) -> Option<ChannelDefinitionMeta> {
        self.by_app.read().get(app)?.get(channel).cloned()
    }

    pub fn install(&self, app: &str, definitions: Vec<ChannelDefinitionMeta>) {
        let definitions = definitions
            .into_iter()
            .map(|definition| (definition.channel.clone(), definition))
            .collect();
        self.by_app.write().insert(app.to_string(), definitions);
    }

    pub fn invalidate(&self, app: &str) {
        self.by_app.write().remove(app);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChannelAuthScheme, ChannelDefinitionMeta};
    use serde_json::json;

    fn meta(name: &str) -> ChannelDefinitionMeta {
        ChannelDefinitionMeta {
            channel: name.to_string(),
            params_schema: json!({ "type": "object" }),
            auth: ChannelAuthScheme::Public,
            transport: None,
        }
    }

    #[test]
    fn install_and_lookup() {
        let registry = ChannelRegistry::new();
        registry.install("app1", vec![meta("chat"), meta("status")]);

        assert_eq!(registry.get("app1", "chat").unwrap().channel, "chat");
        assert!(registry.get("app1", "missing").is_none());
        assert!(registry.get("app2", "chat").is_none());
    }

    #[test]
    fn install_replaces_existing_app_definitions() {
        let registry = ChannelRegistry::new();
        registry.install("app1", vec![meta("chat")]);
        registry.install("app1", vec![meta("status")]);

        assert!(registry.get("app1", "chat").is_none());
        assert_eq!(registry.get("app1", "status").unwrap().channel, "status");
    }

    #[test]
    fn invalidate_drops_the_app() {
        let registry = ChannelRegistry::new();
        registry.install("app1", vec![meta("chat")]);
        registry.invalidate("app1");

        assert!(registry.get("app1", "chat").is_none());
    }
}
