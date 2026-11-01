use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::ComposeRuntime;

pub(crate) const SERVICE_LABEL: &str = "sh.tako.compose.service";
pub(crate) const SLOT_LABEL: &str = "sh.tako.compose.slot";
pub(crate) const FINGERPRINT_LABEL: &str = "sh.tako.compose.fingerprint";
pub(crate) const RELEASE_LABEL: &str = "sh.tako.compose.release";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateOrder {
    StartFirst,
    StopFirst,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependencyCondition {
    Started,
    Healthy,
    Completed,
}

// Do not derive Debug: the resolved model includes secret environment values.
pub(crate) struct ServicePlan {
    pub name: String,
    pub fingerprint: String,
    pub replicas: u32,
    pub order: UpdateOrder,
    pub monitor: Duration,
    pub dependencies: BTreeMap<String, DependencyCondition>,
    pub completed: bool,
}

pub(crate) struct Plan {
    pub model: Value,
    pub services: Vec<ServicePlan>,
}

impl Plan {
    pub fn new(mut model: Value, runtime: &ComposeRuntime) -> Result<Self, String> {
        let definitions = model["services"]
            .as_object_mut()
            .ok_or("Compose has no services")?;
        let mut plans = BTreeMap::new();
        for (name, definition) in definitions.iter_mut() {
            validate_service(name, definition)?;
            // Builds belong to preparation. Compare stable prepared image
            // identities; the runtime pins creation to exact inspected IDs.
            definition.as_object_mut().unwrap().remove("build");
            definition.as_object_mut().unwrap().remove("pull_policy");
            definition["ports"] = json!([]);
            let replicas = definition
                .pointer("/deploy/replicas")
                .or_else(|| definition.get("scale"))
                .and_then(Value::as_u64)
                .unwrap_or(1);
            if replicas > 128 || (name == &runtime.config.service && replicas != 1) {
                return Err(format!(
                    "Compose service '{name}': HTTP ingress requires one replica; other services support 0..128 replicas"
                ));
            }
            let writable_volumes = definition["volumes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|v| v["type"] == "volume" && v["read_only"] != true)
                .filter_map(|v| v["source"].as_str().map(str::to_owned))
                .collect::<BTreeSet<_>>();
            let explicit = definition
                .pointer("/deploy/update_config/order")
                .and_then(Value::as_str);
            let order = match explicit {
                Some("start-first") => UpdateOrder::StartFirst,
                Some("stop-first") => UpdateOrder::StopFirst,
                None if name == &runtime.config.service && writable_volumes.is_empty() => {
                    UpdateOrder::StartFirst
                }
                None => UpdateOrder::StopFirst,
                Some(_) => {
                    return Err(format!(
                        "Compose service '{name}': update order must be start-first or stop-first"
                    ));
                }
            };
            let monitor = match definition.pointer("/deploy/update_config/monitor") {
                Some(value) => parse_duration(value)?,
                None => Duration::from_secs(5),
            };
            let mut dependencies = BTreeMap::new();
            if let Some(deps) = definition["depends_on"].as_object() {
                for (dependency, options) in deps {
                    if options.get("required") == Some(&Value::Bool(false))
                        || options["restart"] == true
                    {
                        return Err(format!(
                            "Compose service '{name}': optional dependencies and depends_on.restart are not supported in production"
                        ));
                    }
                    let condition = match options["condition"].as_str().unwrap_or("service_started")
                    {
                        "service_started" => DependencyCondition::Started,
                        "service_healthy" => DependencyCondition::Healthy,
                        "service_completed_successfully" => DependencyCondition::Completed,
                        _ => {
                            return Err(format!(
                                "Compose service '{name}': unsupported dependency condition"
                            ));
                        }
                    };
                    dependencies.insert(dependency.clone(), condition);
                }
            }
            plans.insert(
                name.clone(),
                ServicePlan {
                    name: name.clone(),
                    fingerprint: String::new(),
                    replicas: replicas as u32,
                    order,
                    monitor,
                    dependencies,
                    completed: false,
                },
            );
        }
        let completed: BTreeSet<String> = plans
            .values()
            .flat_map(|p| p.dependencies.iter())
            .filter(|(_, c)| **c == DependencyCondition::Completed)
            .map(|(name, _)| name.clone())
            .collect();
        for name in completed {
            let plan = plans
                .get_mut(&name)
                .ok_or_else(|| format!("Unknown Compose dependency '{name}'"))?;
            if name == runtime.config.service {
                return Err("The HTTP ingress cannot be a completed dependency job".into());
            }
            plan.completed = true;
            plan.order = UpdateOrder::StopFirst;
        }
        // Fingerprint only what affects this service's containers, including the
        // referenced resource definitions and file contents, never unrelated services.
        for plan in plans.values_mut() {
            let mut definition = model["services"][&plan.name].clone();
            let object = definition.as_object_mut().unwrap();
            for key in ["depends_on", "scale"] {
                object.remove(key);
            }
            if let Some(deploy) = object.get_mut("deploy").and_then(Value::as_object_mut) {
                deploy.remove("replicas");
                deploy.remove("update_config");
                if deploy.is_empty() {
                    object.remove("deploy");
                }
            }
            let mut identity = json!({"service": definition});
            for kind in ["networks", "volumes", "configs", "secrets"] {
                let references = referenced_resources(&definition, kind);
                for reference in references {
                    let resource = &model[kind][&reference];
                    identity[kind][&reference] = resource.clone();
                    if let Some(file) = resource["file"].as_str() {
                        let relative = relative_input(file, runtime.working_directory())?;
                        identity[kind][&reference]["file"] = json!(relative);
                        identity["files"][&relative] = json!(hash_project_file(
                            Path::new(file),
                            runtime.working_directory()
                        )?);
                    }
                }
            }
            for volume in identity["service"]["volumes"]
                .as_array_mut()
                .into_iter()
                .flatten()
            {
                if volume["type"] == "bind" {
                    let path = volume["source"]
                        .as_str()
                        .ok_or("Bind mount has no source")?;
                    let relative = relative_input(path, runtime.working_directory())?;
                    let contents = hash_project_file(Path::new(path), runtime.working_directory())?;
                    volume["source"] = json!(relative);
                    // Store the content identity on this mount, not in the
                    // actual Compose model used to create containers.
                    volume["x-tako-content"] = json!(contents);
                }
            }
            if plan.name == runtime.config.service {
                identity["ingress"] =
                    serde_json::to_value(&runtime.config).map_err(|e| e.to_string())?;
                identity["ingress"].as_object_mut().unwrap().remove("file");
            }
            plan.fingerprint = hex::encode(Sha256::digest(
                serde_json::to_vec(&identity).map_err(|e| e.to_string())?,
            ));
        }
        let mut ordered = Vec::new();
        let mut visiting = BTreeSet::new();
        let mut visited = BTreeSet::new();
        for name in plans.keys() {
            visit(name, &plans, &mut visiting, &mut visited, &mut ordered)?;
        }
        Ok(Self {
            model,
            services: ordered
                .into_iter()
                .map(|name| plans.remove(&name).unwrap())
                .collect(),
        })
    }

    pub fn container_model(
        &self,
        service: &ServicePlan,
        slot: u32,
        runtime: &ComposeRuntime,
    ) -> Value {
        let mut model = self.model.clone();
        let mut definition = model["services"][&service.name].clone();
        let object = definition.as_object_mut().unwrap();
        object.remove("depends_on");
        object.remove("scale");
        if let Some(deploy) = object.get_mut("deploy").and_then(Value::as_object_mut) {
            deploy.remove("update_config");
            deploy.insert("replicas".into(), json!(1));
        }
        if let Some(networks) = object.get_mut("networks").and_then(Value::as_object_mut) {
            for options in networks.values_mut() {
                if options.is_null() {
                    *options = json!({});
                }
                let mut aliases = options["aliases"].as_array().cloned().unwrap_or_default();
                if !aliases.contains(&json!(service.name)) {
                    aliases.push(json!(service.name));
                }
                options["aliases"] = json!(aliases);
            }
        }
        definition["labels"][SERVICE_LABEL] = json!(service.name);
        definition["labels"][SLOT_LABEL] = json!(slot.to_string());
        definition["labels"][FINGERPRINT_LABEL] = json!(service.fingerprint);
        definition["labels"][RELEASE_LABEL] = json!(runtime.directory);
        if service.name == runtime.config.service {
            // Docker allocates the loopback port atomically; reuse it when adopting
            // this generation after a restart or an unchanged deployment.
            definition["ports"] = json!([{"host_ip":"127.0.0.1", "published":"0", "target":runtime.config.port, "protocol":"tcp"}]);
        }
        model["services"] = json!({ (service.generation(slot)): definition });
        model
    }
}

fn relative_input(path: &str, root: &Path) -> Result<String, String> {
    Path::new(path)
        .canonicalize()
        .map_err(|e| e.to_string())?
        .strip_prefix(root.canonicalize().map_err(|e| e.to_string())?)
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|_| "Compose input must stay inside the app directory".into())
}

impl ServicePlan {
    pub fn generation(&self, slot: u32) -> String {
        format!("{}-{slot}-{}", self.name, &self.fingerprint[..24])
    }
}

fn referenced_resources(service: &Value, kind: &str) -> BTreeSet<String> {
    if kind == "networks" {
        return service[kind]
            .as_object()
            .into_iter()
            .flat_map(|v| v.keys().cloned())
            .collect();
    }
    service[kind]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|v| kind != "volumes" || v["type"] == "volume")
        .filter_map(|v| v["source"].as_str().map(str::to_owned))
        .collect()
}

