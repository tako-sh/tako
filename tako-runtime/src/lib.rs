pub mod download;
pub mod plugin;
pub mod plugins;
mod types;

pub use download::{DownloadManager, resolve_latest_version};
pub use plugin::{PluginContext, RuntimePlugin, plugin_for_id, runtime_def_for};
pub use plugins::javascript::{
    detect_package_manager, find_js_project_root, read_package_manager_spec,
};

/// Find the project root for the given runtime, starting from `project_dir`.
///
/// For JS runtimes (bun/node): walks up to find the lockfile root.
/// For other runtimes: returns `project_dir` unchanged.
pub fn find_runtime_project_root(
    runtime_id: &str,
    project_dir: &std::path::Path,
) -> std::path::PathBuf {
    match runtime_id {
        "bun" | "node" => plugins::javascript::find_js_project_root(project_dir),
        _ => project_dir.to_path_buf(),
    }
}
pub use types::{
    DownloadDef, EntrypointDef, EnvsDef, ExtractDef, ManifestMainDef, PackageManagerDef, PresetDef,
    RuntimeDef, ServerDef, SymlinkDef, VersionSourceDef,
};

/// Known runtime IDs.
pub const KNOWN_RUNTIME_IDS: &[&str] = &["bun", "node", "go", "rust"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_def_for_returns_all_known_runtimes() {
        for &id in KNOWN_RUNTIME_IDS {
            let def =
                runtime_def_for(id, None).unwrap_or_else(|| panic!("missing runtime for {id}"));
            assert_eq!(def.id, id);
        }
    }

    #[test]
    fn runtime_def_for_returns_none_for_unknown() {
        assert!(runtime_def_for("python", None).is_none());
    }

    #[test]
    fn plugin_provides_package_manager_for_js_runtimes() {
        for &(runtime, package_manager) in &[("bun", "bun"), ("node", "pnpm")] {
            let def = runtime_def_for(runtime, None)
                .unwrap_or_else(|| panic!("missing runtime for {runtime}"));
            assert_eq!(def.package_manager.id, package_manager);
            assert!(def.package_manager.add.is_some());
            assert!(def.package_manager.install.is_some());
        }
    }
}
