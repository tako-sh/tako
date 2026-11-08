use std::path::Path;

use crate::build::{self, PresetGroup, detect_build_adapter};
use crate::commands::project_context;
use crate::config::TakoToml;
use crate::output;

pub fn run(config_path: Option<&Path>) -> Result<(), Box<dyn std::error::Error>> {
    let ctx = project_context::resolve(config_path)?;
    let tako_config = TakoToml::load_from_file(&ctx.config_path)?;
    for warning in tako_config.ignored_reserved_var_warnings() {
        output::warning(&format!("Validation: {}", warning));
    }

    let adapter = if let Some(runtime) = tako_config
        .runtime
        .as_deref()
        .map(str::trim)
        .filter(|v: &&str| !v.is_empty())
    {
        build::BuildAdapter::from_id(runtime).unwrap_or(detect_build_adapter(&ctx.project_dir))
    } else {
        detect_build_adapter(&ctx.project_dir)
    };

    match adapter.preset_group() {
        PresetGroup::Js => {
            let result = build::js::write_generated_files_for_adapter_and_app_root(
                &ctx.project_dir,
                adapter,
                tako_config.js_app_root(),
            )?;
            let mut generated = Vec::new();
            if result.wrote_declarations {
                generated.push("tako.d.ts");
            }
            if result.wrote_contracts {
                generated.push("tako.contracts.d.ts");
            }
            if result.wrote_scaffolds {
                generated.push("channel/workflow stubs");
            }
            if generated.is_empty() {
                output::success("Generated files are up to date");
            } else {
                output::success(&format!("Generated {}", generated.join(" and ")));
            }
        }
        PresetGroup::Go => {
            let written = build::go::write_secret_accessors(&ctx.project_dir)?;
            if written {
                output::success("Generated tako_secrets.go");
            } else {
                output::success("tako_secrets.go is up to date");
            }
        }
        PresetGroup::Unknown => {
            return Err("Could not detect project language. Set `runtime` in tako.toml.".into());
        }
    }

    Ok(())
}
