use std::path::{Path, PathBuf};

use crate::build::{
    BuildAdapter, BuildPreset, PresetReference, infer_adapter_from_preset_reference,
    parse_preset_reference, qualify_runtime_local_preset_ref,
};
use crate::config::TakoToml;
use crate::validation::validate_dev_route;

const TAKO_APP_DATA_DIR_ENV: &str = "TAKO_DATA_DIR";
const TAKO_DEV_ALLOWED_HOSTS_ENV: &str = "TAKO_DEV_ALLOWED_HOSTS";

pub(super) fn compute_display_routes(
    cfg: &TakoToml,
    default_host: &str,
    base_domain: Option<&str>,
) -> Vec<String> {
    let configured: Vec<String> = cfg
        .get_routes("development")
        .unwrap_or_default()
        .into_iter()
        .map(|route| match base_domain {
            Some(bd) => route.replace(bd, default_host),
            None => route,
        })
        .collect();

    let has_managed_dev_route = configured.iter().any(|route| is_managed_dev_route(route));
    let mut out = if configured.is_empty() || !has_managed_dev_route {
        vec![default_host.to_string()]
    } else {
        Vec::new()
    };
    out.extend(configured);

    let mut seen = std::collections::HashSet::new();
    out.retain(|r| seen.insert(r.clone()));
    out
}

pub(super) fn sanitize_name_segment(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if (c == '-' || c == '_' || c == '.') && !out.ends_with('-') {
            out.push('-');
        }
    }

    out.trim_matches('-').to_string()
}

pub(super) fn short_path_hash(s: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut hasher);
    format!("{:04x}", hasher.finish() & 0xFFFF)
}

pub(super) fn disambiguate_app_name(
    candidate: &str,
    config_path: &str,
    existing: &[(String, String)],
) -> String {
    let dominated = |name: &str| -> bool {
        existing
            .iter()
            .any(|(n, other_config)| n == name && other_config != config_path)
    };

    if !dominated(candidate) {
        return candidate.to_string();
    }

    if let Some(leaf) = Path::new(config_path)
        .parent()
        .and_then(|dir| dir.file_name())
        .and_then(|n| n.to_str())
    {
        let seg = sanitize_name_segment(leaf);
        if !seg.is_empty() {
            let with_dir = format!("{candidate}-{seg}");
            if !dominated(&with_dir) {
                return with_dir;
            }
        }
    }

    format!("{candidate}-{}", short_path_hash(config_path))
}

pub(super) async fn try_list_registered_app_names() -> Vec<(String, String)> {
    match crate::dev_server_client::list_registered_apps().await {
        Ok(apps) => apps
            .into_iter()
            .map(|a| (a.app_name, a.config_path))
            .collect(),
        Err(_) => Vec::new(),
    }
}

pub(super) fn compute_dev_hosts(
    app_name: &str,
    cfg: &TakoToml,
    default_host: &str,
    base_domain: Option<&str>,
) -> Result<Vec<String>, String> {
    let routes = match cfg.get_routes("development") {
        Some(routes) if !routes.is_empty() => routes,
        _ => return Ok(vec![default_host.to_string()]),
    };

    let mut out: Vec<String> = Vec::new();
    for r in routes {
        validate_dev_route(&r, app_name).map_err(|e| e.to_string())?;
        let r = if let Some(bd) = base_domain {
            r.replace(bd, default_host)
        } else {
            r
        };
        if !r.is_empty() {
            out.push(r);
        }
    }

    if !out.iter().any(|route| is_managed_dev_route(route)) {
        out.insert(0, default_host.to_string());
    }

    let mut seen = std::collections::HashSet::new();
    out.retain(|r| seen.insert(r.clone()));
    Ok(out)
}

fn is_managed_dev_route(route: &str) -> bool {
    let host = route.split('/').next().unwrap_or(route);
    let host = host.strip_prefix("*.").unwrap_or(host);
    host.ends_with(&format!(".{}", crate::dev::SHORT_DEV_DOMAIN))
}

#[cfg(test)]
pub(super) fn route_hostname_matches(route_pattern: &str, request_host: &str) -> bool {
    let host = route_pattern.split('/').next().unwrap_or(route_pattern);
    if host == request_host {
        return true;
    }
    if let Some(suffix) = host.strip_prefix("*.") {
        if request_host == suffix {
            return false;
        }
        return request_host.len() > suffix.len()
            && request_host.as_bytes()[request_host.len() - suffix.len() - 1] == b'.'
            && request_host.ends_with(suffix);
    }
    false
}

pub(super) fn compute_dev_env(cfg: &TakoToml) -> std::collections::HashMap<String, String> {
    let mut env = cfg.get_merged_vars("development");
    env.insert("ENV".to_string(), "development".to_string());
    env.insert("TAKO_BUILD".to_string(), "dev".to_string());
    env
}

