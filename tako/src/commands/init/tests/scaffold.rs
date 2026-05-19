use crate::build::{BuildAdapter, PresetDefinition};
use tempfile::TempDir;

use super::super::scaffold::{
    TemplateParams, detect_js_app_root, detect_local_runtime_version, generate_template,
    infer_default_main_entrypoint, parse_csv_list, preset_default_main, sdk_install_command,
};

#[test]
fn init_template_keeps_only_minimal_options_uncommented() {
    let rendered = generate_template(&TemplateParams {
        app_name: "demo-app",
        app_root: Some("src"),
        main: Some("server/index.mjs"),
        production_route: "demo-app.example.com",
        runtime: Some("bun"),
        runtime_version: None,
        package_manager: None,
        preset_ref: None,
        assets: &[],
        excludes: &[],
    });

    assert!(
        rendered.contains("# Stable app identifier used for deploy paths and local dev hostnames."),
        "expected template to explain app name identity semantics"
    );
    assert!(
        rendered.contains("# Keep it unique per server. Renaming creates a new app path."),
        "expected template to warn that app names must be unique"
    );
    assert!(
        rendered
            .contains("# If you rename it, delete the old deployment manually with `tako delete`."),
        "expected template to explain rename cleanup behavior"
    );
    assert!(
        rendered.contains("\nname = \"demo-app\"\n"),
        "expected app name to be uncommented in minimal template"
    );
    assert!(
        !rendered.contains("app_root"),
        "expected default JavaScript app root to be omitted"
    );
    assert!(
        !rendered.contains("# name = \"demo-app\""),
        "expected app name commented example to be removed"
    );
    assert!(
        rendered.contains("[envs.production]\nroute = \"demo-app.example.com\""),
        "expected production route to remain uncommented"
    );
    assert!(
        rendered.contains("# [envs.development]"),
        "expected development environment section to be optional/commented by default"
    );
    assert!(
        !rendered.contains("[envs.development]\nroute = \"demo-app.tako\""),
        "expected development route not to be uncommented in minimal template"
    );
    assert!(
        rendered.contains("runtime = \"bun\""),
        "expected runtime to be uncommented"
    );
    assert!(
        rendered.contains("# preset = \"tanstack-start\""),
        "expected base runtime preset to be omitted/commented"
    );
    assert!(
        rendered.contains("main = \"server/index.mjs\""),
        "expected required main entrypoint to be uncommented"
    );
    assert!(
        !rendered.contains("# main = \"server/index.mjs\""),
        "expected commented main example to be removed"
    );
    assert!(
        rendered.contains("# assets = [\"public\", \".output/public\"]"),
        "expected optional build assets list to be commented"
    );
    assert!(
        rendered.contains("# [vars]"),
        "expected vars section to be commented"
    );
    assert!(
        rendered.contains("# servers = [\"production\"]"),
        "expected env-local server list example to be commented"
    );
    assert!(
        rendered.contains("# idle_timeout = 300"),
        "expected env-local idle timeout example to be commented"
    );
}

#[test]
fn init_template_includes_reference_link_and_option_examples() {
    let rendered = generate_template(&TemplateParams {
        app_name: "demo-app",
        app_root: Some("src"),
        main: Some("server/index.mjs"),
        production_route: "demo-app.example.com",
        runtime: Some("bun"),
        runtime_version: None,
        package_manager: None,
        preset_ref: None,
        assets: &[],
        excludes: &[],
    });

    assert!(
        rendered.contains("https://tako.sh/docs/tako-toml"),
        "expected link to tako.toml reference docs"
    );
    assert!(
        rendered.contains("# routes = [\"demo-app.example.com\", \"www.demo-app.example.com\"]"),
        "expected routes example in commented options"
    );
    assert!(
        rendered.contains("# include = [\"dist/**\", \".output/**\"]"),
        "expected build include example in commented options"
    );
    assert!(
        rendered.contains("# API_BASE_URL = \"https://api.example.com\""),
        "expected example for environment variables"
    );
    assert!(
        rendered.contains("# idle_timeout = 300"),
        "expected server idle timeout example"
    );
}

#[test]
fn infer_default_main_entrypoint_prefers_existing_file() {
    let temp = TempDir::new().unwrap();
    std::fs::create_dir_all(temp.path().join("server")).unwrap();
    std::fs::write(temp.path().join("server/index.ts"), "export {};").unwrap();
    assert_eq!(
        infer_default_main_entrypoint(temp.path(), BuildAdapter::Unknown),
        "server/index.ts"
    );
}

