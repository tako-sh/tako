use std::path::Path;

use crate::plugin::{PluginContext, RuntimePlugin};
use crate::types::{
    EntrypointDef, EnvsDef, PackageManagerDef, PackageManagerDevDef, PresetDef, RuntimeDef,
    ServerDef,
};

// ── Go Plugin ──────────────────────────────────────────────────────

pub struct GoPlugin;

impl GoPlugin {
    fn build_def(&self, project_dir: Option<&Path>) -> RuntimeDef {
        let build = if project_dir.is_some_and(has_conventional_worker_binary) {
            "CGO_ENABLED=0 go build -o app . && CGO_ENABLED=0 go build -o worker ./cmd/worker"
        } else {
            "CGO_ENABLED=0 go build -o app ."
        };

        RuntimeDef {
            id: "go".to_string(),
            language: "go".to_string(),
            entrypoint: EntrypointDef {
                candidates: vec!["main.go".to_string()],
                manifest: None,
            },
            preset: PresetDef {
                main: Some("app".to_string()),
                dev: vec!["go".to_string(), "run".to_string(), ".".to_string()],
                watch: vec![
                    "**/*.go".to_string(),
                    "go.mod".to_string(),
                    "go.sum".to_string(),
                ],
                start: vec!["{main}".to_string()],
                build: Some(build.to_string()),
            },
            server: ServerDef {
                entrypoint_path: None,
                launch_args: vec!["{main}".to_string()],
            },
            envs: EnvsDef::default(),
            package_manager: PackageManagerDef {
                id: "go".to_string(),
                name: Some("Go Modules".to_string()),
                lockfiles: vec!["go.sum".to_string()],
                add: Some("go get {package}".to_string()),
                install: None,
                development: Some(PackageManagerDevDef {
                    install: Some("go mod download".to_string()),
                }),
            },
            download: None,
        }
    }
}

impl RuntimePlugin for GoPlugin {
    fn id(&self) -> &'static str {
        "go"
    }

    fn language(&self) -> &'static str {
        "go"
    }

    fn runtime_def(&self, ctx: &PluginContext) -> RuntimeDef {
        self.build_def(Some(ctx.project_dir))
    }

    fn default_runtime_def(&self) -> RuntimeDef {
        self.build_def(None)
    }
}

fn has_conventional_worker_binary(project_dir: &Path) -> bool {
    project_dir.join("cmd/worker/main.go").is_file()
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_plugin_id_and_language() {
        assert_eq!(GoPlugin.id(), "go");
        assert_eq!(GoPlugin.language(), "go");
    }

    #[test]
    fn go_plugin_default_runtime_def() {
        let def = GoPlugin.default_runtime_def();
        assert_eq!(def.id, "go");
        assert_eq!(def.language, "go");
        assert_eq!(def.preset.main.as_deref(), Some("app"));
        assert_eq!(
            def.preset.build.as_deref(),
            Some("CGO_ENABLED=0 go build -o app .")
        );
    }

    #[test]
    fn go_plugin_builds_conventional_worker_binary_when_present() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("cmd/worker")).unwrap();
        std::fs::write(temp.path().join("cmd/worker/main.go"), "package main").unwrap();

        let ctx = PluginContext {
            project_dir: temp.path(),
            package_manager: None,
        };
        let def = GoPlugin.runtime_def(&ctx);

        assert_eq!(
            def.preset.build.as_deref(),
            Some(
                "CGO_ENABLED=0 go build -o app . && CGO_ENABLED=0 go build -o worker ./cmd/worker"
            )
        );
    }

    #[test]
    fn go_plugin_has_no_download_def() {
        let def = GoPlugin.default_runtime_def();
        assert!(
            def.download.is_none(),
            "Go needs no server-side runtime download"
        );
    }

    #[test]
    fn go_plugin_launch_args_uses_main_only() {
        let def = GoPlugin.default_runtime_def();
        assert_eq!(def.server.launch_args, vec!["{main}"]);
        assert!(
            !def.server.launch_args.contains(&"{bin}".to_string()),
            "Go binary runs directly, no {{bin}} placeholder"
        );
    }

    #[test]
    fn go_plugin_watch_patterns() {
        let def = GoPlugin.default_runtime_def();
        assert!(def.preset.watch.contains(&"**/*.go".to_string()));
        assert!(def.preset.watch.contains(&"go.mod".to_string()));
        assert!(def.preset.watch.contains(&"go.sum".to_string()));
    }

    #[test]
    fn go_plugin_dev_command() {
        let def = GoPlugin.default_runtime_def();
        assert_eq!(def.preset.dev, vec!["go", "run", "."]);
    }

    #[test]
    fn go_plugin_no_production_install() {
        let def = GoPlugin.default_runtime_def();
        assert!(
            def.package_manager.install.is_none(),
            "Go binary is self-contained, no production install needed"
        );
    }

    #[test]
    fn go_plugin_has_dev_install() {
        let def = GoPlugin.default_runtime_def();
        assert_eq!(
            def.package_manager
                .development
                .as_ref()
                .and_then(|d| d.install.as_deref()),
            Some("go mod download")
        );
    }

    #[test]
    fn go_plugin_package_manager_fields() {
        let def = GoPlugin.default_runtime_def();
        assert_eq!(def.package_manager.id, "go");
        assert_eq!(def.package_manager.lockfiles, vec!["go.sum"]);
        assert_eq!(def.package_manager.add.as_deref(), Some("go get {package}"));
    }

    #[test]
    fn go_plugin_envs_are_empty() {
        let def = GoPlugin.default_runtime_def();
        assert!(
            def.envs.environments.is_empty(),
            "Go doesn't need NODE_ENV or equivalent"
        );
    }
}
