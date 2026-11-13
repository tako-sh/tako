use super::{ConfigError, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Local identity pointer with a name read from the shared network descriptor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LocalNetwork {
    pub name: String,
    pub id: String,
    pub device: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalNetworks {
    pub networks: BTreeMap<String, LocalNetwork>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityPointer {
    id: String,
    device: String,
}

fn invalid(error: impl std::fmt::Display) -> ConfigError {
    ConfigError::Validation(error.to_string())
}
fn index_path() -> Result<PathBuf> {
    Ok(crate::paths::tako_config_dir()
        .map_err(invalid)?
        .join("networks")
        .join("identities.json"))
}
impl LocalNetwork {
    pub fn validate(&self) -> Result<()> {
        validate_network_name(&self.name)?;
        validate_network_id(&self.id)?;
        self.device
            .parse::<tako_network::DeviceId>()
            .map_err(invalid)?;
        Ok(())
    }
    pub fn profile_path(&self) -> Result<PathBuf> {
        self.validate()?;
        Ok(crate::paths::tako_config_dir()
            .map_err(invalid)?
            .join("networks")
            .join(&self.id)
            .join(&self.device)
            .join("profile.json"))
    }
}
pub fn validate_network_name(name: &str) -> Result<()> {
    name.parse::<tako_network::NetworkName>()
        .map(|_| ())
        .map_err(invalid)
}
pub fn validate_network_id(id: &str) -> Result<()> {
    id.parse::<tako_network::NetworkId>()
        .map(|_| ())
        .map_err(|_| {
            invalid("Network must be its stable ID. Find it with `tako network list --json`.")
        })
}

impl LocalNetworks {
    pub fn load() -> Result<Self> {
        Self::load_from_file(&index_path()?)
    }
    pub fn load_from_file(path: &Path) -> Result<Self> {
        let pointers = Self::read_pointers(path)?;
        let mut networks = BTreeMap::new();
        for pointer in pointers {
            let mut local = LocalNetwork {
                name: "network".into(),
                id: pointer.id,
                device: pointer.device,
            };
            let profile: crate::network::Profile =
                crate::network::read_json(&local.profile_path()?).map_err(invalid)?;
            if profile.snapshot().network.id.to_string() != local.id
                || profile
                    .request()
                    .device_id_for(&profile.snapshot().network.id)
                    .map_err(invalid)?
                    .to_string()
                    != local.device
            {
                return Err(invalid(
                    "Network identity pointer does not match its saved profile",
                ));
            }
            local.name = profile.snapshot().network.name.to_string();
            if networks.insert(local.id.clone(), local).is_some() {
                return Err(invalid("Duplicate network identity pointer"));
            }
        }
        Ok(Self { networks })
    }
    fn read_pointers(path: &Path) -> Result<Vec<IdentityPointer>> {
        if !path.try_exists().map_err(invalid)? {
            return Ok(vec![]);
        }
        crate::network::read_json(path).map_err(invalid)
    }
    pub fn select(&self, name_or_id: Option<&str>) -> Result<&LocalNetwork> {
        if let Some(value) = name_or_id {
            if let Some(local) = self.networks.get(value) {
                return Ok(local);
            }
            let matches: Vec<_> = self.networks.values().filter(|n| n.name == value).collect();
            return match matches.as_slice() {
                [local] => Ok(local),
                [] => Err(invalid(format!(
                    "Network '{value}' is not configured on this machine"
                ))),
                _ => Err(invalid(format!(
                    "Network name '{value}' is ambiguous; use its ID from `tako network list --json`"
                ))),
            };
        }
        match self.networks.len() {
            0 => Err(invalid(
                "No network configured. Run `tako network create` or `tako network join`.",
            )),
            1 => Ok(self.networks.values().next().unwrap()),
            _ => Err(invalid(
                "More than one network is configured. Select one with --network.",
            )),
        }
    }
    pub fn save_identity(local: &LocalNetwork) -> Result<()> {
        local.validate()?;
        let path = index_path()?;
        crate::network::private_directory(path.parent().unwrap()).map_err(invalid)?;
        let lock =
            crate::network::private_file(&path.with_extension("lock"), true).map_err(invalid)?;
        lock.lock().map_err(invalid)?;
        let mut pointers = Self::read_pointers(&path)?;
        pointers.retain(|p| p.id != local.id);
        pointers.push(IdentityPointer {
            id: local.id.clone(),
            device: local.device.clone(),
        });
        crate::network::write_json(&path, &pointers, true).map_err(invalid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn same_name_requires_an_id_and_pointer_contains_no_name_or_membership() {
        let a = LocalNetwork {
            name: "work".into(),
            id: "a".repeat(32),
            device: "b".repeat(64),
        };
        let mut b = a.clone();
        b.id = "c".repeat(32);
        let networks = LocalNetworks {
            networks: BTreeMap::from([(a.id.clone(), a.clone()), (b.id.clone(), b)]),
        };
        assert!(networks.select(Some("work")).is_err());
        assert_eq!(networks.select(Some(&a.id)).unwrap(), &a);
        let pointer = serde_json::to_value(IdentityPointer {
            id: a.id,
            device: a.device,
        })
        .unwrap();
        assert!(pointer.get("name").is_none());
    }
    #[test]
    fn identity_paths_reject_traversal() {
        let local = LocalNetwork {
            name: "work".into(),
            id: "../secret".into(),
            device: "b".repeat(64),
        };
        assert!(local.profile_path().is_err());
    }
}
