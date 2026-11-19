//! Validate source references before Compose may read files, then validate the
//! normalized model again before it can reach the privileged Docker socket.
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub(crate) fn validate_source(
    model: &Value,
    root: &Path,
    env: &HashMap<String, String>,
) -> Result<(), String> {
    let object = model.as_object().ok_or("Compose must be an object")?;
    for key in object.keys() {
        if !key.starts_with("x-")
            && !matches!(
                key.as_str(),
                "name" | "version" | "services" | "networks" | "volumes" | "configs" | "secrets"
            )
        {
            return Err(format!("Compose {key} is not supported in production"));
        }
    }
    validate_resources(model, true, Some(env))?;
    for kind in ["configs", "secrets"] {
        for resource in model[kind].as_object().into_iter().flat_map(|v| v.values()) {
            if let Some(file) = resource.get("file") {
                source_path(file, root, root, "config/secret file")?;
            }
        }
    }
    for (name, service) in model["services"]
        .as_object()
        .ok_or("Compose has no services")?
    {
        validate_service(name, service)?;
        if let Some(build) = service.get("build") {
            validate_build(build, root, true)?;
        }
        if let Some(files) = service.get("env_file") {
            let files = files
                .as_array()
                .cloned()
                .unwrap_or_else(|| vec![files.clone()]);
            for file in files {
                source_path(file.get("path").unwrap_or(&file), root, root, "env_file")?;
            }
        }
        for volume in service["volumes"].as_array().into_iter().flatten() {
            if let Some(short) = volume.as_str() {
                let parts: Vec<_> = short.split(':').collect();
                if parts.len() > 1
                    && (parts[0].starts_with('.')
                        || parts[0].starts_with('/')
                        || parts[0].contains('$')
                        || parts[0].contains('/'))
                {
                    source_path(&Value::String(parts[0].into()), root, root, "bind mount")?;
                }
            } else if volume["type"] == "bind" {
                source_path(&volume["source"], root, root, "bind mount")?;
            }
        }
    }
    Ok(())
}

/// Variables Tako owns in every `sdk: true` service.
pub(crate) const SDK_RESERVED_ENV: &[&str] = &[
    "TAKO_APP_NAME",
    "TAKO_INTERNAL_SOCKET",
    "TAKO_BOOTSTRAP_DATA",
    "TAKO_DATA_DIR",
    "TAKO_BUILD",
    "TAKO_COMPOSE_SERVICE",
];
/// Paths Tako mounts into every `sdk: true` service.
pub(crate) const SDK_MOUNTS: &[&str] = &[crate::SDK_SOCKET_MOUNT, crate::SDK_DATA_MOUNT];

