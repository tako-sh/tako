use std::path::Path;

use crate::build::{self, PresetGroup, detect_build_adapter};
use crate::commands::project_context;
use crate::config::TakoToml;
use crate::output;

pub fn run(
    config_path: Option<&Path>,
    update_schemas: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let ctx = project_context::resolve(config_path)?;
    let tako_config = TakoToml::load_from_file(&ctx.config_path)?;
    for warning in tako_config.ignored_reserved_var_warnings() {
        output::warning(&format!("Validation: {}", warning));
    }
    let native_root = build::schemas::native_root(&ctx.project_dir, &tako_config);
    if tako_config.runtime.is_none()
        && native_root.join("Cargo.toml").is_file()
        && (tako_config.container.is_some()
            || tako_config.build.cwd.is_some()
            || !native_root.join("package.json").is_file())
    {
        let changed =
            build::schemas::generate_native(&ctx.project_dir, &tako_config, true, update_schemas)?;
        output::success(if changed {
            "Generated Rust network types and tako-schema.json"
        } else {
            "Generated files are up to date"
        });
        return Ok(());
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
            let result = build::js::write_generated_files_with_schemas(
                &ctx.project_dir,
                adapter,
                &tako_config,
                None,
                update_schemas,
            )?;
            let mut generated = Vec::new();
            if result.wrote_declarations {
                generated.push("tako.d.ts");
            }
            if result.wrote_contracts {
                generated.push("network types and tako-schema.json");
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
            let mut written = build::schemas::generate_native(
                &ctx.project_dir,
                &tako_config,
                false,
                update_schemas,
            )?;
            written |= build::go::write_secret_accessors_with_config(
                &ctx.project_dir,
                &tako_config,
                None,
            )?;
            if written {
                output::success("Generated Go types, secret accessors, and tako-schema.json");
            } else {
                output::success("Generated files are up to date");
            }
        }
        PresetGroup::Unknown => {
            if build::schemas::native_root(&ctx.project_dir, &tako_config)
                .join("Cargo.toml")
                .is_file()
            {
                build::schemas::generate_native(
                    &ctx.project_dir,
                    &tako_config,
                    true,
                    update_schemas,
                )?;
                output::success("Generated Rust network types and tako-schema.json");
            } else {
                return Err(
                    "Could not detect project language. Set `runtime` in tako.toml.".into(),
                );
            }
        }
    }

    Ok(())
}
