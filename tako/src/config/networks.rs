use super::{ConfigError, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// A network this machine belongs to. `alias` is this machine's own name for
/// the network and is unique on it; `name` is the name every member shares,
/// read from the network descriptor, and need not be unique.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LocalNetwork {
    pub alias: String,
    pub name: String,
    pub id: String,
    pub device: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalNetworks {
    /// By stable network ID.
    pub networks: BTreeMap<String, LocalNetwork>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityPointer {
    id: String,
    device: String,
    /// Pointers saved before aliases existed have none; loading gives them one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    alias: Option<String>,
}

fn invalid(error: impl std::fmt::Display) -> ConfigError {
    ConfigError::Validation(error.to_string())
}
fn index_path() -> Result<PathBuf> {
    Ok(crate::paths::network_state_dir()
        .map_err(invalid)?
        .join("networks")
        .join("identities.json"))
}
impl LocalNetwork {
    pub fn validate(&self) -> Result<()> {
        validate_network_alias(&self.alias)?;
        validate_network_name(&self.name)?;
        validate_network_id(&self.id)?;
        self.device
            .parse::<tako_network::DeviceId>()
            .map_err(invalid)?;
        Ok(())
    }
    pub fn profile_path(&self) -> Result<PathBuf> {
        validate_network_id(&self.id)?;
        self.device
            .parse::<tako_network::DeviceId>()
            .map_err(invalid)?;
        Ok(crate::paths::network_state_dir()
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
/// Aliases follow network-name rules and never look like a network ID, so
/// `--network` reads the same value the same way on every machine.
pub fn validate_network_alias(alias: &str) -> Result<()> {
    if alias.parse::<tako_network::NetworkName>().is_err() {
        return Err(invalid(format!(
            "Network alias '{alias}' must start with a lowercase letter and use only lowercase letters, digits, and '-', at most 63 characters"
        )));
    }
    if alias.parse::<tako_network::NetworkId>().is_ok() {
        return Err(invalid(format!(
            "Network alias '{alias}' looks like a network ID; choose another"
        )));
    }
    Ok(())
}
pub fn validate_network_id(id: &str) -> Result<()> {
    id.parse::<tako_network::NetworkId>()
        .map(|_| ())
        .map_err(|_| invalid(format!("'{id}' is not a network ID")))
}

impl LocalNetworks {
    pub fn load() -> Result<Self> {
        Self::load_from_file(&index_path()?)
    }
    pub fn load_from_file(path: &Path) -> Result<Self> {
        let pointers = Self::read_pointers(path)?;
        let mut networks = BTreeMap::new();
        let mut unnamed = Vec::new();
        for pointer in pointers {
            let mut local = LocalNetwork {
                alias: String::new(),
                name: String::new(),
                id: pointer.id,
                device: pointer.device,
            };
            let path = local.profile_path()?;
            let profile: crate::network::Profile =
                crate::network::read_json(&path).map_err(|error| {
                    invalid(format!(
                        "Could not read network profile {}: {error}",
                        path.display()
                    ))
                })?;
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
            match pointer.alias {
                Some(alias) => {
                    validate_network_alias(&alias)?;
                    local.alias = alias;
                }
                None => unnamed.push(local.id.clone()),
            }
            if networks.insert(local.id.clone(), local).is_some() {
                return Err(invalid("Duplicate network identity pointer"));
            }
        }
        let mut seen = BTreeSet::new();
        for local in networks.values().filter(|local| !local.alias.is_empty()) {
            if !seen.insert(local.alias.clone()) {
                return Err(invalid(format!(
                    "Two networks on this machine share the alias '{}'; rename one with `tako network rename`",
                    local.alias
                )));
            }
        }
        let mut loaded = Self { networks };
        for id in unnamed {
            let alias = loaded.free_alias(&loaded.networks[&id].name);
            loaded.networks.get_mut(&id).unwrap().alias = alias;
        }
        Ok(loaded)
    }
    fn read_pointers(path: &Path) -> Result<Vec<IdentityPointer>> {
        if !path.try_exists().map_err(invalid)? {
            return Ok(vec![]);
        }
        crate::network::read_json(path).map_err(invalid)
    }

    /// The network with this alias, or with this stable ID.
    pub fn find(&self, alias_or_id: &str) -> Option<&LocalNetwork> {
        self.networks
            .values()
            .find(|local| local.alias == alias_or_id)
            .or_else(|| self.networks.get(alias_or_id))
    }

    /// The named network, or the only one. Several networks and no name is an
    /// error: there is no default network, so the caller asks or fails.
    pub fn select(&self, alias_or_id: Option<&str>) -> Result<&LocalNetwork> {
        if let Some(value) = alias_or_id {
            return self.find(value).ok_or_else(|| {
                invalid(format!(
                    "Network '{value}' is not on this machine. Run `tako network list` to see its networks."
                ))
            });
        }
        let mut networks = self.networks.values();
        match (networks.next(), networks.next()) {
            (None, _) => Err(invalid(
                "No network on this machine. Run `tako network create` or `tako network join`.",
            )),
            (Some(only), None) => Ok(only),
            _ => Err(invalid(format!(
                "This machine has several networks. Pass --network with one of: {}.",
                self.aliases().join(", ")
            ))),
        }
    }

    /// Aliases in a stable order for choices and messages.
    pub fn aliases(&self) -> Vec<String> {
        let mut aliases: Vec<_> = self
            .networks
            .values()
            .map(|local| local.alias.clone())
            .collect();
        aliases.sort();
        aliases
    }

    /// `name` when no network here uses it as an alias, else `name-2`,
    /// `name-3`, and so on.
    pub fn free_alias(&self, name: &str) -> String {
        let taken = |alias: &str| self.networks.values().any(|local| local.alias == alias);
        if !taken(name) && validate_network_alias(name).is_ok() {
            return name.to_string();
        }
        (2..)
            .map(|n| {
                let suffix = format!("-{n}");
                let base = &name[..name.len().min(63 - suffix.len())];
                format!("{}{suffix}", base.trim_end_matches('-'))
            })
            .find(|alias| !taken(alias) && validate_network_alias(alias).is_ok())
            .unwrap()
    }

    /// Record a network's identity under its alias. The alias must not name
    /// another network on this machine.
    pub fn save_identity(local: &LocalNetwork) -> Result<()> {
        local.validate()?;
        let path = index_path()?;
        crate::network::private_directory(path.parent().unwrap()).map_err(invalid)?;
        let lock =
            crate::network::private_file(&path.with_extension("lock"), true).map_err(invalid)?;
        lock.lock().map_err(invalid)?;
        let mut networks = Self::load_from_file(&path)?;
        if let Some(other) = networks
            .networks
            .values()
            .find(|other| other.alias == local.alias && other.id != local.id)
        {
            return Err(invalid(format!(
                "Alias '{}' already names network {} on this machine; choose another",
                local.alias, other.id
            )));
        }
        networks.networks.insert(local.id.clone(), local.clone());
        networks.write(&path)
    }

    /// Give a network a different alias on this machine. Other members keep
    /// their own aliases and the shared name.
    pub fn rename(alias_or_id: &str, alias: &str) -> Result<LocalNetwork> {
        validate_network_alias(alias)?;
        let path = index_path()?;
        crate::network::private_directory(path.parent().unwrap()).map_err(invalid)?;
        let lock =
            crate::network::private_file(&path.with_extension("lock"), true).map_err(invalid)?;
        lock.lock().map_err(invalid)?;
        let mut networks = Self::load_from_file(&path)?;
        let mut local = networks.select(Some(alias_or_id))?.clone();
        if let Some(other) = networks.find(alias)
            && other.id != local.id
        {
            return Err(invalid(format!(
                "Alias '{alias}' already names network {} on this machine",
                other.id
            )));
        }
        local.alias = alias.to_string();
        networks.networks.insert(local.id.clone(), local.clone());
        networks.write(&path)?;
        Ok(local)
    }

    fn write(&self, path: &Path) -> Result<()> {
        let pointers: Vec<_> = self
            .networks
            .values()
            .map(|local| IdentityPointer {
                id: local.id.clone(),
                device: local.device.clone(),
                alias: Some(local.alias.clone()),
            })
            .collect();
        crate::network::write_json(path, &pointers, true).map_err(invalid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn network(alias: &str, name: &str, id: char) -> LocalNetwork {
        LocalNetwork {
            alias: alias.into(),
            name: name.into(),
            id: id.to_string().repeat(32),
            device: "b".repeat(64),
        }
    }

    #[test]
    fn aliases_select_networks_that_share_a_name() {
        let a = network("work", "work", 'a');
        let b = network("work-2", "work", 'c');
        let networks = LocalNetworks {
            networks: BTreeMap::from([(a.id.clone(), a.clone()), (b.id.clone(), b.clone())]),
        };
        assert_eq!(networks.select(Some("work")).unwrap(), &a);
        assert_eq!(networks.select(Some("work-2")).unwrap(), &b);
        assert_eq!(networks.select(Some(&b.id)).unwrap(), &b);
        assert!(networks.select(Some("missing")).is_err());
        // No default: several networks and no name fails with every alias.
        let error = networks.select(None).unwrap_err().to_string();
        assert!(error.contains("work, work-2"), "{error}");
        let only = LocalNetworks {
            networks: BTreeMap::from([(a.id.clone(), a.clone())]),
        };
        assert_eq!(only.select(None).unwrap(), &a);
        assert!(LocalNetworks::default().select(None).is_err());
    }

    #[test]
    fn free_alias_suffixes_a_taken_name() {
        let a = network("work", "work", 'a');
        let mut networks = LocalNetworks {
            networks: BTreeMap::from([(a.id.clone(), a)]),
        };
        assert_eq!(networks.free_alias("home"), "home");
        assert_eq!(networks.free_alias("work"), "work-2");
        let b = network("work-2", "work", 'c');
        networks.networks.insert(b.id.clone(), b);
        assert_eq!(networks.free_alias("work"), "work-3");
        let long = "a".repeat(63);
        let c = network(&long, &long, 'd');
        networks.networks.insert(c.id.clone(), c);
        let alias = networks.free_alias(&long);
        assert_eq!(alias.len(), 63);
        assert!(alias.ends_with("-2"));
    }

    #[test]
    fn aliases_follow_name_rules_and_never_look_like_an_id() {
        assert!(validate_network_alias("prod-eu").is_ok());
        assert!(validate_network_alias("prod_eu").is_err());
        assert!(validate_network_alias("Prod").is_err());
        assert!(validate_network_alias(&"a".repeat(32)).is_err());
    }

    #[test]
    fn pointers_without_an_alias_load_with_the_name() {
        let pointer: IdentityPointer =
            serde_json::from_value(serde_json::json!({"id": "a".repeat(32), "device": "b"}))
                .unwrap();
        assert!(pointer.alias.is_none());
        let pointer = serde_json::to_value(IdentityPointer {
            id: "a".repeat(32),
            device: "b".repeat(64),
            alias: Some("work".into()),
        })
        .unwrap();
        assert!(pointer.get("name").is_none());
        assert_eq!(pointer["alias"], "work");
    }

    #[test]
    fn identity_paths_reject_traversal() {
        let mut local = network("work", "work", 'a');
        local.id = "../secret".into();
        assert!(local.profile_path().is_err());
    }
}