/// Tako owns the runtime contract of `x-tako` services: a service cannot set
/// it itself or mount over it.
pub(crate) fn validate_tako_services(
    model: &Value,
    config: &tako_core::compose::ComposeConfig,
) -> Result<(), String> {
    for (name, settings) in &config.services {
        let Some(service) = model["services"].get(name) else {
            continue;
        };
        let environment = service["environment"].as_object();
        let defines = |key: &str| environment.is_some_and(|env| env.contains_key(key));
        if defines("TAKO_COMPOSE_SERVICE") {
            return Err(format!(
                "Compose service '{name}': TAKO_COMPOSE_SERVICE is set by Tako"
            ));
        }
        if !settings.sdk {
            continue;
        }
        for key in SDK_RESERVED_ENV {
            if defines(key) {
                return Err(format!(
                    "Compose service '{name}' is marked sdk: true; Tako sets {key}, so remove it from its environment"
                ));
            }
        }
        if settings.is_http() {
            for key in ["HOST", "PORT"] {
                if defines(key) {
                    return Err(format!(
                        "Compose service '{name}' is an SDK HTTP app; Tako sets {key}, so remove it from its environment"
                    ));
                }
            }
        }
        for volume in service["volumes"].as_array().into_iter().flatten() {
            let Some(target) = volume["target"].as_str() else {
                continue;
            };
            for mount in SDK_MOUNTS {
                if Path::new(target).starts_with(mount) || Path::new(mount).starts_with(target) {
                    return Err(format!(
                        "Compose service '{name}' is marked sdk: true; its volumes must not overlap the managed {mount} mount"
                    ));
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_service(name: &str, service: &Value) -> Result<(), String> {
    // An allowlist also fails closed when Compose introduces a new host-facing
    // feature (providers, lifecycle hooks, API socket injection, devices, etc.).
    for key in service
        .as_object()
        .ok_or("Compose service must be an object")?
        .keys()
    {
        if key.starts_with("x-") {
            continue;
        }
        if !matches!(
            key.as_str(),
            "image"
                | "build"
                | "pull_policy"
                | "command"
                | "entrypoint"
                | "environment"
                | "env_file"
                | "ports"
                | "expose"
                | "volumes"
                | "tmpfs"
                | "networks"
                | "configs"
                | "secrets"
                | "depends_on"
                | "healthcheck"
                | "restart"
                | "stop_grace_period"
                | "stop_signal"
                | "user"
                | "working_dir"
                | "read_only"
                | "init"
                | "tty"
                | "stdin_open"
                | "hostname"
                | "domainname"
                | "dns"
                | "dns_opt"
                | "dns_search"
                | "extra_hosts"
                | "labels"
                | "annotations"
                | "logging"
                | "deploy"
                | "scale"
                | "cpus"
                | "cpu_count"
                | "cpu_percent"
                | "cpu_period"
                | "cpu_quota"
                | "cpu_shares"
                | "cpuset"
                | "mem_limit"
                | "mem_reservation"
                | "mem_swappiness"
                | "memswap_limit"
                | "pids_limit"
                | "shm_size"
                | "ulimits"
                | "cap_drop"
                | "security_opt"
                | "platform"
                | "attach"
                | "oom_kill_disable"
                | "oom_score_adj"
        ) {
            return Err(format!(
                "Compose service '{name}': {key} is not supported in production"
            ));
        }
    }
    if let Some(options) = service.get("security_opt") {
        if !options.as_array().is_some_and(|options| {
            options.iter().all(|value| {
                matches!(
                    value.as_str(),
                    Some("no-new-privileges" | "no-new-privileges:true" | "no-new-privileges=true")
                )
            })
        }) {
            return Err("Compose security_opt supports no-new-privileges only".into());
        }
    }
    if let Some(logging) = service.get("logging") {
        if logging
            .get("driver")
            .is_some_and(|value| !matches!(value.as_str(), Some("json-file" | "local" | "none")))
        {
            return Err("Compose logging supports json-file, local, or none only".into());
        }
        for key in logging["options"]
            .as_object()
            .into_iter()
            .flat_map(|v| v.keys())
        {
            if !matches!(key.as_str(), "max-size" | "max-file" | "compress") {
                return Err(format!("Compose logging option '{key}' is not supported"));
            }
        }
    }
    if let Some(deploy) = service["deploy"].as_object() {
        for key in deploy.keys() {
            if key == "placement"
                && deploy[key]
                    .as_object()
                    .is_some_and(|value| value.is_empty())
            {
                continue;
            }
            if !matches!(
                key.as_str(),
                "replicas" | "resources" | "update_config" | "rollback_config" | "mode"
            ) {
                return Err(format!("Compose deploy.{key} is not supported"));
            }
        }
        if deploy.get("mode").is_some_and(|mode| mode != "replicated") {
            return Err("Compose deploy.mode supports replicated only".into());
        }
        for kind in ["limits", "reservations"] {
            for key in service["deploy"]["resources"][kind]
                .as_object()
                .into_iter()
                .flat_map(|v| v.keys())
            {
                if !matches!(key.as_str(), "cpus" | "memory" | "pids") {
                    return Err(format!(
                        "Compose deploy.resources.{kind}.{key} is not supported"
                    ));
                }
            }
        }
    }
    for volume in service["volumes"].as_array().into_iter().flatten() {
        if let Some(kind) = volume.get("type") {
            if !matches!(kind.as_str(), Some("bind" | "volume" | "tmpfs")) {
                return Err("Compose mounts support bind, volume, and tmpfs only".into());
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_resources(
    model: &Value,
    source: bool,
    env: Option<&HashMap<String, String>>,
) -> Result<(), String> {
    for kind in ["volumes", "networks", "configs", "secrets"] {
        for (name, resource) in model[kind].as_object().into_iter().flatten() {
            if resource.get("external").is_some_and(|v| v != false)
                || (source && resource.get("name").is_some())
            {
                return Err(format!(
                    "Compose {kind}.{name} must be scoped to this app; external resources and custom names are not supported"
                ));
            }
            let allowed = match kind {
                "volumes" => &["name", "external", "driver", "labels"][..],
                "networks" => &[
                    "name",
                    "external",
                    "driver",
                    "labels",
                    "internal",
                    "attachable",
                    "enable_ipv4",
                    "enable_ipv6",
                ][..],
                _ => &[
                    "name",
                    "external",
                    "file",
                    "content",
                    "environment",
                    "labels",
                ][..],
            };
            for key in resource.as_object().into_iter().flat_map(|v| v.keys()) {
                // Compose emits an empty IPAM object for the default network.
                if !source
                    && kind == "networks"
                    && key == "ipam"
                    && resource[key]
                        .as_object()
                        .is_some_and(|value| value.is_empty())
                {
                    continue;
                }
                if !key.starts_with("x-") && !allowed.contains(&key.as_str()) {
                    return Err(format!("Compose {kind}.{name}.{key} is not supported"));
                }
            }
            let driver = if kind == "volumes" { "local" } else { "bridge" };
            if matches!(kind, "volumes" | "networks")
                && resource.get("driver").is_some_and(|v| v != driver)
            {
                return Err(format!("Compose {kind} requires the {driver} driver"));
            }
            if let Some(key) = resource["environment"].as_str() {
                if env.is_some_and(|env| !env.contains_key(key)) {
                    return Err(
                        "Compose resource environment must reference an app variable or secret"
                            .into(),
                    );
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_build(build: &Value, root: &Path, source: bool) -> Result<(), String> {
    let context = if build.is_string() {
        build.clone()
    } else {
        build
            .get("context")
            .cloned()
            .unwrap_or(Value::String(".".into()))
    };
    let context = checked_path(&context, root, root, "build context", source)?;
    if !context.is_dir() {
        return Err("Compose build context must be a directory".into());
    }
    for key in build.as_object().into_iter().flat_map(|v| v.keys()) {
        if !matches!(
            key.as_str(),
            "context"
                | "dockerfile"
                | "dockerfile_inline"
                | "args"
                | "target"
                | "platforms"
                | "no_cache"
                | "pull"
                | "labels"
                | "additional_contexts"
                | "secrets"
                | "shm_size"
        ) {
            return Err(format!(
                "Compose build.{key} is not supported in production"
            ));
        }
    }
    if let Some(file) = build.get("dockerfile") {
        checked_path(file, &context, root, "Dockerfile", source)?;
    } else if build.get("dockerfile_inline").is_none() {
        checked_path(
            &Value::String("Dockerfile".into()),
            &context,
            root,
            "Dockerfile",
            source,
        )?;
    }
    let contexts = &build["additional_contexts"];
    let values = if let Some(map) = contexts.as_object() {
        map.values().cloned().collect::<Vec<_>>()
    } else {
        contexts
            .as_array()
            .into_iter()
            .flatten()
            .map(|v| {
                v.as_str()
                    .and_then(|v| v.split_once('='))
                    .map(|(_, path)| Value::String(path.into()))
                    .unwrap_or(Value::Null)
            })
            .collect()
    };
    for value in values {
        if value
            .as_str()
            .is_some_and(|v| v.starts_with("service:") && !v.contains('$'))
        {
            continue;
        }
        checked_path(&value, root, root, "additional build context", source)?;
    }
    Ok(())
}

fn source_path(value: &Value, base: &Path, root: &Path, kind: &str) -> Result<PathBuf, String> {
    checked_path(value, base, root, kind, true)
}

pub(crate) fn validate_resource_files(model: &Value, root: &Path) -> Result<(), String> {
    for kind in ["configs", "secrets"] {
        for resource in model[kind].as_object().into_iter().flat_map(|v| v.values()) {
            if let Some(path) = resource.get("file") {
                checked_path(path, root, root, "config/secret file", false)?;
            }
        }
    }
    Ok(())
}

fn checked_path(
    value: &Value,
    base: &Path,
    root: &Path,
    kind: &str,
    source: bool,
) -> Result<PathBuf, String> {
    let path = value
        .as_str()
        .ok_or_else(|| format!("Compose {kind} requires a file path"))?;
    if source
        && (Path::new(path).is_absolute()
            || path.contains('$')
            || path.starts_with('~')
            || path.contains('\\')
            || path.contains("://"))
    {
        return Err(format!(
            "Compose {kind} must be a literal relative path inside the app directory"
        ));
    }
    let path = base
        .join(path)
        .canonicalize()
        .map_err(|_| format!("Cannot read Compose {kind}"))?;
    if !path.starts_with(root.canonicalize().map_err(|e| e.to_string())?) {
        return Err(format!("Compose {kind} must stay inside the app directory"));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sdk_services_cannot_set_or_mount_over_the_runtime_contract() {
        let config = tako_core::compose::ComposeConfig {
            file: "compose.yaml".into(),
            services: serde_json::from_value(json!({
                "web": {"sdk": true, "http": true},
                "worker": {"sdk": true},
                "grafana": {"http": true}
            }))
            .unwrap(),
        };
        let check = |service: &str, definition: Value| {
            validate_tako_services(&json!({"services": {service: definition}}), &config)
        };
        for key in SDK_RESERVED_ENV {
            for service in ["web", "worker"] {
                let error = check(service, json!({"environment": {*key: "x"}})).unwrap_err();
                assert!(error.contains(key), "{error}");
            }
        }
        for key in ["HOST", "PORT"] {
            assert!(check("web", json!({"environment": {key: "1"}})).is_err());
            // Workers and plain apps own their HOST and PORT.
            check("worker", json!({"environment": {key: "1"}})).unwrap();
            check("grafana", json!({"environment": {key: "1"}})).unwrap();
        }
        // Plain apps keep their environment, apart from the service marker.
        check("grafana", json!({"environment": {"TAKO_APP_NAME": "x"}})).unwrap();
        assert!(
            check(
                "grafana",
                json!({"environment": {"TAKO_COMPOSE_SERVICE": "x"}})
            )
            .is_err()
        );
        for target in ["/run/tako", "/run", "/tako/data/cache", "/tako"] {
            let error = check(
                "worker",
                json!({"volumes": [{"type": "volume", "source": "data", "target": target}]}),
            )
            .unwrap_err();
            assert!(error.contains("managed"), "{target}: {error}");
            check(
                "grafana",
                json!({"volumes": [{"type": "volume", "source": "data", "target": target}]}),
            )
            .unwrap();
        }
        check(
            "web",
            json!({"environment": {"APP": "1"}, "volumes": [{"type": "volume", "source": "data", "target": "/data"}]}),
        )
        .unwrap();
    }

    #[test]
    fn production_rejects_host_privileges_and_cross_app_resources() {
        for (key, value) in [
            ("privileged", json!(true)),
            ("cap_add", json!(["SYS_ADMIN"])),
            ("devices", json!(["/dev/sda:/dev/sda"])),
            ("pid", json!("host")),
            ("ipc", json!("host")),
            ("network_mode", json!("host")),
            ("userns_mode", json!("host")),
            ("use_api_socket", json!(true)),
            ("provider", json!({"type":"host-program"})),
            ("security_opt", json!(["seccomp:unconfined"])),
            (
                "logging",
                json!({"driver":"syslog", "options":{"syslog-address":"unix:///private/socket"}}),
            ),
            (
                "deploy",
                json!({"resources":{"reservations":{"devices":[{"capabilities":["gpu"]}]}}}),
            ),
        ] {
            let mut service = json!({"image":"busybox"});
            service[key] = value;
            assert!(validate_service("web", &service).is_err(), "{key}");
        }
        for model in [
            json!({"volumes":{"data":{"external":true}}}),
            json!({"volumes":{"data":{"name":"another-app-data"}}}),
            json!({"volumes":{"data":{"driver_opts":{"type":"none","device":"/opt/tako","o":"bind"}}}}),
            json!({"networks":{"default":{"external":true}}}),
            json!({"networks":{"default":{"driver":"host"}}}),
        ] {
            assert!(validate_resources(&model, true, None).is_err());
        }
    }
}