fn visit(
    name: &str,
    plans: &BTreeMap<String, ServicePlan>,
    visiting: &mut BTreeSet<String>,
    visited: &mut BTreeSet<String>,
    ordered: &mut Vec<String>,
) -> Result<(), String> {
    if visited.contains(name) {
        return Ok(());
    }
    if !visiting.insert(name.into()) {
        return Err(format!("Compose dependency cycle at '{name}'"));
    }
    let plan = plans
        .get(name)
        .ok_or_else(|| format!("Unknown Compose dependency '{name}'"))?;
    for dependency in plan.dependencies.keys() {
        let dependency_plan = plans
            .get(dependency)
            .ok_or_else(|| format!("Unknown Compose dependency '{dependency}'"))?;
        if dependency_plan.replicas == 0 {
            return Err(format!(
                "Required Compose dependency '{dependency}' has zero replicas"
            ));
        }
        visit(dependency, plans, visiting, visited, ordered)?;
    }
    visiting.remove(name);
    visited.insert(name.into());
    ordered.push(name.into());
    Ok(())
}

fn validate_service(name: &str, service: &Value) -> Result<(), String> {
    crate::validation::validate_service(name, service)?;
    for key in [
        "container_name",
        "network_mode",
        "profiles",
        "links",
        "external_links",
        "volumes_from",
        "develop",
        "post_start",
        "pre_stop",
    ] {
        if service.get(key).is_some() {
            return Err(format!(
                "Compose service '{name}': {key} is not supported by production service updates"
            ));
        }
    }
    for key in ["ipc", "pid"] {
        if service[key]
            .as_str()
            .is_some_and(|s| s.starts_with("service:"))
        {
            return Err(format!(
                "Compose service '{name}': shared service namespaces are not supported"
            ));
        }
    }
    if let Some(update) = service
        .pointer("/deploy/update_config")
        .and_then(Value::as_object)
    {
        for key in update.keys() {
            if !matches!(key.as_str(), "order" | "monitor") {
                return Err(format!(
                    "Compose service '{name}': update_config supports order and monitor only"
                ));
            }
        }
    }
    if service.pointer("/deploy/rollback_config").is_some() {
        return Err(format!(
            "Compose service '{name}': rollback_config is not supported; Tako restores the previous deployment on failure"
        ));
    }
    if service["labels"]
        .as_object()
        .is_some_and(|labels| labels.keys().any(|k| k.starts_with("sh.tako.compose.")))
    {
        return Err(format!(
            "Compose service '{name}': sh.tako.compose labels are reserved"
        ));
    }
    for v in service["volumes"].as_array().into_iter().flatten() {
        if v["type"] == "volume" && v["source"].as_str().is_none_or(str::is_empty) {
            return Err(format!(
                "Compose service '{name}': anonymous volumes cannot survive service replacement; declare a named volume"
            ));
        }
    }
    Ok(())
}

