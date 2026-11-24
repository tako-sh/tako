use std::path::Path;

use crate::build::BuildAdapter;
use crate::config::TakoToml;

/// Whether the release runs workflows, so tako-server needs a database for
/// their runs. Mirrors when the server installs a workflow runtime: a JS
/// `workflows/` directory with source files, a Go `cmd/worker`, a container
/// `[workflows].run` command, or a Compose group with SDK services (whose
/// HTTP services may enqueue for external workers).
pub(in crate::commands::deploy) fn needs_workflows(
    project_dir: &Path,
    tako_config: &TakoToml,
    adapter: BuildAdapter,
) -> bool {
    if let Some(compose) = &tako_config.compose {
        return compose.has_sdk_services();
    }
    if tako_config.container.is_some() || tako_config.image.is_some() {
        return tako_config.workflows.base.run.is_some()
            || tako_config
                .workflows
                .groups
                .values()
                .any(|group| group.run.is_some());
    }
    if adapter == BuildAdapter::Go {
        return project_dir.join("cmd/worker/main.go").is_file();
    }
    let workflows_dir =
        crate::build::js::js_app_root_dir(project_dir, tako_config.js_app_root()).join("workflows");
    match std::fs::read_dir(workflows_dir) {
        Ok(entries) => entries.into_iter().any(|entry| match entry {
            Ok(entry) => is_js_runtime_source_file(&entry.path()),
            // An unreadable directory fails the build later; provision
            // rather than deploy a runtime without storage.
            Err(_) => true,
        }),
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    }
}

/// Whether the project runs workflows, which keep their runs in the
/// environment's workflow database.
pub(crate) fn project_needs_workflows(project_dir: &Path, tako_config: &TakoToml) -> bool {
    let adapter =
        super::resolve_build_adapter(project_dir, tako_config).unwrap_or(BuildAdapter::Unknown);
    needs_workflows(project_dir, tako_config, adapter)
}

/// Whether the project's code uses channels, presence or stores, which keep
/// their history in the environment's channel replay backend.
pub(crate) fn project_needs_channel_replay(project_dir: &Path, tako_config: &TakoToml) -> bool {
    let adapter =
        super::resolve_build_adapter(project_dir, tako_config).unwrap_or(BuildAdapter::Unknown);
    needs_channel_replay(project_dir, tako_config, adapter)
}

pub(in crate::commands::deploy) fn needs_channel_replay(
    project_dir: &Path,
    tako_config: &TakoToml,
    adapter: BuildAdapter,
) -> bool {
    if let Some(compose) = &tako_config.compose {
        return compose.has_sdk_services();
    }
    if tako_config.container.is_some() || tako_config.image.is_some() {
        return tako_config.container_options.sdk;
    }
    // JS definitions are discovered from files. Go and custom programs register
    // channels at runtime through a Tako SDK, so they need one to use replay.
    if adapter.preset_group() != crate::build::PresetGroup::Js {
        let source_root =
            crate::commands::deploy::project::source_bundle_root(project_dir, adapter.id());
        if tako_runtime::app_uses_sdk(adapter.id(), project_dir, &source_root) {
            return true;
        }
    }
    let app_root = crate::build::js::js_app_root_dir(project_dir, tako_config.js_app_root());
    ["channels", "presence", "stores"].iter().any(|kind| {
        let entries = match std::fs::read_dir(app_root.join(kind)) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
            Err(_) => return true,
        };
        entries.into_iter().any(|entry| match entry {
            Ok(entry) => is_js_runtime_source_file(&entry.path()),
            Err(_) => true,
        })
    })
}

/// The rule the server applies too, so both agree on what a release runs.
fn is_js_runtime_source_file(path: &Path) -> bool {
    tako_workflows::is_js_definition_source(path)
}
