mod presets;
mod project;
mod scaffold;

use std::fs;
use std::path::Path;

use crate::app::resolve_app_name;
use crate::build::{BuildAdapter, PresetDefinition, PresetGroup, detect_build_adapter, go, js};
use crate::config::TakoToml;
use crate::output;
use presets::{build_preset_selection_options, fetch_group_presets_for_adapter};
use project::{display_config_path_for_prompt, ensure_project_gitignore_tracks_secrets};

use scaffold::{
    TemplateParams, detect_js_app_root, detect_local_runtime_version, generate_template,
    infer_default_main_entrypoint, parse_csv_list, preset_default_main, sanitize_route,
    sdk_install_command,
};

pub fn run(config_path: Option<&Path>) -> Result<(), Box<dyn std::error::Error>> {
    output::logo_header();
    let cwd = std::env::current_dir()?;
    let context = crate::commands::project_context::resolve(config_path)?;
    let project_dir = context.project_dir;
    let mut tako_toml_path = context.config_path;

    // Load existing config for pre-filling defaults
    let existing = if tako_toml_path.exists() {
        TakoToml::load_from_file(&tako_toml_path).ok()
    } else {
        None
    };

    // Check if config already exists — prompt to overwrite or use a different name
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

    // Non-interactive: skip wizard, use defaults
    if !output::is_interactive() {
        return run_non_interactive(
            &project_dir,
            &tako_toml_path,
            detected_adapter,
            existing.as_ref(),
        );
    }

    // Interactive wizard with state machine for ESC go-back
    let mut wizard = output::Wizard::new()
        .with_fields(&[
            ("Application name", false),
            ("Runtime", false),
            ("App root", true),
            ("Build preset", false),
            ("Entrypoint", true), // subsection — hidden until custom preset
            ("Assets", true),     // subsection
            ("Exclude", true),    // subsection
            ("Production route", false),
        ])
        .with_confirmation();
    let mut step = 0usize;
    let mut step_history: Vec<usize> = Vec::new();

    // Cached group presets (keyed by adapter to avoid re-fetching)
    let mut group_presets_cache: Option<(BuildAdapter, Vec<PresetDefinition>)> = None;

    // Accumulated values — pre-filled from existing config when overwriting
    let mut adapter = existing
        .as_ref()
        .and_then(|c| c.runtime.as_deref())
        .and_then(BuildAdapter::from_id)
        .unwrap_or(detected_adapter);
    let mut selected_preset: Option<String> = existing.as_ref().and_then(|c| c.preset.clone());
    let mut main_entry: Option<String> = existing.as_ref().and_then(|c| c.main.clone());
    let mut app_root = existing
        .as_ref()
        .and_then(|c| c.app_root.as_ref().map(|root| root.trim().to_string()))
        .unwrap_or_else(|| detect_js_app_root(&project_dir));
    let mut assets: Vec<String> = existing
        .as_ref()
        .map(|c| c.assets.clone())
        .unwrap_or_default();
    let mut excludes: Vec<String> = existing
        .as_ref()
        .map(|c| c.build.exclude.clone())
        .unwrap_or_default();
    let mut app_name = existing
        .as_ref()
        .and_then(|c| c.name.clone())
        .unwrap_or_default();
    let mut production_route = existing
        .as_ref()
        .and_then(|c| c.envs.get("production").and_then(|e| e.route.clone()))
        .unwrap_or_default();

    // Derived state
    let mut is_custom = selected_preset.is_none();

    // Pre-populate wizard from existing config
    if existing.is_some() {
        if !app_name.is_empty() {
            wizard.set("Application name", &app_name);
        }
        wizard.set("Runtime", adapter.id());
        if adapter.preset_group() == PresetGroup::Js {
            wizard.set_visible("App root", true);
            wizard.set("App root", &app_root);
        }
        if let Some(ref preset) = selected_preset {
            wizard.set("Build preset", preset);
        } else {
            wizard.set("Build preset", "custom");
        }
        if is_custom {
            wizard.set_visible("Entrypoint", true);
            wizard.set_visible("Assets", true);
            wizard.set_visible("Exclude", true);
            if let Some(ref main) = main_entry {
                wizard.set("Entrypoint", main);
            }
            if !assets.is_empty() {
                wizard.set("Assets", &assets.join(", "));
            }
            if !excludes.is_empty() {
                wizard.set("Exclude", &excludes.join(", "));
            }
        }
        if !production_route.is_empty() {
            wizard.set("Production route", &production_route);
        }
    }

    loop {
        match step {
            // Step 0: App name
            0 => {
                let default_app_name = if !app_name.is_empty() {
                    app_name.clone()
                } else {
                    existing
                        .as_ref()
                        .and_then(|c| c.name.clone())
                        .unwrap_or_else(|| {
                            resolve_app_name(&project_dir).unwrap_or_else(|_| "my-app".to_string())
                        })
                };
                match wizard.input(
                    "Application name",
                    Some(&default_app_name),
                    Some("Name cannot be changed after the first deployment."),
                ) {
                    Ok(v) => {
                        app_name = v;
                        step_history.push(0);
                        step = 1;
                    }
                    Err(e) if output::is_wizard_back(&e) => return Ok(()),
                    Err(e) => return Err(e.into()),
                }
            }
            // Step 1: Runtime (pre-filled with detected value)
            1 => {
                let adapters = [BuildAdapter::Bun, BuildAdapter::Node];
                let default_index = adapters.iter().position(|a| *a == adapter).unwrap_or(0);
                let options: Vec<(String, BuildAdapter)> =
                    adapters.iter().map(|&a| (a.id().to_string(), a)).collect();
                let hints: Vec<&str> = adapters
                    .iter()
                    .map(|&a| {
                        if a == detected_adapter && detected_adapter != BuildAdapter::Unknown {
                            "detected"
                        } else {
                            ""
                        }
                    })
                    .collect();
                match wizard.select(
                    "Runtime",
                    "Choose a runtime:",
                    options,
                    &hints,
                    default_index,
                ) {
                    Ok(a) => {
                        adapter = a;
                        let js_runtime = adapter.preset_group() == PresetGroup::Js;
                        wizard.set_visible("App root", js_runtime);
                        step_history.push(1);
                        step = if js_runtime { 2 } else { 3 };
                    }
                    Err(e) if output::is_wizard_back(&e) => {
                        if let Some(prev) = step_history.pop() {
                            step = prev;
                        } else {
                            return Ok(());
                        }
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            // Step 2: JavaScript app root
            2 => {
                let default_app_root = if app_root.trim().is_empty() {
                    detect_js_app_root(&project_dir)
                } else {
                    app_root.clone()
                };
                match wizard.input(
                    "App root",
                    Some(&default_app_root),
                    Some("JS root for channels/ and workflows/ (use . for project root)."),
                ) {
                    Ok(v) => {
                        let trimmed = v.trim();
                        app_root = if trimmed.is_empty() {
                            default_app_root
                        } else {
                            trimmed.to_string()
                        };
                        step_history.push(2);
                        step = 3;
                    }
                    Err(e) if output::is_wizard_back(&e) => {
                        if let Some(prev) = step_history.pop() {
                            step = prev;
                        } else {
                            return Ok(());
                        }
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            // Step 3: Build preset + compute derived state
            3 => {
                let mut prompted_for_preset = false;
                let group_presets = match &group_presets_cache {
                    Some((cached, presets)) if *cached == adapter => presets.clone(),
                    _ => {
                        let presets = fetch_group_presets_for_adapter(adapter)?;
                        group_presets_cache = Some((adapter, presets.clone()));
                        presets
                    }
                };
                let group_preset_names: Vec<String> =
                    group_presets.iter().map(|p| p.name.clone()).collect();
                let existing_preset_ref = existing.as_ref().and_then(|c| c.preset.as_deref());

                if let Some(options) = build_preset_selection_options(adapter, &group_preset_names)
                {
                    let default_index = selected_preset
                        .as_deref()
                        .and_then(|sp| options.iter().position(|(_, v)| v.as_deref() == Some(sp)))
                        .or_else(|| {
                            existing_preset_ref.and_then(|ep| {
                                options.iter().position(|(_, v)| v.as_deref() == Some(ep))
                            })
                        })
                        .unwrap_or(0);
                    match wizard.select(
                        "Build preset",
                        "Choose a build preset:",
                        options,
                        &[],
                        default_index,
                    ) {
                        Ok(sp) => {
                            selected_preset = sp;
                            prompted_for_preset = true;
                        }
                        Err(e) if output::is_wizard_back(&e) => {
                            if let Some(prev) = step_history.pop() {
                                step = prev;
                            } else {
                                return Ok(());
                            }
                            continue;
                        }
                        Err(e) => return Err(e.into()),
                    }
                } else {
                    selected_preset = Some(adapter.default_preset().to_string());
                }

                // Compute derived state
                is_custom = selected_preset.is_none();
                let preset_dm = selected_preset
                    .as_deref()
                    .and_then(|preset| preset_default_main(preset, adapter, &group_presets));
                let inferred_main = adapter.infer_main_entrypoint(&project_dir);

                push_history_if_interactive(&mut step_history, 3, prompted_for_preset);

                if is_custom {
                    wizard.set_visible("Entrypoint", true);
                    wizard.set_visible("Assets", true);
                    wizard.set_visible("Exclude", true);
                    step = 4; // entrypoint prompt
                } else if let Some(ref inferred) = inferred_main {
                    main_entry = if preset_dm.as_deref() == Some(inferred.as_str()) {
                        None
                    } else {
                        Some(inferred.clone())
                    };
                    wizard.set_visible("Entrypoint", false);
                    wizard.set_visible("Assets", false);
                    wizard.set_visible("Exclude", false);
                    step = 7; // skip to production route
                } else if preset_dm.is_some() {
                    main_entry = None;
                    wizard.set_visible("Entrypoint", false);
                    wizard.set_visible("Assets", false);
                    wizard.set_visible("Exclude", false);
                    step = 7;
                } else {
                    wizard.set_visible("Entrypoint", true);
                    wizard.set_visible("Assets", false);
                    wizard.set_visible("Exclude", false);
                    step = 4; // need entrypoint prompt
                }
            }
            // Step 4: Entrypoint
            4 => {
                let default_main = main_entry
                    .clone()
                    .or_else(|| existing.as_ref().and_then(|c| c.main.clone()))
                    .or_else(|| adapter.infer_main_entrypoint(&project_dir))
                    .unwrap_or_else(|| infer_default_main_entrypoint(&project_dir, adapter));
                match wizard.input("Entrypoint", Some(&default_main), None) {
                    Ok(v) => {
                        main_entry = Some(v);
                        step_history.push(4);
                        if is_custom {
                            step = 5;
                        } else {
                            step = 7;
                        }
                    }
                    Err(e) if output::is_wizard_back(&e) => {
                        if let Some(prev) = step_history.pop() {
                            step = prev;
                        } else {
                            return Ok(());
                        }
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            // Step 5: Assets (custom only)
            5 => {
                let existing_assets = existing
                    .as_ref()
                    .map(|c| c.assets.clone())
                    .unwrap_or_default();
                let prev = if !assets.is_empty() {
                    &assets
                } else {
                    &existing_assets
                };
                let default = if prev.is_empty() {
                    None
                } else {
                    Some(prev.join(", "))
                };
                let mut builder = output::TextField::new("Assets")
                    .optional()
                    .with_hint("Comma-separated asset directories. Leave empty for none.");
                if let Some(ref d) = default {
                    builder = builder.with_default(d);
                }
                match wizard.text_field(builder) {
                    Ok(value) => {
                        let collected = parse_csv_list(&value);
                        if !collected.is_empty() {
                            wizard.set("Assets", &collected.join(", "));
                        }
                        assets = collected;
                        step_history.push(5);
                        step = 6;
                    }
                    Err(e) if output::is_wizard_back(&e) => {
                        if let Some(prev) = step_history.pop() {
                            step = prev;
                        } else {
                            return Ok(());
                        }
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            // Step 6: Excludes (custom only)
            6 => {
                let existing_excludes = existing
                    .as_ref()
                    .map(|c| c.build.exclude.clone())
                    .unwrap_or_default();
                let prev = if !excludes.is_empty() {
                    &excludes
                } else {
                    &existing_excludes
                };
                let default = if prev.is_empty() {
                    None
                } else {
                    Some(prev.join(", "))
                };
                let mut builder = output::TextField::new("Exclude")
                    .optional()
                    .with_hint("Comma-separated exclude patterns. Leave empty for none.");
                if let Some(ref d) = default {
                    builder = builder.with_default(d);
                }
                match wizard.text_field(builder) {
                    Ok(value) => {
                        let collected = parse_csv_list(&value);
                        if !collected.is_empty() {
                            wizard.set("Exclude", &collected.join(", "));
                        }
                        excludes = collected;
                        step_history.push(6);
                        step = 7;
                    }
                    Err(e) if output::is_wizard_back(&e) => {
                        if let Some(prev) = step_history.pop() {
                            step = prev;
                        } else {
                            return Ok(());
                        }
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            // Step 7: Production route
            7 => {
                let default_route = if !production_route.is_empty() {
                    production_route.clone()
                } else {
                    existing
                        .as_ref()
                        .and_then(|c| c.envs.get("production").and_then(|e| e.route.clone()))
                        .unwrap_or_else(|| format!("{}.example.com", app_name.trim()))
                };
                match wizard.input("Production route", Some(&default_route), None) {
                    Ok(v) => {
                        production_route = v;
                        step_history.push(7);
                        step = 8;
                    }
                    Err(e) if output::is_wizard_back(&e) => {
                        if let Some(prev) = step_history.pop() {
                            step = prev;
                        } else {
                            return Ok(());
                        }
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            // Step 8: Confirm
            _ => match wizard.finish() {
                Ok(true) => break,
                Ok(false) => {
                    step_history.clear();
                    step = 0;
                }
                Err(e) if output::is_wizard_back(&e) => {
                    if let Some(prev) = step_history.pop() {
                        step = prev;
                    }
                }
                Err(e) => return Err(e.into()),
            },
        }
    }

    let selected_preset_for_toml = selected_preset
        .as_deref()
        .filter(|preset| *preset != adapter.default_preset())
        .map(str::to_string);

    // Detect local runtime version for pinning.
    let runtime_version = detect_local_runtime_version(adapter.id());

    // Detect package manager (only write if it differs from runtime default).
    let detected_pm = tako_runtime::detect_package_manager(&project_dir);
    let pm_for_toml = detected_pm.map(|pm| pm.id().to_string()).filter(|pm_id| {
        let default_pm = tako_runtime::plugin_for_id(adapter.id())
            .map(|p| p.default_runtime_def().package_manager.id)
            .unwrap_or_default();
        *pm_id != default_pm
    });

    // Generate tako.toml
    let production_route = sanitize_route(&production_route);
    let init_dns_token = prompt_init_dns_token(&production_route)?;
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
    let configured_init_dns = init_dns_token.is_some();
    if let Some((token, expires_on)) = init_dns_token {
        crate::commands::dns::configure_env_dns(
            &project_dir,
            "production",
            Some(token),
            expires_on,
            false,
        )?;
    }

    let config_name = tako_toml_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    if let Some(generated_file) =
        write_init_generated_file(&project_dir, adapter, parsed_template.js_app_root())?
    {
        if configured_init_dns {
            output::success(&format!(
                "Created {config_name}, {generated_file}, and DNS secrets"
            ));
        } else {
            output::success(&format!("Created {config_name} and {generated_file}"));
        }
    } else if configured_init_dns {
        output::success(&format!("Created {config_name} and DNS secrets"));
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

/// Install the tako.sh SDK package using the runtime package manager.
fn install_tako_sdk(project_dir: &Path, runtime: BuildAdapter) {
    let Some(cmd) = sdk_install_command(runtime, project_dir) else {
        return;
    };
    // Ensure pnpm is available for Node runtime.
    if runtime == BuildAdapter::Node {
        ensure_pnpm(project_dir);
    }
    output::info(&format!("Installing tako.sh SDK: {}", output::strong(&cmd)));
    let result = std::process::Command::new("sh")
        .args(["-c", &cmd])
        .current_dir(project_dir)
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status();
    match result {
        Ok(status) if status.success() => {}
        _ => {
            output::info(&format!(
                "Could not install tako.sh automatically. Run {} manually.",
                output::strong(&cmd)
            ));
        }
    }
}

/// Ensure pnpm is available, installing it via npm if missing.
fn ensure_pnpm(project_dir: &Path) {
    let has_pnpm = std::process::Command::new("pnpm")
        .arg("--version")
        .current_dir(project_dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if has_pnpm {
        return;
    }
    output::info("Installing pnpm…");
    let _ = std::process::Command::new("npm")
        .args(["install", "-g", "pnpm"])
        .current_dir(project_dir)
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status();
}

fn write_init_generated_file(
    project_dir: &Path,
    adapter: BuildAdapter,
    app_root: &str,
) -> Result<Option<&'static str>, Box<dyn std::error::Error>> {
    match adapter.preset_group() {
        PresetGroup::Js => {
            if js::write_tako_declarations_for_adapter_and_app_root(project_dir, adapter, app_root)?
            {
                Ok(Some("tako.d.ts"))
            } else {
                Ok(None)
            }
        }
        PresetGroup::Go => {
            if go::write_secret_accessors(project_dir)? {
                Ok(Some("tako_secrets.go"))
            } else {
                Ok(None)
            }
        }
        PresetGroup::Unknown => Ok(None),
    }
}

fn prompt_init_dns_token(
    production_route: &str,
) -> Result<Option<(String, Option<String>)>, Box<dyn std::error::Error>> {
    if !output::is_interactive() || !production_route_needs_dns(production_route) {
        return Ok(None);
    }

    let description = "Wildcard routes need DNS-01 certificates. Tako stores the token encrypted in .tako/secrets.json.";
    let should_configure = output::confirm_with_description(
        "Set up Cloudflare DNS for wildcard HTTPS?",
        Some(description),
        true,
    )?;
    if !should_configure {
        return Ok(None);
    }

    let token = crate::commands::dns::read_dns_credential(None, "Cloudflare API token")?;
    let expires_on = output::TextField::new("Expires on")
        .with_hint(crate::config::secret_expires_on_prompt_hint())
        .prompt_validated(|value| {
            crate::config::normalize_secret_expires_on(value)
                .map(|_| ())
                .map_err(|e| e.to_string())
        })?;
    Ok(Some((
        token,
        crate::config::normalize_secret_expires_on(&expires_on)?,
    )))
}

fn production_route_needs_dns(route: &str) -> bool {
    route
        .trim()
        .split('/')
        .next()
        .unwrap_or_default()
        .trim()
        .starts_with("*.")
}

fn resolve_adapter(detected_adapter: BuildAdapter, existing: Option<&TakoToml>) -> BuildAdapter {
    let preferred = existing
        .and_then(|c| c.runtime.as_deref())
        .and_then(BuildAdapter::from_id)
        .unwrap_or(detected_adapter);
    match preferred {
        BuildAdapter::Unknown => BuildAdapter::Bun,
        other => other,
    }
}

fn run_non_interactive(
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

    let runtime_version = detect_local_runtime_version(adapter.id());
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

fn push_history_if_interactive(step_history: &mut Vec<usize>, step: usize, interactive: bool) {
    if interactive {
        step_history.push(step);
    }
}

#[cfg(test)]
mod tests;
