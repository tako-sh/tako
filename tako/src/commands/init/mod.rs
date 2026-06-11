mod generated;
mod non_interactive;
mod presets;
mod project;
mod scaffold;
mod ssl;
mod wizard;

use std::fs;
use std::path::Path;

use crate::build::{PresetGroup, detect_build_adapter};
use crate::config::TakoToml;
use crate::output;
use generated::{install_tako_sdk, write_init_generated_file};
use non_interactive::run_non_interactive;
use project::{display_config_path_for_prompt, ensure_project_gitignore_tracks_secrets};
use scaffold::{TemplateParams, detect_local_runtime_version, generate_template, sanitize_route};
use ssl::prompt_init_ssl_token;
use wizard::{InteractiveInitSelection, prompt_interactive_config};

pub fn run(config_path: Option<&Path>) -> Result<(), Box<dyn std::error::Error>> {
    output::logo_header();
    let cwd = std::env::current_dir()?;
    let context = crate::commands::project_context::resolve(config_path)?;
    let project_dir = context.project_dir;
    let mut tako_toml_path = context.config_path;

    let existing = if tako_toml_path.exists() {
        TakoToml::load_from_file(&tako_toml_path).ok()
    } else {
        None
    };

    if existing.is_some() {
        if !output::is_interactive() {
            output::operation_cancelled();
            return Ok(());
        }
        if !output::confirm(
            &format!(
                "Configuration file {} already exists. Overwrite?",
                output::strong(&display_config_path_for_prompt(&tako_toml_path, &cwd))
            ),
            false,
        )? {
            let name = output::TextField::new("New config name").prompt()?;
            let name = if name.ends_with(".toml") {
                name
            } else {
                format!("{name}.toml")
            };
            tako_toml_path = project_dir.join(&name);
        }
    }

    let detected_adapter = detect_build_adapter(&project_dir);
    if !output::is_interactive() {
        return run_non_interactive(
            &project_dir,
            &tako_toml_path,
            detected_adapter,
            existing.as_ref(),
        );
    }

    let Some(selection) = prompt_interactive_config(&project_dir, &existing, detected_adapter)?
    else {
        return Ok(());
    };

    let InteractiveInitSelection {
        adapter,
        selected_preset,
        main_entry,
        app_root,
        assets,
        excludes,
        app_name,
        production_route,
    } = selection;

    let selected_preset_for_toml = selected_preset
        .as_deref()
        .filter(|preset| *preset != adapter.default_preset())
        .map(str::to_string);

    let runtime_version = detect_local_runtime_version(adapter.version_probe_tool());

    let detected_pm = tako_runtime::detect_package_manager(&project_dir);
    let pm_for_toml = detected_pm.map(|pm| pm.id().to_string()).filter(|pm_id| {
        let default_pm = tako_runtime::plugin_for_id(adapter.id())
            .map(|p| p.default_runtime_def().package_manager.id)
            .unwrap_or_default();
        *pm_id != default_pm
    });

    let production_route = sanitize_route(&production_route);
    let init_ssl_token = prompt_init_ssl_token(&production_route)?;
    let app_root_for_toml = if adapter.preset_group() == PresetGroup::Js {
        Some(app_root.trim())
    } else {
        None
    };
    let template = generate_template(&TemplateParams {
        app_name: app_name.trim(),
        app_root: app_root_for_toml,
        main: main_entry.as_deref().map(str::trim),
        production_route: &production_route,
        runtime: Some(adapter.id()),
        runtime_version: runtime_version.as_deref(),
        package_manager: pm_for_toml.as_deref(),
        preset_ref: selected_preset_for_toml.as_deref(),
        assets: &assets,
        excludes: &excludes,
    });

    let parsed_template = TakoToml::parse(&template)?;
    fs::write(&tako_toml_path, template)?;
    ensure_project_gitignore_tracks_secrets(&project_dir)?;
    let configured_init_ssl = init_ssl_token.is_some();
    if let Some((token, expires_on)) = init_ssl_token {
        crate::commands::credentials::set_ssl_cloudflare_credential(
            &project_dir,
            "production",
            &token,
            expires_on,
        )?;
    }

    let config_name = tako_toml_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    if let Some(generated_file) =
        write_init_generated_file(&project_dir, adapter, parsed_template.js_app_root())?
    {
        if configured_init_ssl {
            output::success(&format!(
                "Created {config_name}, {generated_file}, and SSL credentials"
            ));
        } else {
            output::success(&format!("Created {config_name} and {generated_file}"));
        }
    } else if configured_init_ssl {
        output::success(&format!("Created {config_name} and SSL credentials"));
    } else {
        output::success(&format!("Created {config_name}"));
    }

    install_tako_sdk(&project_dir, adapter);

    output::heading("Next steps");
    output::info(&format!(
        "1. Edit {} to set environment variables and more",
        output::strong(&config_name)
    ));
    output::info(&format!(
        "2. Run {} to add deployment servers",
        output::strong("tako servers add")
    ));
    output::info(&format!(
        "3. Run {} to add secrets",
        output::strong("tako secrets set")
    ));
    output::info(&format!(
        "4. Run {} to deploy your app",
        output::strong("tako deploy")
    ));

    Ok(())
}

#[cfg(test)]
mod tests;
