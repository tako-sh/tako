use std::path::Path;

use crate::RuntimeDef;

/// Context provided to a plugin when producing a RuntimeDef.
pub struct PluginContext<'a> {
    /// Project directory (the app directory with package.json / tako.toml).
    pub project_dir: &'a Path,
    /// Explicit package manager from tako.toml, if set by the user.
    pub package_manager: Option<&'a str>,
}

/// A runtime plugin produces a RuntimeDef dynamically based on project context.
///
/// Plugins own all behavioral logic (install commands, build commands, launch
/// args, entrypoint candidates). The registry TOML files are just constants
/// (download URLs, version sources) that plugins read however they want.
pub trait RuntimePlugin: Send + Sync {
    fn id(&self) -> &'static str;
    fn language(&self) -> &'static str;

    /// Produce a full RuntimeDef for the given project context.
    fn runtime_def(&self, ctx: &PluginContext) -> RuntimeDef;

    /// Produce a RuntimeDef with sensible defaults (no project context).
    /// Used by tako-server as a fallback when the manifest is incomplete.
    fn default_runtime_def(&self) -> RuntimeDef;
}

/// Look up a plugin by runtime ID.
pub fn plugin_for_id(id: &str) -> Option<&'static dyn RuntimePlugin> {
    match id {
        "bun" => Some(&super::plugins::javascript::BunPlugin),
        "node" => Some(&super::plugins::javascript::NodePlugin),
        "go" => Some(&super::plugins::go::GoPlugin),
        _ => None,
    }
}

/// Produce a RuntimeDef via the plugin system.
/// Returns None for unknown runtime IDs.
pub fn runtime_def_for(id: &str, ctx: Option<&PluginContext>) -> Option<RuntimeDef> {
    let plugin = plugin_for_id(id)?;
    Some(match ctx {
        Some(ctx) => plugin.runtime_def(ctx),
        None => plugin.default_runtime_def(),
    })
}
