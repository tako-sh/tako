//! Local databases for `tako dev` and `tako run`.
//!
//! A database belongs to a network, not to a project, so its local copy does
//! too: each lives in Tako's data directory as
//! `databases/<network>/<name>.sqlite`, shared by every project of the
//! network that binds the name. A project whose environments name no network
//! uses the `local` namespace. The files are served over the same protocol
//! as on servers, so app code is identical in development and production.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tako_core::DatabaseConnection;
use tako_database::{Binding, DatabaseHost, DatabaseId, Home, HostConfig};

use crate::config::TakoToml;

/// The namespace of local databases that have no network.
pub(crate) const LOCAL_NAMESPACE: &str = "local";

/// A local database: its name and the namespace (network) it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalDatabase {
    pub name: String,
    pub namespace: String,
}

/// The namespace of a project's local databases: the network `requested`,
/// or else the development environment's, or else the one network the
/// project's other environments use, or `local`.
pub(crate) fn namespace(config: &TakoToml, requested: Option<&str>) -> String {
    if let Some(requested) = requested {
        return requested.to_owned();
    }
    if let Some(network) = config
        .envs
        .get("development")
        .and_then(|env| env.network.as_deref())
    {
        return network.to_owned();
    }
    let networks: std::collections::BTreeSet<&str> = config
        .envs
        .iter()
        .filter(|(name, _)| name.as_str() != "development")
        .filter_map(|(_, env)| env.network.as_deref())
        .collect();
    match networks.len() {
        1 => networks.into_iter().next().expect("one network").to_owned(),
        _ => LOCAL_NAMESPACE.to_owned(),
    }
}

/// Binding name → local database for `tako dev` and `tako run`. Uses
/// `[envs.development].databases` when set, and otherwise every binding of
/// every environment, so a project works locally without extra config; a
/// binding taken from an environment belongs to that environment's network.
pub(crate) fn development_bindings(config: &TakoToml) -> BTreeMap<String, LocalDatabase> {
    let project = namespace(config, None);
    if let Some(env_config) = config.envs.get("development")
        && !env_config.databases.is_empty()
    {
        return env_config
            .databases
            .iter()
            .map(|(binding, database)| {
                (
                    binding.clone(),
                    LocalDatabase {
                        name: database.clone(),
                        namespace: project.clone(),
                    },
                )
            })
            .collect();
    }
    let mut envs: Vec<_> = config.envs.iter().collect();
    envs.sort_by(|a, b| a.0.cmp(b.0));
    let mut bindings = BTreeMap::new();
    for (_, env_config) in envs {
        for (binding, database) in &env_config.databases {
            bindings
                .entry(binding.clone())
                .or_insert_with(|| LocalDatabase {
                    name: database.clone(),
                    namespace: env_config
                        .network
                        .clone()
                        .unwrap_or_else(|| project.clone()),
                });
        }
    }
    bindings
}

/// Binding name → absolute path of its local file, for the dev daemon.
pub(crate) fn dev_files(project_dir: &Path, config: &TakoToml) -> BTreeMap<String, String> {
    development_bindings(config)
        .into_iter()
        .map(|(binding, database)| {
            (
                binding,
                database_path(project_dir, &database).display().to_string(),
            )
        })
        .collect()
}

/// What applying development migrations did.
#[derive(Debug, Default)]
pub(crate) struct DevMigrations {
    /// One line per binding that applied migrations.
    pub applied: Vec<String>,
    /// One line per binding with pending migrations, for a dry run.
    pub pending: Vec<String>,
    pub warnings: Vec<String>,
}

/// Applies each development binding's pending migrations to its local file.
pub(crate) fn migrate_development(
    project_dir: &Path,
    config: &TakoToml,
) -> Result<DevMigrations, String> {
    development_migrations(project_dir, config, false, None)
}

/// Applies the pending migrations of each local database, or only of
/// `database` when given, or lists them when `dry_run` is set.
pub(crate) fn development_migrations(
    project_dir: &Path,
    config: &TakoToml,
    dry_run: bool,
    database: Option<&str>,
) -> Result<DevMigrations, String> {
    let project_dir = project_dir
        .canonicalize()
        .unwrap_or_else(|_| project_dir.to_path_buf());
    let owner = config.name.as_deref().unwrap_or("app");
    let mut result = DevMigrations::default();
    let bindings = development_bindings(config);
    let selected = crate::database_migrations::by_database(
        &project_dir,
        config,
        bindings
            .iter()
            .filter(|(_, local)| database.is_none_or(|only| only == local.name.as_str()))
            .map(|(binding, local)| (binding.as_str(), local.name.as_str())),
    )?;
    for (database, selected) in selected {
        let (binding, migrations) = (selected.binding, selected.migrations);
        let local = bindings
            .get(&binding)
            .cloned()
            .unwrap_or_else(|| LocalDatabase {
                name: database.clone(),
                namespace: namespace(config, None),
            });
        let path = database_path(&project_dir, &local);
        let report = tako_database::migrate::migrate_file(&path, owner, &migrations, dry_run)
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
        if !report.pending.is_empty() {
            result.pending.push(format!(
                "`{binding}` has {} pending: {}",
                report.pending.len(),
                report.pending.join(", ")
            ));
        }
    }
    Ok(result)
}