pub(super) fn inject_dev_allowed_hosts(
    hosts: &[String],
    env: &mut std::collections::HashMap<String, String>,
) {
    let mut allowed = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for route in hosts {
        let Some(host) = allowed_host_from_route(route) else {
            continue;
        };
        if seen.insert(host.clone()) {
            allowed.push(host);
        }
    }

    if !allowed.is_empty() {
        env.insert(TAKO_DEV_ALLOWED_HOSTS_ENV.to_string(), allowed.join(","));
    }
}

fn allowed_host_from_route(route: &str) -> Option<String> {
    let host = route.split('/').next().unwrap_or(route).trim();
    if host.is_empty() {
        return None;
    }

    if let Some(suffix) = host.strip_prefix("*.") {
        if suffix.is_empty() {
            return None;
        }
        return Some(format!(".{suffix}"));
    }

    Some(host.to_string())
}

pub(super) fn dev_runtime_data_root(project_dir: &Path) -> PathBuf {
    project_dir.join(".tako").join("data")
}

pub(super) fn ensure_dev_runtime_data_dirs(project_dir: &Path) -> Result<PathBuf, String> {
    let data_root = dev_runtime_data_root(project_dir);
    let app_data_dir = data_root.join("app");
    let tako_data_dir = data_root.join("tako");
    std::fs::create_dir_all(&app_data_dir)
        .map_err(|e| format!("create app data dir {}: {e}", app_data_dir.display()))?;
    std::fs::create_dir_all(&tako_data_dir)
        .map_err(|e| format!("create tako data dir {}: {e}", tako_data_dir.display()))?;
    Ok(app_data_dir)
}

pub(super) fn inject_dev_data_dir(
    project_dir: &Path,
    env: &mut std::collections::HashMap<String, String>,
) -> Result<(), String> {
    let app_data_dir = ensure_dev_runtime_data_dirs(project_dir)?;
    env.insert(
        TAKO_APP_DATA_DIR_ENV.to_string(),
        app_data_dir.display().to_string(),
    );
    Ok(())
}

pub(super) fn inject_dev_secrets(
    project_dir: &Path,
    env: &mut std::collections::HashMap<String, String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let secrets = crate::config::SecretsStore::load_from_dir(project_dir)?;

    let encrypted = match secrets.get_env("development") {
        Some(map) if !map.is_empty() => map,
        _ => return Ok(()),
    };

    let key = crate::commands::secret::load_secret_key("development", &secrets, Some(project_dir))?;
    for (name, encrypted_value) in encrypted {
        match crate::crypto::decrypt(encrypted_value, &key) {
            Ok(value) => {
                env.insert(name.clone(), value);
            }
            Err(e) => {
                tracing::warn!("Failed to decrypt development secret {}: {}", name, e);
            }
        }
    }

    Ok(())
}

fn resolve_dev_build_adapter(project_dir: &Path, cfg: &TakoToml) -> Result<BuildAdapter, String> {
    if let Some(adapter_override) = cfg
        .runtime
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return BuildAdapter::from_id(adapter_override).ok_or_else(|| {
            format!(
                "Invalid runtime '{}'; expected one of: bun, node, go",
                adapter_override
            )
        });
    }

    Ok(crate::build::detect_build_adapter(project_dir))
}

pub(super) fn resolve_effective_dev_build_adapter(
    project_dir: &Path,
    cfg: &TakoToml,
    preset_ref: &str,
) -> Result<BuildAdapter, String> {
    let configured_or_detected = resolve_dev_build_adapter(project_dir, cfg)?;
    if configured_or_detected != BuildAdapter::Unknown {
        return Ok(configured_or_detected);
    }

    let inferred = infer_adapter_from_preset_reference(preset_ref);
    if inferred != BuildAdapter::Unknown {
        return Ok(inferred);
    }

    Ok(configured_or_detected)
}

pub(super) fn resolve_dev_preset_ref(project_dir: &Path, cfg: &TakoToml) -> Result<String, String> {
    let runtime = resolve_dev_build_adapter(project_dir, cfg)?;
    if let Some(preset_ref) = cfg
        .preset
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return qualify_runtime_local_preset_ref(runtime, preset_ref);
    }
    Ok(runtime.default_preset().to_string())
}

fn resolve_runtime_default_dev_command(
    runtime_adapter: BuildAdapter,
    main: &str,
) -> Result<Vec<String>, String> {
    let Some(runtime_def) = runtime_adapter.runtime_def() else {
        return Err(
            "Cannot determine default dev command because runtime is unknown. Set top-level `runtime` or set `preset`."
                .to_string(),
        );
    };
    if runtime_def.preset.dev.is_empty() {
        return Err(format!(
            "Runtime '{}' does not define a default `dev` command.",
            runtime_adapter.id()
        ));
    }
    Ok(runtime_def
        .preset
        .dev
        .iter()
        .map(|arg| {
            if arg == "{main}" {
                main.to_string()
            } else {
                arg.clone()
            }
        })
        .collect())
}

