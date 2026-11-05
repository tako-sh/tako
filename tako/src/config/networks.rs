use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{ConfigError, Result, ServersToml};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NetworkAlias {
    pub name: String,
    pub id: String,
    pub device: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetworksToml {
    pub networks: BTreeMap<String, NetworkAlias>,
}

impl NetworkAlias {
    pub fn validate(&self) -> Result<()> {
        super::servers_toml::validate_server_name(&self.name)?;
        validate_network_id(&self.id)?;
        if !valid_hex_id(&self.device, 64) {
            return Err(ConfigError::Validation("Invalid network device ID".into()));
        }
        Ok(())
    }

    pub fn profile_path(&self) -> Result<PathBuf> {
        self.validate()?;
        let directory = crate::paths::tako_config_dir()
            .map_err(|error| ConfigError::Validation(error.to_string()))?;
        Ok(directory
            .join("networks")
            .join(&self.id)
            .join("profile.json"))
    }
}

pub fn validate_network_id(id: &str) -> Result<()> {
    if valid_hex_id(id, 32) {
        Ok(())
    } else {
        Err(ConfigError::Validation(
            "Network must be its 32-character ID. Find it with `tako network list`.".into(),
        ))
    }
}

fn valid_hex_id(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl NetworksToml {
    pub fn load() -> Result<Self> {
        Self::load_from_file(&ServersToml::default_path()?)
    }

    pub fn load_from_file(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(contents) => Self::parse(&contents),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(ConfigError::FileRead(path.into(), error)),
        }
    }

    pub fn parse(contents: &str) -> Result<Self> {
        #[derive(Deserialize)]
        struct Document {
            #[serde(default)]
            networks: Vec<NetworkAlias>,
        }
        let document: Document = toml::from_str(contents)?;
        let mut networks = BTreeMap::new();
        for network in document.networks {
            network.validate()?;
            if networks
                .values()
                .any(|entry: &NetworkAlias| entry.id == network.id)
            {
                return Err(ConfigError::Validation(format!(
                    "Network '{}' has more than one alias",
                    network.id
                )));
            }
            if networks.insert(network.name.clone(), network).is_some() {
                return Err(ConfigError::Validation("Duplicate network alias".into()));
            }
        }
        Ok(Self { networks })
    }

    pub fn select(&self, name_or_id: Option<&str>) -> Result<&NetworkAlias> {
        if let Some(value) = name_or_id {
            return self
                .networks
                .get(value)
                .or_else(|| self.networks.values().find(|entry| entry.id == value))
                .ok_or_else(|| {
                    ConfigError::Validation(format!(
                        "Network '{value}' is not configured on this machine"
                    ))
                });
        }
        match self.networks.len() {
            0 => Err(ConfigError::Validation(
                "No network configured. Create or import a network first.".into(),
            )),
            1 => Ok(self.networks.values().next().unwrap()),
            _ => Err(ConfigError::Validation(
                "More than one network is configured. Select one with --network.".into(),
            )),
        }
    }

    pub fn save_alias(network: &NetworkAlias) -> Result<()> {
        Self::save_alias_to_file(&ServersToml::default_path()?, network)
    }

    pub fn save_alias_to_file(path: &Path, network: &NetworkAlias) -> Result<()> {
        network.validate()?;
        super::global::edit(path, |document| {
            let mut aliases = Self::parse(&toml::to_string(document)?)?;
            if let Some(existing) = aliases.networks.get(&network.name)
                && existing.id != network.id
            {
                return Err(ConfigError::Validation(format!(
                    "Alias '{}' already names another network",
                    network.name
                )));
            }
            aliases.networks.retain(|_, entry| entry.id != network.id);
            aliases
                .networks
                .insert(network.name.clone(), network.clone());
            let entries = aliases
                .networks
                .values()
                .map(toml::Value::try_from)
                .collect::<std::result::Result<Vec<_>, _>>()?;
            document.insert("networks".into(), toml::Value::Array(entries));
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn network(name: &str, index: u8) -> NetworkAlias {
        NetworkAlias {
            name: name.into(),
            id: format!("{index:032x}"),
            device: "a".repeat(64),
        }
    }

    #[test]
    fn importing_and_renaming_networks_preserves_servers_and_other_settings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            "theme = 'dark'\n[[servers]]\nname = 'tokyo'\nhost = '10.1.1.1'\n",
        )
        .unwrap();
        NetworksToml::save_alias_to_file(&path, &network("home", 1)).unwrap();
        NetworksToml::save_alias_to_file(&path, &network("work", 2)).unwrap();
        NetworksToml::save_alias_to_file(&path, &network("production", 1)).unwrap();
        let aliases = NetworksToml::load_from_file(&path).unwrap();
        assert_eq!(aliases.networks.len(), 2);
        assert!(aliases.select(Some("home")).is_err());
        assert_eq!(
            aliases.select(Some(&format!("{:032x}", 1))).unwrap().name,
            "production"
        );
        assert!(aliases.select(None).is_err());
        assert_eq!(ServersToml::load_from_file(&path).unwrap().len(), 1);
        let document: toml::Table = toml::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(document["theme"].as_str(), Some("dark"));
    }

    #[test]
    fn import_does_not_replace_another_network_under_the_same_alias() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        NetworksToml::save_alias_to_file(&path, &network("home", 1)).unwrap();
        let before = fs::read(&path).unwrap();
        assert!(NetworksToml::save_alias_to_file(&path, &network("home", 2)).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(
            NetworksToml::load_from_file(&path)
                .unwrap()
                .select(None)
                .unwrap()
                .id,
            network("home", 1).id
        );
    }

    #[test]
    fn profile_paths_reject_untrusted_identifiers() {
        for id in [
            "../secrets",
            "",
            "network-name",
            "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF",
        ] {
            let mut alias = network("home", 1);
            alias.id = id.into();
            assert!(alias.profile_path().is_err());
        }
        assert!(NetworksToml::parse("networks = 'invalid'").is_err());
    }
}