fn parse_duration(value: &Value) -> Result<Duration, String> {
    // compose-go emits normalized durations as strings. Limit monitoring so a
    // typo cannot hold the app deployment lock indefinitely.
    let text = value
        .as_str()
        .ok_or("update_config.monitor must be a duration")?;
    let mut rest = text;
    let mut seconds = 0.0f64;
    while !rest.is_empty() {
        let end = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .ok_or("Invalid update monitor duration")?;
        let count: f64 = rest[..end]
            .parse()
            .map_err(|_| "Invalid update monitor duration")?;
        rest = &rest[end..];
        let (unit, multiplier) = [
            ("ms", 0.001),
            ("us", 0.000001),
            ("µs", 0.000001),
            ("ns", 0.000000001),
            ("h", 3600.0),
            ("m", 60.0),
            ("s", 1.0),
        ]
        .into_iter()
        .find(|(unit, _)| rest.starts_with(unit))
        .ok_or("Invalid update monitor duration")?;
        seconds += count * multiplier;
        rest = &rest[unit.len()..];
    }
    if !seconds.is_finite() || !(0.0..=120.0).contains(&seconds) {
        return Err("update_config.monitor must be between 0s and 2m".into());
    }
    Ok(Duration::from_secs_f64(seconds))
}

fn hash_project_file(path: &Path, directory: &Path) -> Result<String, String> {
    let path = path
        .canonicalize()
        .map_err(|_| "Cannot read a Compose bind/config/secret file")?;
    let root = directory.canonicalize().map_err(|e| e.to_string())?;
    if !path.starts_with(root) {
        return Err("Compose bind/config/secret files must stay inside the app directory".into());
    }
    fn hash(path: &Path, root: &Path, digest: &mut Sha256) -> Result<(), String> {
        let canonical = path
            .canonicalize()
            .map_err(|_| "Cannot read a Compose mounted file")?;
        if !canonical.starts_with(root) {
            return Err("Compose mounted files must stay inside the app directory".into());
        }
        if path
            .symlink_metadata()
            .map_err(|e| e.to_string())?
            .file_type()
            .is_symlink()
        {
            return Err("Compose mounted files must not contain symlinks".into());
        }
        if path.is_dir() {
            let mut entries = std::fs::read_dir(path)
                .map_err(|e| e.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?;
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                digest.update(entry.file_name().as_encoded_bytes());
                digest.update([0]);
                hash(&entry.path(), root, digest)?;
            }
        } else {
            use std::io::Read;
            let mut file =
                std::fs::File::open(path).map_err(|_| "Cannot read a Compose mounted file")?;
            let mut buffer = [0u8; 16 * 1024];
            loop {
                let n = file.read(&mut buffer).map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                digest.update(&buffer[..n]);
            }
        }
        Ok(())
    }
    let mut digest = Sha256::new();
    hash(
        &path,
        &directory.canonicalize().map_err(|e| e.to_string())?,
        &mut digest,
    )?;
    Ok(hex::encode(digest.finalize()))
}

#[cfg(test)]
mod tests;
