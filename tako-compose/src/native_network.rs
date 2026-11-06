use serde_json::{Value, json};
use std::net::Ipv4Addr;

const RESOURCE: &str = "tako_native_services";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeNetwork {
    network_id: String,
    name: String,
    bridge: String,
    gateway: Ipv4Addr,
}

impl NativeNetwork {
    pub fn new(network_id: &str) -> Result<Self, String> {
        if network_id.len() != 32
            || !network_id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("native network ID must be 32 lowercase hex characters".into());
        }
        let bytes = hex::decode(network_id).map_err(|e| e.to_string())?;
        // Mirrors tako-network-helper.py: 10.64.0.0/11 stays clear of the private
        // ranges clouds and container platforms hand out by default, which the
        // helper's host route check would otherwise refuse.
        Ok(Self {
            network_id: network_id.into(),
            name: format!("tako-svc-{}", &network_id[..12]),
            bridge: format!("tks{}", &network_id[..12]),
            gateway: Ipv4Addr::new(10, 64 + bytes[6] % 32, bytes[7], 1),
        })
    }

    pub fn network_id(&self) -> &str {
        &self.network_id
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn bridge(&self) -> &str {
        &self.bridge
    }
    pub fn gateway(&self) -> Ipv4Addr {
        self.gateway
    }
    pub fn subnet(&self) -> String {
        let [a, b, c, _] = self.gateway.octets();
        format!("{a}.{b}.{c}.0/24")
    }

    pub(crate) async fn ensure_bridge(&self) -> Result<(), String> {
        let mut command =
            tokio::process::Command::new("/usr/local/libexec/tako/tako-network-helper");
        command
            .args([self.network_id(), "bridge"])
            .env_clear()
            .kill_on_drop(true)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        crate::process_output::check_cancellation().map_err(|e| e.to_string())?;
        let child = command
            .spawn()
            .map_err(|_| "Could not provision the native Compose bridge".to_string())?;
        let output = crate::process_output::capture(
            child,
            Some(std::time::Duration::from_secs(60)),
            crate::process_output::StdoutMode::Complete,
        )
        .await
        .map_err(|_| "Could not provision the native Compose bridge".to_string())?;
        // The helper answers with JSON on both outcomes. A refusal names the host
        // conflict (such as a route overlapping this bridge's subnet) so the
        // operator can act on it instead of a generic failure.
        let response: Value = serde_json::from_slice(&output.stdout)
            .map_err(|_| "Invalid native bridge helper response".to_string())?;
        if response["ok"] == false
            && let Some(reason) = response["error"].as_str()
        {
            return Err(format!(
                "Native Compose bridge {} was refused: {reason}",
                self.subnet()
            ));
        }
        if !output.status.is_some_and(|status| status.success())
            || response["ok"] != true
            || response["network_id"] != self.network_id
            || response["action"] != "bridge"
        {
            return Err("Native Compose bridge was not acknowledged".into());
        }
        Ok(())
    }

    pub(crate) fn is_resource(name: &str) -> bool {
        name == RESOURCE
    }

    pub(crate) fn apply(&self, model: &mut Value) -> Result<(), String> {
        if model
            .get("networks")
            .and_then(|v| v.get(RESOURCE))
            .is_some()
        {
            return Err("Compose network name tako_native_services is reserved".into());
        }
        let services = model["services"]
            .as_object_mut()
            .ok_or("Compose has no services")?;
        let mut needs_default = false;
        for service in services.values_mut() {
            let definition = service.as_object_mut().ok_or("invalid Compose service")?;
            let networks = definition.entry("networks").or_insert_with(|| {
                needs_default = true;
                json!({"default": {}})
            });
            let networks = networks
                .as_object_mut()
                .ok_or("expected resolved Compose networks")?;
            if networks.contains_key(RESOURCE) {
                return Err("Compose service references reserved native network".into());
            }
            networks.insert(RESOURCE.into(), json!({}));
            definition.insert("dns".into(), json!([self.gateway.to_string()]));
        }
        if model.get("networks").is_none() {
            model["networks"] = json!({});
        }
        let networks = model["networks"]
            .as_object_mut()
            .ok_or("invalid Compose networks")?;
        if needs_default {
            networks.entry("default").or_insert_with(|| json!({}));
        }
        networks.insert(RESOURCE.into(), json!({"external":true,"name":self.name}));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_identity_is_deterministic_and_cannot_come_from_app_input() {
        let network = NativeNetwork::new("00112233445566778899aabbccddeeff").unwrap();
        assert_eq!(network.name(), "tako-svc-001122334455");
        assert_eq!(network.bridge(), "tks001122334455");
        assert_eq!(network.gateway().to_string(), "10.70.119.1");
        assert_eq!(network.subnet(), "10.70.119.0/24");
        assert!(NativeNetwork::new("../../evil").is_err());
        assert!(NativeNetwork::new("00112233445566778899AABBCCDDEEFF").is_err());
    }

    #[test]
    fn managed_dns_and_network_preserve_stack_networks_and_application_environment() {
        let network = NativeNetwork::new("00112233445566778899aabbccddeeff").unwrap();
        let mut model = json!({
            "services": {
                "web": {"networks":{"frontend":{"aliases":["api"]}},"dns":["8.8.8.8"],"environment":{"PORT":"8080"}},
                "db": {"image":"postgres"}
            },
            "networks":{"frontend":{"internal":true}}
        });
        network.apply(&mut model).unwrap();
        assert_eq!(
            model["services"]["web"]["networks"]["frontend"],
            json!({"aliases":["api"]})
        );
        assert_eq!(
            model["services"]["web"]["environment"],
            json!({"PORT":"8080"})
        );
        assert_eq!(
            model["services"]["db"]["networks"],
            json!({"default":{},RESOURCE:{}})
        );
        assert_eq!(model["services"]["web"]["dns"], json!(["10.70.119.1"]));
        assert_eq!(
            model["networks"][RESOURCE],
            json!({"external":true,"name":network.name()})
        );
        assert_eq!(model["networks"]["frontend"], json!({"internal":true}));
        assert!(network.apply(&mut model).is_err());
    }
}
