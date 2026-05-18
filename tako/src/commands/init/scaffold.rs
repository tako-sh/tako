use std::path::Path;

use crate::build::{BuildAdapter, PresetDefinition};
use crate::config::DEFAULT_JS_APP_ROOT;

pub(super) fn parse_csv_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

pub(super) fn sanitize_route(route: &str) -> String {
    let s = route.trim();
    let s = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(s);
    s.trim_end_matches('/').to_string()
}

pub(super) fn infer_default_main_entrypoint(project_dir: &Path, adapter: BuildAdapter) -> String {
    if let Some(main) = adapter.infer_main_entrypoint(project_dir) {
        return main;
    }

    const CANDIDATES: &[&str] = &[
        "index.ts",
        "index.tsx",
        "index.js",
        "index.jsx",
        "src/index.ts",
        "src/index.tsx",
        "src/index.js",
        "src/index.jsx",
        "server/index.mjs",
        "server/index.ts",
        "server/index.js",
        "main.py",
        "main.rb",
        "main.go",
    ];

    for candidate in CANDIDATES {
        if project_dir.join(candidate).is_file() {
            return (*candidate).to_string();
        }
    }

    "index.ts".to_string()
}

pub(super) fn detect_js_app_root(project_dir: &Path) -> String {
    for candidate in [DEFAULT_JS_APP_ROOT, "app", "."] {
        let dir = if candidate == "." {
            project_dir.to_path_buf()
        } else {
            project_dir.join(candidate)
        };
        if dir.join("tako.d.ts").is_file()
            || dir.join("tako.gen.ts").is_file()
            || dir.join("channels").is_dir()
            || dir.join("workflows").is_dir()
        {
            return candidate.to_string();
        }
    }
    DEFAULT_JS_APP_ROOT.to_string()
}

pub(super) fn preset_default_main(
    preset_ref: &str,
    adapter: BuildAdapter,
    group_presets: &[PresetDefinition],
) -> Option<String> {
    match preset_ref {
        "bun" | "node" => {
            let def = adapter.runtime_def()?;
            def.preset.main
        }
        _ => group_presets
            .iter()
            .find(|preset| preset.name == preset_ref)
            .and_then(|preset| preset.main.clone()),
    }
}

pub(super) fn detect_local_runtime_version(runtime: &str) -> Option<String> {
    let mut child = std::process::Command::new(runtime)
        .arg("--version")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(_) => return None,
        }
    }

    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    let version = raw
        .lines()
        .find(|line| !line.trim().is_empty())?
        .trim()
        .trim_start_matches(|c: char| !c.is_ascii_digit())
        .trim();
    if version.is_empty() {
        return None;
    }
    Some(version.to_string())
}

pub(super) fn sdk_install_command(runtime: BuildAdapter, project_dir: &Path) -> Option<String> {
    let ctx = tako_runtime::PluginContext {
        project_dir,
        package_manager: None,
    };
    let def = tako_runtime::runtime_def_for(runtime.id(), Some(&ctx))?;
    let add_cmd = def.package_manager.add?;
    Some(add_cmd.replace("{package}", "tako.sh"))
}

pub(super) struct TemplateParams<'a> {
    pub(super) app_name: &'a str,
    pub(super) app_root: Option<&'a str>,
    pub(super) main: Option<&'a str>,
    pub(super) production_route: &'a str,
    pub(super) runtime: Option<&'a str>,
    pub(super) runtime_version: Option<&'a str>,
    pub(super) package_manager: Option<&'a str>,
    pub(super) preset_ref: Option<&'a str>,
    pub(super) assets: &'a [String],
    pub(super) excludes: &'a [String],
}

