//! Local databases for `tako dev` and `tako run`.
//!
//! Each database a project binds gets a SQLite file under
//! `.tako/data/databases/<name>.sqlite`, served over the same protocol as on
//! servers, so app code is identical in development and production.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tako_core::DatabaseConnection;
use tako_database::{Binding, DatabaseHost, DatabaseId, Home, HostConfig};

use crate::config::TakoToml;

/// Binding name → database name for `tako dev` and `tako run`. Uses
/// `[envs.development].databases` when set, and otherwise every binding of
/// every environment, so a project works locally without extra config.
pub(crate) fn development_bindings(config: &TakoToml) -> BTreeMap<String, String> {
    if let Some(env_config) = config.envs.get("development")
        && !env_config.databases.is_empty()
    {
        return env_config
            .databases
            .iter()
            .map(|(binding, database)| (binding.clone(), database.clone()))
            .collect();
    }
    let mut envs: Vec<_> = config.envs.iter().collect();
    envs.sort_by(|a, b| a.0.cmp(b.0));
    let mut bindings = BTreeMap::new();
    for (_, env_config) in envs {
        for (binding, database) in &env_config.databases {
            bindings
                .entry(binding.clone())
                .or_insert_with(|| database.clone());
        }
    }
    bindings
}

/// Binding name → absolute path of its local file, for the dev daemon.
pub(crate) fn dev_files(project_dir: &Path, config: &TakoToml) -> BTreeMap<String, String> {
    let project_dir = project_dir
        .canonicalize()
        .unwrap_or_else(|_| project_dir.to_path_buf());
    development_bindings(config)
        .into_iter()
        .map(|(binding, database)| {
            (
                binding,
                database_path(&project_dir, &database).display().to_string(),
            )
        })
        .collect()
}

/// What applying development migrations did.
#[derive(Debug, Default)]
pub(crate) struct DevMigrations {
    /// One line per binding that applied migrations.
    pub applied: Vec<String>,
    pub warnings: Vec<String>,
}

/// Applies each development binding's pending migrations to its local file.
pub(crate) fn migrate_development(
    project_dir: &Path,
    config: &TakoToml,
) -> Result<DevMigrations, String> {
    let project_dir = project_dir
        .canonicalize()
        .unwrap_or_else(|_| project_dir.to_path_buf());
    let owner = config.name.as_deref().unwrap_or("app");
    let mut result = DevMigrations::default();
    for (binding, database) in development_bindings(config) {
        let migrations = crate::database_migrations::load(&project_dir, config, &binding)?;
        if migrations.is_empty() {
            continue;
        }
        let path = database_path(&project_dir, &database);
        let report = tako_database::migrate::migrate_file(&path, owner, &migrations, false)
            .map_err(|error| format!("Database `{binding}`: {error}"))?;
        result.warnings.extend(
            report
                .warnings
                .into_iter()
                .map(|warning| format!("Database `{binding}`: {warning}")),
        );
        if !report.applied.is_empty() {
            result.applied.push(format!(
                "Applied {} to `{binding}`",
                report.applied.join(", ")
            ));
        }
    }
    Ok(result)
}

pub(crate) fn database_path(project_dir: &Path, database: &str) -> PathBuf {
    project_dir
        .join(".tako")
        .join("data")
        .join("databases")
        .join(format!("{database}.sqlite"))
}

/// A stable local ID for a database file.
fn local_id(path: &Path) -> DatabaseId {
    let digest = Sha256::digest(path.to_string_lossy().as_bytes());
    // A name the file's path determines; it never leaves this process.
    DatabaseId::parse(&format!("local-{}", hex::encode(&digest[..16])))
        .expect("a hex digest makes a valid name")
}

/// Serves a project's local databases until dropped.
pub(crate) struct LocalDatabases {
    runtime: Option<tokio::runtime::Runtime>,
    connections: HashMap<String, DatabaseConnection>,
}

impl LocalDatabases {
    pub(crate) fn connections(&self) -> &HashMap<String, DatabaseConnection> {
        &self.connections
    }