#[test]
fn infer_default_main_entrypoint_prefers_root_js_extension_order_before_src() {
    let temp = TempDir::new().unwrap();
    std::fs::create_dir_all(temp.path().join("src")).unwrap();
    std::fs::write(temp.path().join("index.jsx"), "export default {};").unwrap();
    std::fs::write(temp.path().join("src/index.ts"), "export {};").unwrap();

    assert_eq!(
        infer_default_main_entrypoint(temp.path(), BuildAdapter::Unknown),
        "index.jsx"
    );
}

#[test]
fn infer_default_main_entrypoint_supports_tsx_candidates() {
    let temp = TempDir::new().unwrap();
    std::fs::create_dir_all(temp.path().join("src")).unwrap();
    std::fs::write(temp.path().join("src/index.tsx"), "export default {};").unwrap();

    assert_eq!(
        infer_default_main_entrypoint(temp.path(), BuildAdapter::Unknown),
        "src/index.tsx"
    );
}

#[test]
fn infer_default_main_entrypoint_falls_back_when_no_candidate_exists() {
    let temp = TempDir::new().unwrap();
    assert_eq!(
        infer_default_main_entrypoint(temp.path(), BuildAdapter::Unknown),
        "index.ts"
    );
}

#[test]
fn infer_default_main_entrypoint_uses_package_json_main_when_file_exists() {
    let temp = TempDir::new().unwrap();
    std::fs::create_dir_all(temp.path().join("app")).unwrap();
    std::fs::write(temp.path().join("app/server.ts"), "export {};").unwrap();
    std::fs::write(
        temp.path().join("package.json"),
        r#"{"name":"demo","main":"app/server.ts"}"#,
    )
    .unwrap();

    assert_eq!(
        infer_default_main_entrypoint(temp.path(), BuildAdapter::Node),
        "app/server.ts"
    );
}

#[test]
fn infer_default_main_entrypoint_skips_nonexistent_package_json_main() {
    let temp = TempDir::new().unwrap();
    std::fs::create_dir_all(temp.path().join("server")).unwrap();
    std::fs::write(temp.path().join("server/index.ts"), "export {};").unwrap();
    std::fs::write(
        temp.path().join("package.json"),
        r#"{"name":"demo","main":"dist/index.js"}"#,
    )
    .unwrap();

    assert_eq!(
        infer_default_main_entrypoint(temp.path(), BuildAdapter::Node),
        "server/index.ts"
    );
}

#[test]
fn init_template_can_omit_main_when_preset_provides_default() {
    let rendered = generate_template(&TemplateParams {
        app_name: "demo-app",
        app_root: Some("src"),
        main: None,
        production_route: "demo-app.example.com",
        runtime: Some("bun"),
        runtime_version: None,
        package_manager: None,
        preset_ref: None,
        assets: &[],
        excludes: &[],
    });
    assert!(rendered.contains("# Entrypoint comes from the selected preset default `main`."));
    assert!(!rendered.contains("\nmain = \""));
}

#[test]
fn init_template_omits_app_root_when_not_javascript() {
    let rendered = generate_template(&TemplateParams {
        app_name: "demo-app",
        app_root: None,
        main: Some("main.go"),
        production_route: "demo-app.example.com",
        runtime: Some("go"),
        runtime_version: None,
        package_manager: None,
        preset_ref: None,
        assets: &[],
        excludes: &[],
    });

    assert!(!rendered.contains("app_root"));
}

#[test]
fn init_template_writes_non_default_app_root() {
    let rendered = generate_template(&TemplateParams {
        app_name: "demo-app",
        app_root: Some("app"),
        main: Some("server/index.mjs"),
        production_route: "demo-app.example.com",
        runtime: Some("bun"),
        runtime_version: None,
        package_manager: None,
        preset_ref: None,
        assets: &[],
        excludes: &[],
    });

    assert!(
        rendered.contains("# JavaScript app root, relative to this file."),
        "expected template to describe non-default app_root"
    );
    assert!(
        rendered.contains("\napp_root = \"app\"\n"),
        "expected non-default JavaScript app root to be written"
    );
}

#[test]
fn init_template_uses_prompted_production_route() {
    let rendered = generate_template(&TemplateParams {
        app_name: "demo-app",
        app_root: Some("src"),
        main: Some("server/index.mjs"),
        production_route: "api.demo-app.com",
        runtime: Some("bun"),
        runtime_version: None,
        package_manager: None,
        preset_ref: None,
        assets: &[],
        excludes: &[],
    });
    assert!(rendered.contains("[envs.production]\nroute = \"api.demo-app.com\""));
    assert!(!rendered.contains("[envs.production]\nroute = \"demo-app.example.com\""));
}

#[test]
fn init_template_can_leave_preset_unset() {
    let rendered = generate_template(&TemplateParams {
        app_name: "demo-app",
        app_root: Some("src"),
        main: None,
        production_route: "demo-app.example.com",
        runtime: Some("node"),
        runtime_version: None,
        package_manager: None,
        preset_ref: None,
        assets: &[],
        excludes: &[],
    });
    assert!(rendered.contains("runtime = \"node\""));
    assert!(rendered.contains("# preset = \"my-node-preset\""));
}

