use crate::http::Bootstrap;
use std::{collections::HashMap, path::PathBuf};

pub const ENV_ENV: &str = "ENV";
pub const HOST_ENV: &str = "HOST";
pub const PORT_ENV: &str = "PORT";
pub const BUILD_ENV: &str = "TAKO_BUILD";
pub const DATA_DIR_ENV: &str = "TAKO_DATA_DIR";

#[derive(Debug, Clone)]
pub struct Runtime {
    env: String,
    host: String,
    port: u16,
    build: String,
    data_dir: PathBuf,
    app_name: String,
    secrets: HashMap<String, String>,
    storages: serde_json::Value,
}

impl Runtime {
    pub fn from_env(bootstrap: Bootstrap) -> Result<Self, Error> {
        Self::from_env_map(std::env::vars().collect(), bootstrap)
    }

    pub fn from_env_map(env: HashMap<String, String>, bootstrap: Bootstrap) -> Result<Self, Error> {
        let port = env
            .get(PORT_ENV)
            .map(|raw| {
                raw.parse::<u16>()
                    .map_err(|_| Error::InvalidPort(raw.clone()))
            })
            .transpose()?
            .unwrap_or(0);
        Ok(Self {
            env: env.get(ENV_ENV).cloned().unwrap_or_default(),
            host: env.get(HOST_ENV).cloned().unwrap_or_default(),
            port,
            build: env.get(BUILD_ENV).cloned().unwrap_or_default(),
            data_dir: env.get(DATA_DIR_ENV).map(PathBuf::from).unwrap_or_default(),
            app_name: env.get(crate::APP_NAME_ENV).cloned().unwrap_or_default(),
            secrets: bootstrap.secrets,
            storages: bootstrap.storages,
        })
    }

    pub fn env(&self) -> &str {
        &self.env
    }

    pub fn is_dev(&self) -> bool {
        self.env == "development"
    }

    pub fn is_prod(&self) -> bool {
        self.env == "production"
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn build(&self) -> &str {
        &self.build
    }

    pub fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    pub fn app_name(&self) -> &str {
        &self.app_name
    }

    pub fn base_app_name(&self) -> &str {
        self.app_name.split('/').next().unwrap_or(&self.app_name)
    }

    pub fn secret(&self, name: &str) -> Option<&str> {
        self.secrets.get(name).map(String::as_str)
    }

    pub fn secrets(&self) -> Secrets<'_> {
        Secrets(&self.secrets)
    }

    pub fn storages(&self) -> &serde_json::Value {
        &self.storages
    }
}

pub struct Secrets<'a>(&'a HashMap<String, String>);

impl<'a> Secrets<'a> {
    pub fn get(&self, name: &str) -> Option<&'a str> {
        self.0.get(name).map(String::as_str)
    }

    pub fn contains_key(&self, name: &str) -> bool {
        self.0.contains_key(name)
    }
}

impl std::fmt::Debug for Secrets<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl std::fmt::Display for Secrets<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid PORT: {0}")]
    InvalidPort(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn runtime_reads_env_bootstrap_and_base_app_name() {
        let runtime = Runtime::from_env_map(
            HashMap::from([
                ("ENV".to_string(), "production".to_string()),
                ("HOST".to_string(), "127.0.0.1".to_string()),
                ("PORT".to_string(), "4100".to_string()),
                ("TAKO_BUILD".to_string(), "v1".to_string()),
                ("TAKO_DATA_DIR".to_string(), "/data/app".to_string()),
                ("TAKO_APP_NAME".to_string(), "cloud/production".to_string()),
            ]),
            Bootstrap {
                token: "tok".to_string(),
                secrets: HashMap::from([("DATABASE_URL".to_string(), "postgres://db".to_string())]),
                storages: serde_json::json!({"uploads":{"provider":"local"}}),
            },
        )
        .unwrap();

        assert!(runtime.is_prod());
        assert!(!runtime.is_dev());
        assert_eq!(runtime.host(), "127.0.0.1");
        assert_eq!(runtime.port(), 4100);
        assert_eq!(runtime.build(), "v1");
        assert_eq!(runtime.base_app_name(), "cloud");
        assert_eq!(runtime.secret("DATABASE_URL"), Some("postgres://db"));
        assert_eq!(runtime.storages()["uploads"]["provider"], "local");
    }

    #[test]
    fn secrets_debug_is_redacted() {
        let runtime = Runtime::from_env_map(
            HashMap::new(),
            Bootstrap {
                token: String::new(),
                secrets: HashMap::from([("API_KEY".to_string(), "secret".to_string())]),
                storages: serde_json::Value::Null,
            },
        )
        .unwrap();

        assert_eq!(format!("{:?}", runtime.secrets()), "[REDACTED]");
        assert!(runtime.secrets().contains_key("API_KEY"));
    }
}