    /// Starts serving `bindings` (binding → database name) on a loopback
    /// port in a background runtime. Returns an empty set without bindings.
    pub(crate) fn start(
        project_dir: &Path,
        bindings: &BTreeMap<String, String>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        if bindings.is_empty() {
            return Ok(Self {
                runtime: None,
                connections: HashMap::new(),
            });
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("tako-local-db")
            .enable_all()
            .build()?;
        let host = DatabaseHost::new(HostConfig::default());
        let mut registered = BTreeMap::new();
        let mut host_bindings = Vec::new();
        let mut connections = HashMap::new();
        // Bind outside the runtime: callers may already run inside one.
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let url = format!("http://{}", listener.local_addr()?);
        for (binding, database) in bindings {
            let path = database_path(project_dir, database);
            let id = match registered.get(&path) {
                Some(id) => DatabaseId::clone(id),
                None => {
                    if !path.exists() {
                        DatabaseHost::create_file(&path)?;
                    }
                    let id = local_id(&path);
                    host.register(id.clone(), path.clone())?;
                    registered.insert(path.clone(), id.clone());
                    id
                }
            };
            let token = tako_database::generate_token();
            host_bindings.push(Binding {
                token_hash: tako_database::hash_token(&token),
                database: id,
                home: Home::Local,
            });
            connections.insert(
                binding.clone(),
                DatabaseConnection {
                    url: url.clone(),
                    auth_token: token,
                },
            );
        }
        host.set_bindings("local/development", host_bindings);
        let serve_host = host.clone();
        runtime.spawn(async move {
            match tokio::net::TcpListener::from_std(listener) {
                Ok(listener) => {
                    tako_database::http::serve(
                        serve_host,
                        listener,
                        tako_database::http::Access::Apps,
                    )
                    .await;
                }
                Err(error) => tracing::warn!("Local database listener failed: {error}"),
            }
        });
        let reaper_host = host.clone();
        runtime.spawn(async move {
            let _ = reaper_host.spawn_reaper().await;
        });
        Ok(Self {
            runtime: Some(runtime),
            connections,
        })
    }
}

impl Drop for LocalDatabases {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            // Safe to call from async code, unlike a blocking shutdown.
            runtime.shutdown_background();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> TakoToml {
        TakoToml::parse(text).unwrap()
    }

    const PROJECT: &str = r#"
name = "shop"

[envs.production]
route = "shop.example.com"
databases = { main = "shop-db" }

[envs.staging]
route = "staging.shop.example.com"
databases = { main = "shop-staging", events = "events" }
"#;

    #[test]
    fn development_uses_every_binding_when_it_declares_none() {
        let bindings = development_bindings(&config(PROJECT));
        assert_eq!(
            bindings["main"], "shop-db",
            "the first environment by name wins"
        );
        assert_eq!(bindings["events"], "events");
        let explicit =
            format!("{PROJECT}\n[envs.development]\ndatabases = {{ main = \"local\" }}\n");
        assert_eq!(
            development_bindings(&config(&explicit)),
            BTreeMap::from([("main".to_string(), "local".to_string())])
        );
    }

    #[test]
    fn local_databases_serve_their_files() {
        let dir = tempfile::tempdir().unwrap();
        let bindings = BTreeMap::from([("main".to_string(), "shop".to_string())]);
        let local = LocalDatabases::start(dir.path(), &bindings).unwrap();
        let connection = &local.connections()["main"];
        assert!(connection.url.starts_with("http://127.0.0.1:"));
        assert!(database_path(dir.path(), "shop").exists());
        let body = serde_json::json!({"baton": null, "requests": [
            {"type": "execute", "stmt": {"sql": "SELECT 41 + 1"}}
        ]});
        let value: serde_json::Value = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let response = reqwest::Client::new()
                    .post(format!("{}/v2/pipeline", connection.url))
                    .bearer_auth(&connection.auth_token)
                    .json(&body)
                    .send()
                    .await
                    .unwrap();
                assert!(response.status().is_success());
                response.json().await.unwrap()
            });
        assert_eq!(
            value["results"][0]["response"]["result"]["rows"][0][0]["value"],
            "42"
        );
    }
}