#[test]
fn init_template_writes_selected_build_adapter() {
    let rendered = generate_template(&TemplateParams {
        app_name: "demo-app",
        app_root: Some("src"),
        main: None,
        production_route: "demo-app.example.com",
        runtime: Some("bun"),
        runtime_version: None,
        package_manager: None,
        preset_ref: None,
        assets: &[],
        excludes: &[],
    });
    assert!(rendered.contains("runtime = \"bun\""));
}

#[test]
fn init_template_writes_runtime_local_preset_reference() {
    let rendered = generate_template(&TemplateParams {
        app_name: "demo-app",
        app_root: Some("src"),
        main: None,
        production_route: "demo-app.example.com",
        runtime: Some("bun"),
        runtime_version: None,
        package_manager: None,
        preset_ref: Some("tanstack-start"),
        assets: &[],
        excludes: &[],
    });
    assert!(rendered.contains("preset = \"tanstack-start\""));
    assert!(!rendered.contains("preset = \"js/tanstack-start\""));
}

#[test]
fn init_template_pins_runtime_version_when_provided() {
    let rendered = generate_template(&TemplateParams {
        app_name: "demo-app",
        app_root: Some("src"),
        main: None,
        production_route: "demo-app.example.com",
        runtime: Some("bun"),
        runtime_version: Some("1.2.3"),
        package_manager: None,
        preset_ref: None,
        assets: &[],
        excludes: &[],
    });
    assert!(rendered.contains("runtime = \"bun@1.2.3\""));
}

#[test]
fn init_template_omits_runtime_pin_when_absent() {
    let rendered = generate_template(&TemplateParams {
        app_name: "demo-app",
        app_root: Some("src"),
        main: None,
        production_route: "demo-app.example.com",
        runtime: Some("bun"),
        runtime_version: None,
        package_manager: None,
        preset_ref: None,
        assets: &[],
        excludes: &[],
    });
    assert!(rendered.contains("runtime = \"bun\""));
}

#[test]
fn detect_js_app_root_prefers_existing_src_tako_files() {
    let temp = TempDir::new().unwrap();
    std::fs::create_dir_all(temp.path().join("src").join("workflows")).unwrap();

    assert_eq!(detect_js_app_root(temp.path()), "src");
}

#[test]
fn detect_js_app_root_preserves_root_level_tako_files() {
    let temp = TempDir::new().unwrap();
    std::fs::create_dir_all(temp.path().join("channels")).unwrap();

    assert_eq!(detect_js_app_root(temp.path()), ".");
}

#[test]
fn detect_js_app_root_preserves_existing_app_tako_files() {
    let temp = TempDir::new().unwrap();
    std::fs::create_dir_all(temp.path().join("app").join("workflows")).unwrap();

    assert_eq!(detect_js_app_root(temp.path()), "app");
}

#[test]
fn detect_js_app_root_defaults_to_src_for_new_projects() {
    let temp = TempDir::new().unwrap();

    assert_eq!(detect_js_app_root(temp.path()), "src");
}

#[test]
fn sdk_install_command_uses_runtime_package_manager() {
    let tmp = tempfile::TempDir::new().unwrap();
    assert_eq!(
        sdk_install_command(BuildAdapter::Node, tmp.path()),
        Some("pnpm add tako.sh".to_string())
    );
    assert_eq!(
        sdk_install_command(BuildAdapter::Bun, tmp.path()),
        Some("bun add tako.sh".to_string())
    );
}

#[test]
fn detect_local_runtime_version_returns_none_for_unknown_binary() {
    assert!(detect_local_runtime_version("nonexistent-runtime-xyz-123").is_none());
}

#[test]
fn embedded_bun_preset_default_main_is_set() {
    assert_eq!(
        preset_default_main("bun", BuildAdapter::Bun, &[]),
        Some("src/index.ts".to_string())
    );
}

#[test]
fn embedded_bun_tanstack_start_preset_default_main_is_set() {
    let presets = vec![PresetDefinition {
        name: "tanstack-start".to_string(),
        main: Some("dist/server/tako-entry.mjs".to_string()),
    }];
    assert_eq!(
        preset_default_main("tanstack-start", BuildAdapter::Bun, &presets),
        Some("dist/server/tako-entry.mjs".to_string())
    );
}

#[test]
fn parse_csv_list_trims_items_and_drops_empty_segments() {
    assert_eq!(
        parse_csv_list(" public, .output/public ,, static "),
        vec![
            "public".to_string(),
            ".output/public".to_string(),
            "static".to_string()
        ]
    );
}
