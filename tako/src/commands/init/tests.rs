use super::{
    build_preset_selection_options, display_config_path_for_prompt,
    ensure_project_gitignore_tracks_secrets, production_route_needs_dns,
    push_history_if_interactive, resolve_adapter,
};
use crate::build::{BuildAdapter, PresetDefinition};
use crate::commands::init::presets::normalize_group_preset_definitions;
use crate::commands::init::scaffold::{
    TemplateParams, detect_js_app_root, generate_template, infer_default_main_entrypoint,
    parse_csv_list, preset_default_main, sdk_install_command,
};
use tempfile::TempDir;

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
fn init_offers_dns_setup_for_wildcard_production_route() {
    assert!(production_route_needs_dns("*.demo-app.example.com"));
    assert!(production_route_needs_dns("*.demo-app.example.com/api"));
}

#[test]
fn init_skips_dns_setup_for_exact_production_route() {
    assert!(!production_route_needs_dns("demo-app.example.com"));
    assert!(!production_route_needs_dns("demo-app.example.com/api"));
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
    assert!(rendered.contains("runtime_version = \"1.2.3\""));
    assert!(!rendered.contains("# runtime_version"));
}

#[test]
fn init_template_comments_runtime_version_when_absent() {
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
    assert!(rendered.contains("# runtime_version = \"1.0.0\""));
    assert!(!rendered.contains("\nruntime_version = \""));
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
fn display_config_path_for_prompt_uses_path_relative_to_current_directory() {
    let temp = TempDir::new().unwrap();
    let cwd = std::fs::canonicalize(temp.path()).unwrap();
    let config_path = cwd.join("tako.toml");
    std::fs::write(&config_path, "name = \"demo\"\n").unwrap();

    assert_eq!(
        display_config_path_for_prompt(&config_path, &cwd),
        "tako.toml"
    );
}

#[test]
fn display_config_path_for_prompt_keeps_subdirectory_when_relative_to_current_directory() {
    let temp = TempDir::new().unwrap();
    let cwd = std::fs::canonicalize(temp.path()).unwrap();
    let project_dir = cwd.join("apps/web");
    std::fs::create_dir_all(&project_dir).unwrap();
    let config_path = project_dir.join("preview.toml");
    std::fs::write(&config_path, "name = \"demo\"\n").unwrap();

    assert_eq!(
        display_config_path_for_prompt(&config_path, &cwd),
        "apps/web/preview.toml"
    );
}

#[test]
fn display_config_path_for_prompt_falls_back_to_absolute_path_outside_current_directory() {
    let temp = TempDir::new().unwrap();
    let cwd = std::fs::canonicalize(temp.path()).unwrap();

    let outside = TempDir::new().unwrap();
    let config_path = std::fs::canonicalize(outside.path())
        .unwrap()
        .join("preview.toml");
    std::fs::write(&config_path, "name = \"demo\"\n").unwrap();

    assert_eq!(
        display_config_path_for_prompt(&config_path, &cwd),
        config_path.display().to_string()
    );
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
    assert!(super::detect_local_runtime_version("nonexistent-runtime-xyz-123").is_none());
}

#[test]
fn init_gitignore_uses_repo_root_for_nested_project() {
    let temp = TempDir::new().unwrap();
    let repo_root = temp.path();
    let project_dir = repo_root.join("apps/web");
    std::fs::create_dir_all(&project_dir).unwrap();
    std::fs::write(repo_root.join(".git"), "gitdir: /tmp/fake-git-dir\n").unwrap();

    ensure_project_gitignore_tracks_secrets(&project_dir).unwrap();

    let gitignore = std::fs::read_to_string(repo_root.join(".gitignore")).unwrap();
    assert!(
        gitignore.contains("**/.tako/*\n!**/.tako/secrets.json\n"),
        "expected repo root .gitignore to contain global tako rules: {gitignore}"
    );
    assert!(
        !project_dir.join(".gitignore").exists(),
        "expected nested app .gitignore to remain untouched"
    );
}

#[test]
fn init_gitignore_falls_back_to_project_dir_outside_git_repo() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().join("app");
    std::fs::create_dir_all(&project_dir).unwrap();

    ensure_project_gitignore_tracks_secrets(&project_dir).unwrap();

    let gitignore = std::fs::read_to_string(project_dir.join(".gitignore")).unwrap();
    assert!(
        gitignore.contains("**/.tako/*\n!**/.tako/secrets.json\n"),
        "expected project-local .gitignore when no repo root is found: {gitignore}"
    );
}

