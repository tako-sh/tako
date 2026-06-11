use std::path::Path;

use crate::plugin::{PluginContext, RuntimePlugin};
use crate::types::{
    EntrypointDef, EnvsDef, PackageManagerDef, PackageManagerDevDef, PresetDef, RuntimeDef,
    ServerDef,
};

pub struct RustPlugin;

impl RustPlugin {
    fn build_def(&self, project_dir: Option<&Path>) -> RuntimeDef {
        let package_name = project_dir.and_then(read_package_name);
        let build = match package_name {
            Some(name) => format!(
                "cargo build --release --locked --bin {name} && cp target/release/{name} app"
            ),
            None => "cargo build --release --locked && cp target/release/$(basename \"$PWD\") app"
                .to_string(),
        };

        RuntimeDef {
            id: "rust".to_string(),
            language: "rust".to_string(),
            entrypoint: EntrypointDef {
                candidates: vec!["Cargo.toml".to_string(), "src/main.rs".to_string()],
                manifest: None,
            },
            preset: PresetDef {
                main: Some("app".to_string()),
                dev: vec!["cargo".to_string(), "run".to_string()],
                watch: vec![
                    "Cargo.toml".to_string(),
                    "Cargo.lock".to_string(),
                    "src/**/*.rs".to_string(),
                ],
                start: vec!["{main}".to_string()],
                build: Some(build),
            },
            server: ServerDef {
                entrypoint_path: None,
                launch_args: vec!["{main}".to_string()],
            },
            envs: EnvsDef::default(),
            package_manager: PackageManagerDef {
                id: "cargo".to_string(),
                name: Some("Cargo".to_string()),
                lockfiles: vec!["Cargo.lock".to_string()],
                add: Some("cargo add tako".to_string()),
                install: None,
                development: Some(PackageManagerDevDef {
                    install: Some("cargo fetch".to_string()),
                }),
            },
            download: None,
        }
    }
}

impl RuntimePlugin for RustPlugin {
    fn id(&self) -> &'static str {
        "rust"
    }

    fn language(&self) -> &'static str {
        "rust"
    }

    fn runtime_def(&self, ctx: &PluginContext) -> RuntimeDef {
        self.build_def(Some(ctx.project_dir))
    }

    fn default_runtime_def(&self) -> RuntimeDef {
        self.build_def(None)
    }
}

fn read_package_name(project_dir: &Path) -> Option<String> {
    let manifest = std::fs::read_to_string(project_dir.join("Cargo.toml")).ok()?;
    let mut in_package = false;

    for raw_line in manifest.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_package = line == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "name" {
            continue;
        }
        let value = value.trim().trim_matches('"').trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_plugin_uses_stable_app_binary() {
        let def = RustPlugin.default_runtime_def();
        assert_eq!(def.id, "rust");
        assert_eq!(def.language, "rust");
        assert_eq!(def.preset.main.as_deref(), Some("app"));
        assert_eq!(def.server.launch_args, vec!["{main}"]);
    }

    #[test]
    fn rust_plugin_reads_package_name_for_build_command() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("Cargo.toml"),
            r#"
            [package]
            name = "tako-cloud"
            version = "0.1.0"
            "#,
        )
        .unwrap();

        let ctx = PluginContext {
            project_dir: temp.path(),
            package_manager: None,
        };
        let def = RustPlugin.runtime_def(&ctx);
        assert_eq!(
            def.preset.build.as_deref(),
            Some(
                "cargo build --release --locked --bin tako-cloud && cp target/release/tako-cloud app"
            )
        );
    }
}