pub(super) fn generate_template(params: &TemplateParams<'_>) -> String {
    let TemplateParams {
        app_name,
        app_root,
        main,
        production_route,
        runtime,
        runtime_version,
        package_manager,
        preset_ref,
        assets,
        excludes,
    } = params;
    let app_root = app_root
        .map(str::trim)
        .filter(|root| !root.is_empty() && *root != "src");
    let app_root_block = if let Some(app_root) = app_root {
        format!(
            r#"
# JavaScript app root, relative to this file.
# Tako discovers `channels/` and `workflows/` inside it.
# Use "." if those files live next to `tako.toml`.
# `main`, `assets`, and build paths stay relative to this file.
app_root = "{}"
"#,
            app_root
        )
    } else {
        String::new()
    };
    let main_line = if let Some(main) = main {
        format!(
            "# Required: runtime entrypoint used by `tako dev` and `tako deploy` (relative to project root).\nmain = \"{}\"",
            main
        )
    } else {
        "# Entrypoint comes from the selected preset default `main`.\n# main = \"index.ts\""
            .to_string()
    };
    let runtime_line = match (runtime, runtime_version) {
        (Some(runtime), Some(version)) => format!("runtime = \"{}@{}\"", runtime, version),
        (Some(runtime), None) => format!("runtime = \"{}\"", runtime),
        (None, Some(version)) => format!("# runtime = \"bun@{}\"", version),
        (None, None) => "# runtime = \"bun@1.0.0\"".to_string(),
    };
    let package_manager_line = if let Some(pm) = package_manager {
        format!("package_manager = \"{}\"", pm)
    } else {
        "# package_manager = \"npm\"".to_string()
    };
    let preset_example = match runtime {
        Some("bun") => "tanstack-start",
        Some("node") => "my-node-preset",
        _ => "my-preset",
    };
    let preset_line = if let Some(preset_ref) = preset_ref {
        format!("preset = \"{}\"", preset_ref)
    } else {
        format!("# preset = \"{}\"", preset_example)
    };
    let assets_line = if assets.is_empty() {
        "# assets = [\"public\", \".output/public\"]".to_string()
    } else {
        let items: Vec<String> = assets
            .iter()
            .map(|asset| format!("\"{}\"", asset))
            .collect();
        format!("assets = [{}]", items.join(", "))
    };
    let exclude_line = if excludes.is_empty() {
        "# exclude = [\"**/*.map\"]".to_string()
    } else {
        let items: Vec<String> = excludes
            .iter()
            .map(|exclude| format!("\"{}\"", exclude))
            .collect();
        format!("exclude = [{}]", items.join(", "))
    };
    format!(
        r#"# Tako configuration
# tako.toml reference: https://tako.sh/docs/tako-toml

# Stable app identifier used for deploy paths and local dev hostnames.
# Keep it unique per server. Renaming creates a new app path.
# If you rename it, delete the old deployment manually with `tako delete`.
name = "{app_name}"
{app_root_block}
{main_line}

# Build runtime and optional @version pin for runtime/build lifecycle defaults.
{runtime_line}
{package_manager_line}

# App preset (provides main + assets defaults).
{preset_line}
{assets_line}

# Build configuration.
# [build]
# run = "bun run build"
# install = "bun install"
# include = ["dist/**", ".output/**"]
{exclude_line}

# Multi-stage build (mutually exclusive with [build].run).
# [[build_stages]]
# name = "frontend-assets"
# cwd = "frontend"
# install = "bun install"
# run = "bun run build"

# Global environment variables applied to every environment.
# [vars]
# API_BASE_URL = "https://api.example.com"

# Environment-specific variable overrides merged on top of [vars].
# [vars.production]
# API_BASE_URL = "https://api.example.com"

# [vars.staging]
# API_BASE_URL = "https://staging-api.example.com"

# Environment declarations. Deploy environments must define `route` or `routes`.
[envs.production]
route = "{production_route}"

# Development routes are optional; default is `{app_name}.test`.
# External dev routes are treated as additional host aliases.
# [envs.development]
# route = "{app_name}.test"

# Optional: use multiple routes instead of `route`.
# routes = ["{app_name}.example.com", "www.{app_name}.example.com"]

# Environment sections define routes, server membership, and idle scale-down.
# Set environment variables in [vars] and [vars.<environment>].

# [envs.staging]
# route = "staging.{app_name}.example.com"
# routes = ["staging.{app_name}.example.com", "www.staging.{app_name}.example.com"]
# servers = ["production"]
# idle_timeout = 300

# [envs.staging]
# route = "staging.{app_name}.example.com"
# servers = ["staging"]
# idle_timeout = 120
"#,
        app_name = app_name,
        main_line = main_line,
        runtime_line = runtime_line,
        preset_line = preset_line,
        production_route = production_route
    )
}