/// Build the command that spawns the workflow worker subprocess in dev.
/// Returns `None` when the project has no `workflows/` directory (nothing
/// to run) or the runtime isn't JS (only JS workflows are supported).
///
/// The worker entrypoint path mirrors production: it lives in the linked
/// SDK under `node_modules/tako.sh/dist/entrypoints/{runtime}-worker.mjs`.
/// The worker takes no CLI args — it reads `TAKO_APP_NAME` and
/// `TAKO_INTERNAL_SOCKET` from env.
pub(super) fn resolve_dev_worker_command(
    project_dir: &Path,
    runtime_adapter: BuildAdapter,
) -> Option<Vec<String>> {
    if !project_dir.join("workflows").is_dir() {
        return None;
    }
    let base = "node_modules/tako.sh/dist/entrypoints";
    match runtime_adapter {
        BuildAdapter::Bun => Some(vec![
            "bun".to_string(),
            "run".to_string(),
            format!("{base}/bun-worker.mjs"),
        ]),
        BuildAdapter::Node => Some(vec![
            "node".to_string(),
            "--experimental-strip-types".to_string(),
            format!("{base}/node-worker.mjs"),
        ]),
        BuildAdapter::Go | BuildAdapter::Unknown => None,
    }
}

pub(super) fn has_explicit_dev_preset(cfg: &TakoToml) -> bool {
    cfg.preset
        .as_deref()
        .map(str::trim)
        .is_some_and(|preset| !preset.is_empty())
}

pub(super) fn resolve_dev_run_command(
    cfg: &TakoToml,
    preset: &BuildPreset,
    main: &str,
    runtime_adapter: BuildAdapter,
    _explicit_preset: bool,
    project_dir: &Path,
) -> Result<Vec<String>, String> {
    let abs_main = if Path::new(main).is_absolute() {
        main.to_string()
    } else {
        project_dir.join(main).to_string_lossy().to_string()
    };

    let raw = if !cfg.dev.is_empty() {
        cfg.dev.clone()
    } else if let Some(runtime_dev) = preset
        .runtime_overrides
        .get(runtime_adapter.id())
        .map(|override_def| &override_def.dev)
        .filter(|dev| !dev.is_empty())
    {
        runtime_dev.clone()
    } else if !preset.dev.is_empty() {
        preset.dev.clone()
    } else {
        return resolve_runtime_default_dev_command(runtime_adapter, &abs_main);
    };

    Ok(raw
        .iter()
        .map(|arg| {
            if arg == "{main}" {
                abs_main.clone()
            } else {
                arg.clone()
            }
        })
        .collect())
}

pub(super) fn readiness_failure_hint_for_dev_command(command: &[String]) -> Option<String> {
    if !command_invokes_vite_dev(command) {
        return None;
    }

    Some(
        "Vite started without Tako readiness. Add the `tako.sh/vite` plugin to vite.config.ts so Tako can configure allowed hosts and receive the bound port on fd 4."
            .to_string(),
    )
}

fn command_invokes_vite_dev(command: &[String]) -> bool {
    let has_vite = command.iter().any(|arg| {
        Path::new(arg)
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name == "vite")
            .unwrap_or(arg == "vite")
    });
    let has_dev = command.len() == 1 || command.iter().any(|arg| arg == "dev" || arg == "--host");
    has_vite && has_dev
}

pub(super) fn infer_preset_name_from_ref(preset_ref: &str) -> String {
    match parse_preset_reference(preset_ref) {
        Ok(PresetReference::OfficialAlias { name, .. }) => name,
        Err(_) => "preset".to_string(),
    }
}

pub(super) fn dev_startup_lines(
    verbose: bool,
    app_name: &str,
    runtime_name: &str,
    entry_point: &Path,
    url: &str,
) -> Vec<String> {
    let mut lines = Vec::new();

    if verbose {
        lines.push("Tako Dev Server".to_string());
        lines.push("───────────────────────────────────────".to_string());
        lines.push(format!("App:     {}", app_name));
        lines.push(format!("Runtime: {}", runtime_name));
        lines.push(format!("Entry:   {}", entry_point.display()));
        lines.push(format!("URL:     {}", url));
        lines.push("───────────────────────────────────────".to_string());
    } else {
        lines.push(url.to_string());
    }

    lines
}

pub(super) fn dev_url(domain: &str, public_port: u16) -> String {
    if public_port == 443 {
        format!("https://{}/", domain)
    } else {
        format!("https://{}:{}/", domain, public_port)
    }
}

pub(super) fn preferred_public_url(
    domain: &str,
    daemon_url: &str,
    listen_port: u16,
    display_port: u16,
) -> String {
    if display_port != listen_port || daemon_url.is_empty() {
        dev_url(domain, display_port)
    } else {
        daemon_url.to_string()
    }
}
