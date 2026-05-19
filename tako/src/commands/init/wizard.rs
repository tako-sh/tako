use std::path::Path;

use crate::app::resolve_app_name;
use crate::build::{BuildAdapter, PresetDefinition, PresetGroup};
use crate::config::TakoToml;
use crate::output;

use super::presets::{build_preset_selection_options, fetch_group_presets_for_adapter};
use super::scaffold::{
    detect_js_app_root, infer_default_main_entrypoint, parse_csv_list, preset_default_main,
};

pub(super) struct InteractiveInitSelection {
    pub(super) adapter: BuildAdapter,
    pub(super) selected_preset: Option<String>,
    pub(super) main_entry: Option<String>,
    pub(super) app_root: String,
    pub(super) assets: Vec<String>,
    pub(super) excludes: Vec<String>,
    pub(super) app_name: String,
    pub(super) production_route: String,
}

pub(super) fn prompt_interactive_config(
    project_dir: &Path,
    existing: &Option<TakoToml>,
    detected_adapter: BuildAdapter,
) -> Result<Option<InteractiveInitSelection>, Box<dyn std::error::Error>> {
    let mut wizard = output::Wizard::new()
        .with_fields(&[
            ("Application name", false),
            ("Runtime", false),
            ("App root", true),
            ("Build preset", false),
            ("Entrypoint", true),
            ("Assets", true),
            ("Exclude", true),
            ("Production route", false),
        ])
        .with_confirmation();
    let mut step = 0usize;
    let mut step_history: Vec<usize> = Vec::new();
    let mut group_presets_cache: Option<(BuildAdapter, Vec<PresetDefinition>)> = None;

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
        .unwrap_or_else(|| detect_js_app_root(project_dir));
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

    let mut is_custom = selected_preset.is_none();

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
            0 => {
                let default_app_name = if !app_name.is_empty() {
                    app_name.clone()
                } else {
                    existing
                        .as_ref()
                        .and_then(|c| c.name.clone())
                        .unwrap_or_else(|| {
                            resolve_app_name(project_dir).unwrap_or_else(|_| "my-app".to_string())
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
                    Err(e) if output::is_wizard_back(&e) => return Ok(None),
                    Err(e) => return Err(e.into()),
                }
            }
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
                            return Ok(None);
                        }
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            2 => {
                let default_app_root = if app_root.trim().is_empty() {
                    detect_js_app_root(project_dir)
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
                            return Ok(None);
                        }
                    }
                    Err(e) => return Err(e.into()),
                }
            }
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
                                return Ok(None);
                            }
                            continue;
                        }
                        Err(e) => return Err(e.into()),
                    }
                } else {
                    selected_preset = Some(adapter.default_preset().to_string());
                }

                is_custom = selected_preset.is_none();
                let preset_dm = selected_preset
                    .as_deref()
                    .and_then(|preset| preset_default_main(preset, adapter, &group_presets));
                let inferred_main = adapter.infer_main_entrypoint(project_dir);

                push_history_if_interactive(&mut step_history, 3, prompted_for_preset);

                if is_custom {
                    wizard.set_visible("Entrypoint", true);
                    wizard.set_visible("Assets", true);
                    wizard.set_visible("Exclude", true);
                    step = 4;
                } else if let Some(ref inferred) = inferred_main {
                    main_entry = if preset_dm.as_deref() == Some(inferred.as_str()) {
                        None
                    } else {
                        Some(inferred.clone())
                    };
                    wizard.set_visible("Entrypoint", false);
                    wizard.set_visible("Assets", false);
                    wizard.set_visible("Exclude", false);
                    step = 7;
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
                    step = 4;
                }
            }
            4 => {
                let default_main = main_entry
                    .clone()
                    .or_else(|| existing.as_ref().and_then(|c| c.main.clone()))
                    .or_else(|| adapter.infer_main_entrypoint(project_dir))
                    .unwrap_or_else(|| infer_default_main_entrypoint(project_dir, adapter));
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
                            return Ok(None);
                        }
                    }
                    Err(e) => return Err(e.into()),
                }
            }
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
                            return Ok(None);
                        }
                    }
                    Err(e) => return Err(e.into()),
                }
            }
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
                            return Ok(None);
                        }
                    }
                    Err(e) => return Err(e.into()),
                }
            }
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
                            return Ok(None);
                        }
                    }
                    Err(e) => return Err(e.into()),
                }
            }
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

    Ok(Some(InteractiveInitSelection {
        adapter,
        selected_preset,
        main_entry,
        app_root,
        assets,
        excludes,
        app_name,
        production_route,
    }))
}

pub(super) fn push_history_if_interactive(
    step_history: &mut Vec<usize>,
    step: usize,
    interactive: bool,
) {
    if interactive {
        step_history.push(step);
    }
}