#[test]
fn init_gitignore_does_not_duplicate_existing_rules() {
    let temp = TempDir::new().unwrap();
    let project_dir = temp.path().to_path_buf();

    ensure_project_gitignore_tracks_secrets(&project_dir).unwrap();
    ensure_project_gitignore_tracks_secrets(&project_dir).unwrap();

    let gitignore = std::fs::read_to_string(project_dir.join(".gitignore")).unwrap();
    assert_eq!(
        gitignore.matches("!**/.tako/secrets.json").count(),
        1,
        "expected secrets tracking rule to remain deduplicated"
    );
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
fn normalize_group_preset_names_excludes_base_and_deduplicates() {
    let names = normalize_group_preset_definitions(
        BuildAdapter::Bun,
        vec![
            PresetDefinition {
                name: "".to_string(),
                main: None,
            },
            PresetDefinition {
                name: "bun".to_string(),
                main: None,
            },
            PresetDefinition {
                name: " tanstack-start ".to_string(),
                main: Some("dist/server/tako-entry.mjs".to_string()),
            },
            PresetDefinition {
                name: "tanstack-start".to_string(),
                main: Some("dist/server/ignored.mjs".to_string()),
            },
            PresetDefinition {
                name: "custom".to_string(),
                main: None,
            },
        ],
    );
    assert_eq!(
        names,
        vec![
            PresetDefinition {
                name: "tanstack-start".to_string(),
                main: Some("dist/server/tako-entry.mjs".to_string()),
            },
            PresetDefinition {
                name: "custom".to_string(),
                main: None,
            },
        ]
    );
}

#[test]
fn build_preset_selection_options_returns_none_when_no_group_presets_found() {
    let options = build_preset_selection_options(BuildAdapter::Bun, &[]);
    assert!(options.is_none());
}

#[test]
fn build_preset_selection_options_includes_presets_and_custom_mode() {
    let options = build_preset_selection_options(
        BuildAdapter::Node,
        &["tanstack-start".to_string(), "nextjs".to_string()],
    )
    .expect("options should be available");

    assert_eq!(options.len(), 3);
    assert_eq!(
        options[0],
        (
            "tanstack-start".to_string(),
            Some("tanstack-start".to_string())
        )
    );
    assert_eq!(
        options[1],
        ("nextjs".to_string(), Some("nextjs".to_string()))
    );
    assert_eq!(options[2], ("custom".to_string(), None));
}

#[test]
fn push_history_if_interactive_records_prompted_steps() {
    let mut step_history = vec![0, 1];
    push_history_if_interactive(&mut step_history, 2, true);
    assert_eq!(step_history, vec![0, 1, 2]);
}

#[test]
fn push_history_if_interactive_skips_auto_derived_steps() {
    let mut step_history = vec![0, 1];
    push_history_if_interactive(&mut step_history, 2, false);
    assert_eq!(step_history, vec![0, 1]);
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

#[test]
fn resolve_adapter_uses_existing_config_runtime() {
    use crate::config::TakoToml;
    let existing = TakoToml {
        runtime: Some("node".to_string()),
        ..Default::default()
    };
    assert_eq!(
        resolve_adapter(BuildAdapter::Bun, Some(&existing)),
        BuildAdapter::Node
    );
}

#[test]
fn resolve_adapter_defaults_unknown_detection_to_bun() {
    assert_eq!(
        resolve_adapter(BuildAdapter::Unknown, None),
        BuildAdapter::Bun
    );
}