/// Where local databases live: `<Tako data directory>/databases`.
pub(crate) fn root() -> std::io::Result<PathBuf> {
    Ok(crate::paths::tako_data_dir()?.join("databases"))
}

/// The file of a local database. Without a Tako data directory (no home
/// directory), it stays in the project.
pub(crate) fn database_path(project_dir: &Path, database: &LocalDatabase) -> PathBuf {
    let file = format!("{}.sqlite", database.name);
    match root() {
        Ok(root) => root.join(&database.namespace).join(file),
        Err(_) => project_dir
            .join(".tako")
            .join("data")
            .join("databases")
            .join(file),
    }
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

    /// Starts serving `bindings` (binding → local database) on a loopback
    /// port in a background runtime. Returns an empty set without bindings.
    pub(crate) fn start(
        project_dir: &Path,
        bindings: &BTreeMap<String, LocalDatabase>,
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

    /// Points Tako's data directory at a temporary one for a test.
    struct TestHome {
        directory: tempfile::TempDir,
        previous: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl TestHome {
        fn new() -> Self {
            let lock = crate::paths::test_tako_home_env_lock();
            let directory = tempfile::tempdir().unwrap();
            let previous = std::env::var_os("TAKO_HOME");
            unsafe { std::env::set_var("TAKO_HOME", directory.path()) };
            Self {
                directory,
                previous,
                _lock: lock,
            }
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            match &self.previous {
                Some(previous) => unsafe { std::env::set_var("TAKO_HOME", previous) },
                None => unsafe { std::env::remove_var("TAKO_HOME") },
            }
        }
    }

    const NETWORK: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn development_uses_every_binding_when_it_declares_none() {
        let bindings = development_bindings(&config(PROJECT));
        assert_eq!(
            bindings["main"].name, "shop-db",
            "the first environment by name wins"
        );
        assert_eq!(bindings["events"].name, "events");
        let explicit =
            format!("{PROJECT}\n[envs.development]\ndatabases = {{ main = \"local\" }}\n");
        let bindings = development_bindings(&config(&explicit));
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings["main"].name, "local");
    }

    #[test]
    fn local_databases_belong_to_the_projects_network() {
        // Without a network, the `local` namespace.
        assert_eq!(namespace(&config(PROJECT), None), LOCAL_NAMESPACE);
        assert_eq!(
            development_bindings(&config(PROJECT))["main"].namespace,
            LOCAL_NAMESPACE
        );
        // With one, its databases' local copies are the network's.
        let networked = PROJECT.replace(
            "[envs.production]\n",
            &format!("[envs.production]\nnetwork = \"{NETWORK}\"\n"),
        );
        let config = config(&networked);
        assert_eq!(namespace(&config, None), NETWORK);
        assert_eq!(development_bindings(&config)["events"].namespace, NETWORK);
        // So two projects of the network share one file per database name.
        let home = TestHome::new();
        let local = &development_bindings(&config)["main"];
        assert_eq!(
            database_path(Path::new("/elsewhere"), local),
            home.directory
                .path()
                .join("databases")
                .join(NETWORK)
                .join("shop-db.sqlite")
        );
    }

    #[test]
    fn local_databases_serve_their_files() {
        let home = TestHome::new();
        let dir = tempfile::tempdir().unwrap();
        let local = LocalDatabase {
            name: "shop".into(),
            namespace: LOCAL_NAMESPACE.into(),
        };
        let bindings = BTreeMap::from([("main".to_string(), local.clone())]);
        let served = LocalDatabases::start(dir.path(), &bindings).unwrap();
        let connection = &served.connections()["main"];
        assert!(connection.url.starts_with("http://127.0.0.1:"));
        let path = database_path(dir.path(), &local);
        assert!(path.starts_with(home.directory.path()));
        assert!(path.exists());
        // While it is served, it cannot be deleted.
        assert!(tako_database::lock_unused(&path).unwrap().is_none());
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
        // The runtime stops in the background, and the file with it.
        drop(served);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let lock = loop {
            if let Some(lock) = tako_database::lock_unused(&path).unwrap() {
                break lock;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the file stays in use"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        tako_database::remove_database_files(&path, lock).unwrap();
        assert!(!path.exists());
    }
}
