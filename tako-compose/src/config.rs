//! Read Tako service metadata without Docker or secret interpolation.
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use tako_core::compose::{ComposeConfig, ComposeServiceConfig};

pub fn load_config(directory: &Path, file: &str) -> Result<ComposeConfig, String> {
    let mut config = ComposeConfig {
        file: file.into(),
        ..Default::default()
    };
    config.validate()?;
    let root = directory.canonicalize().map_err(|e| e.to_string())?;
    let path = directory
        .join(file)
        .canonicalize()
        .map_err(|e| format!("Compose file: {e}"))?;
    if !path.starts_with(&root) {
        return Err("Compose file must stay inside the app directory".into());
    }
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|e| e.to_string())?
        .take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > 16 * 1024 * 1024 {
        return Err("Compose source exceeds 16 MiB".into());
    }
    let mut yaml: serde_yaml_ng::Value =
        serde_yaml_ng::from_slice(&bytes).map_err(|_| "Invalid Compose YAML")?;
    yaml.apply_merge()
        .map_err(|_| "Invalid Compose YAML merge")?;
    let model = serde_json::to_value(yaml).map_err(|_| "Invalid Compose model")?;
    config.services = service_configs(&model)?;
    config.validate()?;
    Ok(config)
}

pub(crate) fn service_configs(
    model: &Value,
) -> Result<BTreeMap<String, ComposeServiceConfig>, String> {
    model["services"]
        .as_object()
        .ok_or("Compose has no services")?
        .iter()
        .filter(|(_, service)| service.get("x-tako").is_some())
        .map(|(name, service)| {
            let config = serde_json::from_value(service["x-tako"].clone())
                .map_err(|e| format!("Compose service '{name}', x-tako: {e}"))?;
            Ok((name.clone(), config))
        })
        .collect()
}

/// Prefer explicit Compose exposure; image metadata is only a fallback.
/// Multiple TCP candidates must be disambiguated with a single `expose` entry.
pub fn ingress_port(service: &str, definition: &Value, image_ports: &Value) -> Result<u16, String> {
    let mut ports = BTreeSet::new();
    let exposed = definition["expose"].as_array().filter(|v| !v.is_empty());
    if let Some(exposed) = exposed {
        for port in exposed {
            add_port(&mut ports, port)?;
        }
    } else if let Some(published) = definition["ports"].as_array().filter(|v| !v.is_empty()) {
        for port in published {
            if port["protocol"].as_str().unwrap_or("tcp") == "tcp" {
                add_port(&mut ports, &port["target"])?;
            }
        }
    } else if let Some(exposed) = image_ports.as_object() {
        for port in exposed.keys() {
            add_port(&mut ports, &Value::String(port.clone()))?;
        }
    }
    if ports.len() != 1 {
        return Err(format!(
            "Compose service '{service}' needs exactly one HTTP TCP port; declare it with expose: [\"<container-port>\"] (found {})",
            ports.len()
        ));
    }
    Ok(*ports.first().unwrap())
}

fn add_port(ports: &mut BTreeSet<u16>, port: &Value) -> Result<(), String> {
    let value = port
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| port.to_string());
    let (number, protocol) = value.split_once('/').unwrap_or((&value, "tcp"));
    if protocol != "tcp" {
        return Ok(());
    }
    let number: u16 = number
        .parse()
        .ok()
        .filter(|n| *n > 0)
        .ok_or("Compose HTTP exposure must be one TCP port, without a range")?;
    ports.insert(number);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn marker_defines_apps_independently_of_routes() {
        let model = json!({"services": {
            "web": {"x-tako": {"envs": {"production": {"route": "example.com"}}}},
            "api": {"x-tako": {"envs": {"production": {"routes": ["api.example.com", "example.com/api/*"]}}}},
            "private": {"x-tako": {}},
            "redis": {"image": "redis"}
        }});
        let config = ComposeConfig {
            file: "compose.yaml".into(),
            services: service_configs(&model).unwrap(),
        };
        config.validate().unwrap();
        assert_eq!(
            config
                .services
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["api", "private", "web"]
        );
        assert_eq!(
            config.routes("production"),
            ["api.example.com", "example.com/api/*", "example.com"]
        );
        assert!(config.default_service().unwrap_err().contains("--service"));
    }

    #[test]
    fn ports_come_from_compose_or_image_metadata_without_tako_settings() {
        let image = json!({"80/tcp":{}, "443/tcp":{}});
        assert_eq!(
            ingress_port("web", &json!({"expose":[3000]}), &image).unwrap(),
            3000
        );
        assert_eq!(
            ingress_port(
                "web",
                &json!({"ports":[{"target":8080,"published":"9999"}]}),
                &image
            )
            .unwrap(),
            8080
        );
        assert_eq!(
            ingress_port("web", &json!({}), &json!({"80/tcp":{}, "53/udp":{}})).unwrap(),
            80
        );
        for definition in [
            json!({}),
            json!({"expose":[80,443]}),
            json!({"expose":["0"]}),
            json!({"expose":["80-90"]}),
        ] {
            assert!(ingress_port("web", &definition, &Value::Null).is_err());
        }
        assert!(
            ingress_port("web", &json!({}), &image)
                .unwrap_err()
                .contains("expose")
        );
    }

    #[test]
    fn service_settings_are_strict_and_routes_are_unambiguous() {
        for extension in [
            json!({"health_path":"/health"}),
            json!({"port":3000}),
            json!({"envs":{"production":{"route":"a", "routes":["b"]}}}),
        ] {
            let model = json!({"services":{"web":{"x-tako":extension}}});
            let result = service_configs(&model).and_then(|services| {
                ComposeConfig {
                    file: "compose.yaml".into(),
                    services,
                }
                .validate()
            });
            assert!(result.is_err());
        }
        let services = service_configs(&json!({"services":{
            "web":{"x-tako":{"envs":{"production":{"route":"example.com"}}}},
            "api":{"x-tako":{"envs":{"production":{"route":"example.com"}}}}
        }}))
        .unwrap();
        assert!(
            ComposeConfig {
                file: "compose.yaml".into(),
                services
            }
            .validate()
            .unwrap_err()
            .contains("both")
        );
    }

    #[test]
    fn route_duplicates_follow_hostname_and_path_matching() {
        let validate = |web: &str, api: &str| {
            ComposeConfig {
                file: "compose.yaml".into(),
                services: service_configs(&json!({"services": {
                    "web": {"x-tako": {"envs": {"production": {"route": web}}}},
                    "api": {"x-tako": {"envs": {"production": {"route": api}}}}
                }}))
                .unwrap(),
            }
            .validate()
        };
        assert!(validate("EXAMPLE.com", "example.com").is_err());
        assert!(validate("example.com/api/", "example.com/api").is_err());
        assert!(validate("example.com/API", "example.com/api").is_ok());
        assert!(validate("example.com", "example.com/api/*").is_ok());
    }

    #[test]
    fn yaml_anchors_work_and_outside_files_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("compose.yaml"), "x-defaults: &defaults\n  envs:\n    production:\n      source_ip: direct\nservices:\n  api:\n    x-tako:\n      <<: *defaults\n  redis:\n    image: redis\n").unwrap();
        let config = load_config(dir.path(), "compose.yaml").unwrap();
        assert_eq!(config.services.len(), 1);
        assert_eq!(
            config.services["api"].envs["production"].source_ip,
            Some(tako_core::SourceIpMode::Direct)
        );
        assert!(load_config(dir.path(), "../compose.yaml").is_err());
    }
}
