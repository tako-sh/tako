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
    /// Stable ID chosen with `tako network use`.
    pub selected: Option<String>,
    /// Why the saved choice could not be read. It is reported only by commands
    /// that rely on it, so `tako network use` can still replace it.
    selected_error: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectedNetwork {
    id: String,
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
    Ok(crate::paths::network_state_dir()
        .map_err(invalid)?
        .join("networks")
        .join("identities.json"))
}
fn selected_path(index: &Path) -> PathBuf {
    index.with_file_name("selected.json")
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
        let (selected, selected_error) = match Self::read_selected(&selected_path(path)) {
            Ok(selected) => (selected, None),
            Err(error) => (None, Some(error.to_string())),
        };
        Ok(Self {
            networks,
            selected,
            selected_error,
        })
    }
    fn read_selected(path: &Path) -> Result<Option<String>> {
        if !path.try_exists().map_err(invalid)? {
            return Ok(None);
        }
        let selected: SelectedNetwork = crate::network::read_json(path).map_err(invalid)?;
        Ok(Some(selected.id))
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
        if let Some(error) = &self.selected_error {
            return Err(invalid(format!(
                "Could not read the network chosen with `tako network use` ({error}). Run `tako network use <network>` to choose again or name the network in the command."
            )));
        }
        if let Some(id) = &self.selected {
            return self.networks.get(id).ok_or_else(|| {
                invalid(
                    "The network chosen with `tako network use` is no longer configured on this machine. Run `tako network use <network>` or name the network in the command.",
                )
            });
        }
        match self.networks.len() {
            0 => Err(invalid(
                "No network configured. Run `tako network create` or `tako network join`.",
            )),
            1 => Ok(self.networks.values().next().unwrap()),
            _ => Err(invalid(
                "More than one network is configured. Run `tako network use <network>` or name the network in the command.",
            )),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_choice(
        networks: impl IntoIterator<Item = LocalNetwork>,
        selected: Option<&str>,
    ) -> Self {
        Self {
            networks: networks
                .into_iter()
                .map(|network| (network.id.clone(), network))
                .collect(),
            selected: selected.map(str::to_string),
            selected_error: None,
        }
    }

    /// Whether a choice from `tako network use` exists, even an unreadable one.
    pub fn has_choice(&self) -> bool {
        self.selected.is_some() || self.selected_error.is_some()
    }

    /// Make a network the default for commands that do not name one.
    pub fn use_network(name_or_id: &str) -> Result<LocalNetwork> {
        let index = index_path()?;
        let local = Self::load_from_file(&index)?
            .select(Some(name_or_id))?
            .clone();
        let path = selected_path(&index);
        crate::network::private_directory(path.parent().unwrap()).map_err(invalid)?;
        crate::network::write_json(
            &path,
            &SelectedNetwork {
                id: local.id.clone(),
            },
            true,
        )
        .map_err(invalid)?;
        Ok(local)
    }

    /// Keep inventory selectors unique even when shared network names collide.
    pub(crate) fn server_namespace(&self, local: &LocalNetwork) -> String {
        if self
            .networks
            .values()
            .any(|other| other.name == local.name && other.id != local.id)
        {
            format!("{}/{}", local.name, local.id)
        } else {
            local.name.clone()
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
        let mut networks = LocalNetworks {
            networks: BTreeMap::from([(a.id.clone(), a.clone()), (b.id.clone(), b.clone())]),
            ..Default::default()
        };
        assert!(networks.select(Some("work")).is_err());
        assert_eq!(networks.select(Some(&a.id)).unwrap(), &a);
        assert!(networks.select(None).is_err());
        networks.selected = Some(b.id.clone());
        assert_eq!(networks.select(None).unwrap(), &b);
        // An explicit network always overrides the chosen default.
        assert_eq!(networks.select(Some(&a.id)).unwrap(), &a);
        networks.networks.remove(&b.id);
        // A stale choice fails instead of silently selecting the sole network.
        assert!(networks.select(None).is_err());
        networks.selected = None;
        assert_eq!(networks.select(None).unwrap(), &a);
        let pointer = serde_json::to_value(IdentityPointer {
            id: a.id,
            device: a.device,
        })
        .unwrap();
        assert!(pointer.get("name").is_none());
    }
    #[test]
    fn unreadable_choice_fails_only_when_a_command_relies_on_it() {
        let directory = tempfile::TempDir::new().unwrap();
        let index = directory.path().join("identities.json");
        std::fs::write(selected_path(&index), b"not json").unwrap();
        let networks = LocalNetworks::load_from_file(&index).unwrap();
        assert!(networks.has_choice());
        let error = networks.select(None).unwrap_err().to_string();
        assert!(error.contains("tako network use"), "{error}");
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
