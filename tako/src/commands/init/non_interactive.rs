use std::fs;
use std::path::Path;

use crate::app::resolve_app_name;
use crate::build::{BuildAdapter, PresetGroup};
use crate::config::TakoToml;
use crate::output;

use super::generated::write_init_generated_file;
use super::project::ensure_project_gitignore_tracks_secrets;
use super::scaffold::{
    TemplateParams, detect_js_app_root, detect_local_runtime_version, generate_template,
    infer_default_main_entrypoint, preset_default_main, sanitize_route, sdk_install_command,
};

pub(super) fn run_non_interactive(
    project_dir: &Path,
    tako_toml_path: &Path,
    detected_adapter: BuildAdapter,
    existing: Option<&TakoToml>,
) -> Result<(), Box<dyn std::error::Error>> {
    let adapter = resolve_adapter(detected_adapter, existing);
    let preset = adapter.default_preset().to_string();
    let preset_dm = preset_default_main(&preset, adapter, &[]);

    let inferred_main = adapter.infer_main_entrypoint(project_dir);
    let main = if let Some(inferred) = inferred_main {
        if preset_dm.as_deref() == Some(inferred.as_str()) {
            None
        } else {
            Some(inferred)
        }
    } else if preset_dm.is_some() {
        None
    } else {
        Some(
            existing
                .and_then(|c| c.main.clone())
                .unwrap_or_else(|| infer_default_main_entrypoint(project_dir, adapter)),
        )
    };

    let app_name = existing
        .and_then(|c| c.name.clone())
        .unwrap_or_else(|| resolve_app_name(project_dir).unwrap_or_else(|_| "my-app".to_string()));

    let production_route = existing
        .and_then(|c| c.envs.get("production").and_then(|e| e.route.clone()))
        .unwrap_or_else(|| format!("{}.example.com", app_name.trim()));

    let runtime_version = detect_local_runtime_version(adapter.version_probe_tool());
    let app_root = if adapter.preset_group() == PresetGroup::Js {
        Some(
            existing
                .and_then(|c| c.app_root.as_ref().map(|root| root.trim().to_string()))
                .unwrap_or_else(|| detect_js_app_root(project_dir)),
        )
    } else {
        None
    };

    let detected_pm = tako_runtime::detect_package_manager(project_dir);
    let pm_for_toml = detected_pm.map(|pm| pm.id().to_string()).filter(|pm_id| {
        let default_pm = tako_runtime::plugin_for_id(adapter.id())
            .map(|p| p.default_runtime_def().package_manager.id)
            .unwrap_or_default();
        *pm_id != default_pm
    });

    let template = generate_template(&TemplateParams {
        app_name: app_name.trim(),
        app_root: app_root.as_deref(),
        main: main.as_deref().map(str::trim),
        production_route: &sanitize_route(&production_route),
        runtime: Some(adapter.id()),
        runtime_version: runtime_version.as_deref(),
        package_manager: pm_for_toml.as_deref(),
        preset_ref: None,
        assets: &[],
        excludes: &[],
    });

    let parsed_template = TakoToml::parse(&template)?;
    fs::write(tako_toml_path, template)?;
    ensure_project_gitignore_tracks_secrets(project_dir)?;

    if let Some(generated_file) =
        write_init_generated_file(project_dir, adapter, parsed_template.js_app_root())?
    {
        output::success(&format!("Created tako.toml and {generated_file}"));
    } else {
        output::success("Created tako.toml");
    }

    if let Some(cmd) = sdk_install_command(adapter, project_dir) {
        output::info(&format!("Install the SDK: {}", output::strong(&cmd)));
    }

    Ok(())
}

pub(super) fn resolve_adapter(
    detected_adapter: BuildAdapter,
    existing: Option<&TakoToml>,
) -> BuildAdapter {
    let preferred = existing
        .and_then(|c| c.runtime.as_deref())
        .and_then(BuildAdapter::from_id)
        .unwrap_or(detected_adapter);
    match preferred {
        BuildAdapter::Unknown => BuildAdapter::Bun,
        other => other,
    }
}
