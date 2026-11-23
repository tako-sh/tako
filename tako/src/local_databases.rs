//! Local databases and KV stores for `tako dev` and `tako run`.
//!
//! Each database a project binds gets a SQLite file under
//! `.tako/data/databases/<name>.sqlite`, and each KV store a redb file under
//! `.tako/data/kv/<name>.redb`, served over the same protocols as on servers
//! (KV through the internal socket, like the cache), so app code is identical
//! in development and production.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tako_core::{DatabaseConnection, DatabaseEngine, KvConnection};
use tako_database::{Binding, DatabaseHost, DatabaseId, Home, HostConfig};

use crate::config::TakoToml;

/// Binding name → database name for `tako dev` and `tako run`. Uses
/// `[envs.development].databases` when set, and otherwise every binding of
/// every environment, so a project works locally without extra config.
pub(crate) fn development_bindings(config: &TakoToml) -> BTreeMap<String, String> {
    development_bindings_of(config, DatabaseEngine::Sqlite)
}

/// Binding name → KV store name for `tako dev` and `tako run`, chosen like
/// [`development_bindings`] from `[envs.<env>].kv`.
pub(crate) fn development_kv_bindings(config: &TakoToml) -> BTreeMap<String, String> {
    development_bindings_of(config, DatabaseEngine::Kv)
}

fn development_bindings_of(config: &TakoToml, engine: DatabaseEngine) -> BTreeMap<String, String> {
    use crate::commands::sqlite::env_bindings;
    if let Some(env_config) = config.envs.get("development")
        && !env_bindings(env_config, engine).is_empty()
    {
        return env_bindings(env_config, engine)
            .iter()
            .map(|(binding, database)| (binding.clone(), database.clone()))
            .collect();
    }
    let mut envs: Vec<_> = config.envs.iter().collect();
    envs.sort_by(|a, b| a.0.cmp(b.0));
    let mut bindings = BTreeMap::new();
    for (_, env_config) in envs {
        for (binding, database) in env_bindings(env_config, engine) {
            bindings
                .entry(binding.clone())
                .or_insert_with(|| database.clone());
        }
    }
    bindings
}

/// Custom local environments bind only their own databases, never a fallback.
pub(crate) fn environment_bindings(
    config: &TakoToml,
    environment: &str,
) -> BTreeMap<String, String> {
    environment_bindings_of(config, environment, DatabaseEngine::Sqlite)
}

fn environment_bindings_of(
    config: &TakoToml,
    environment: &str,
    engine: DatabaseEngine,
) -> BTreeMap<String, String> {
    if environment == "development" {
        return development_bindings_of(config, engine);
    }
    config
        .envs
        .get(environment)
        .map(|env| {
            crate::commands::sqlite::env_bindings(env, engine)
                .clone()
                .into_iter()
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn data_root(project_dir: &Path, environment: &str) -> PathBuf {
    let root = project_dir.join(".tako").join("data");
    if environment == "development" {
        root
    } else if environment.len() > 255 {
        // '~' cannot occur in an environment name, so the hashed and literal namespaces are disjoint.
        root.join("envs").join(format!(
            "~{}",
            hex::encode(Sha256::digest(environment.as_bytes()))
        ))
    } else {
        root.join("envs").join(environment)
    }
}

/// Binding name → absolute path of its local file, for the dev daemon.
pub(crate) fn dev_files(
    project_dir: &Path,
    config: &TakoToml,
    environment: &str,
) -> BTreeMap<String, String> {
    let project_dir = project_dir
        .canonicalize()
        .unwrap_or_else(|_| project_dir.to_path_buf());
    environment_bindings(config, environment)
        .into_iter()
        .map(|(binding, database)| {
            (
                binding,
                environment_database_path(&project_dir, &database, environment)
                    .display()
                    .to_string(),
            )
        })
        .collect()
}

/// Binding name → absolute path of its local KV store file, for the dev
/// daemon.
pub(crate) fn dev_kv_files(
    project_dir: &Path,
    config: &TakoToml,
    environment: &str,
) -> BTreeMap<String, String> {
    let project_dir = project_dir
        .canonicalize()
        .unwrap_or_else(|_| project_dir.to_path_buf());
    environment_bindings_of(config, environment, DatabaseEngine::Kv)
        .into_iter()
        .map(|(binding, store)| {
            (
                binding,
                environment_kv_path(&project_dir, &store, environment)
                    .display()
                    .to_string(),
            )
        })
        .collect()
}

pub(crate) fn kv_path(project_dir: &Path, store: &str) -> PathBuf {
    environment_kv_path(project_dir, store, "development")
}

fn environment_kv_path(project_dir: &Path, store: &str, environment: &str) -> PathBuf {
    data_root(project_dir, environment)
        .join("kv")
        .join(format!("{store}.redb"))
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

pub(crate) fn migrate_environment(
    project_dir: &Path,
    config: &TakoToml,
    environment: &str,
) -> Result<DevMigrations, String> {
    environment_migrations(project_dir, config, environment, false, None)
}

/// Applies the pending migrations of each local database, or only of
/// `database` when given, or lists them when `dry_run` is set.
pub(crate) fn development_migrations(
    project_dir: &Path,
    config: &TakoToml,
    dry_run: bool,
    database: Option<&str>,
) -> Result<DevMigrations, String> {
    environment_migrations(project_dir, config, "development", dry_run, database)
}

fn environment_migrations(
    project_dir: &Path,
    config: &TakoToml,
    environment: &str,
    dry_run: bool,
    database: Option<&str>,
) -> Result<DevMigrations, String> {
    let project_dir = project_dir
        .canonicalize()
        .unwrap_or_else(|_| project_dir.to_path_buf());
    let owner = config.name.as_deref().unwrap_or("app");
    let mut result = DevMigrations::default();
    let bindings = environment_bindings(config, environment);
    let selected = crate::database_migrations::by_database(
        &project_dir,
        config,
        bindings
            .iter()
            .filter(|(_, name)| database.is_none_or(|only| only == name.as_str()))
            .map(|(binding, name)| (binding.as_str(), name.as_str())),
    )?;
    for (database, selected) in selected {
        let (binding, migrations) = (selected.binding, selected.migrations);
        let path = environment_database_path(&project_dir, &database, environment);
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

pub(crate) fn database_path(project_dir: &Path, database: &str) -> PathBuf {
    environment_database_path(project_dir, database, "development")
}

fn environment_database_path(project_dir: &Path, database: &str, environment: &str) -> PathBuf {
    data_root(project_dir, environment)
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

/// Serves a project's local databases and KV stores until dropped.
pub(crate) struct LocalDatabases {
    runtime: Option<tokio::runtime::Runtime>,
    connections: HashMap<String, DatabaseConnection>,
    kv_connections: HashMap<String, KvConnection>,
    /// Serves the KV bindings through the run's internal socket.
    kv: Option<tako_kv::KvHost>,
    cache_socket: Option<(tako_workflows::EnqueueSocketHandle, tempfile::TempDir)>,
}

impl LocalDatabases {
    /// `tako run` need not start the dev daemon. Serve the same cache file
    /// and protocol, and the local KV stores, on a private, app-pinned socket
    /// for the child's lifetime.
    pub(crate) fn serve_run_cache(
        &mut self,
        project: &Path,
        environment: &str,
        env: &mut HashMap<String, String>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        use std::sync::Arc;
        use tako_core::instance_env::{TAKO_APP_NAME_ENV, TAKO_INTERNAL_SOCKET_ENV};
        use tako_workflows::enqueue_socket::{SocketOptions, pinned_app_auth, spawn_with_options};
        let dir = data_root(project, environment).join("tako");
        std::fs::create_dir_all(&dir)?;
        let tmp = tempfile::tempdir()?;
        let socket = tmp.path().join("c.sock");
        if self.runtime.is_none() {
            self.runtime = Some(
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_all()
                    .build()?,
            );
        }
        let _guard = self.runtime.as_ref().expect("initialized runtime").enter();
        let caches = tako_cache::Caches::default();
        let host = crate::paths::tako_data_dir()?;
        let environment = environment.to_owned();
        let app = format!("local/{environment}");
        let kv: Option<tako_workflows::KvFn> = self.kv.clone().map(|host| {
            let kv: tako_workflows::KvFn = Arc::new(move |app, token, request| {
                match host.call(app, token, request) {
                    Ok(value) => tako_core::Response::ok(value),
                    Err(error) => error.into_response(),
                }
            });
            kv
        });
        let handle = spawn_with_options(
            &socket,
            Arc::new(|_| None),
            None,
            Some(pinned_app_auth(&app)),
            None,
            SocketOptions {
                mode: Some(0o600),
                cache: Some(Arc::new(move |_, shared, req| {
                    if shared {
                        caches.reply_shared(&host, &environment, req)
                    } else {
                        caches.reply(&dir, req)
                    }
                })),
                kv,
            },
        )?;
        env.insert(
            TAKO_INTERNAL_SOCKET_ENV.into(),
            socket.display().to_string(),
        );
        env.insert(TAKO_APP_NAME_ENV.into(), app);
        self.cache_socket = Some((handle, tmp));
        Ok(())
    }

    pub(crate) fn connections(&self) -> &HashMap<String, DatabaseConnection> {
        &self.connections
    }

    pub(crate) fn kv_connections(&self) -> &HashMap<String, KvConnection> {
        &self.kv_connections
    }

    /// Starts serving `bindings` (binding → database name) on a loopback
    /// port in a background runtime, and opens `kv` (binding → KV store
    /// name) for [`Self::serve_run_cache`] to serve. Returns an empty set
    /// without bindings.
    pub(crate) fn start(
        project_dir: &Path,
        bindings: &BTreeMap<String, String>,
        kv: &BTreeMap<String, String>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        if bindings.is_empty() && kv.is_empty() {
            return Ok(Self {
                runtime: None,
                cache_socket: None,
                connections: HashMap::new(),
                kv_connections: HashMap::new(),
                kv: None,
            });
        }
        let (kv, kv_connections) = open_kv(project_dir, kv)?;
        if bindings.is_empty() {
            return Ok(Self {
                runtime: None,
                cache_socket: None,
                connections: HashMap::new(),
                kv_connections,
                kv,
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
            cache_socket: None,
            connections,
            kv_connections,
            kv,
        })
    }
}

/// The app scope local KV bindings belong to: the run's pinned app.
const LOCAL_KV_SCOPE: &str = "local/development";

/// Opens local KV store files and binds them for the run's app.
fn open_kv(
    project_dir: &Path,
    kv: &BTreeMap<String, String>,
) -> Result<(Option<tako_kv::KvHost>, HashMap<String, KvConnection>), Box<dyn std::error::Error>>
{
    if kv.is_empty() {
        return Ok((None, HashMap::new()));
    }
    let host = tako_kv::KvHost::new(tako_kv::HostConfig::default());
    let mut registered = BTreeMap::new();
    let mut bindings = Vec::new();
    let mut connections = HashMap::new();
    for (binding, store) in kv {
        let path = kv_path(project_dir, store);
        let id = match registered.get(&path) {
            Some(id) => tako_kv::StoreId::clone(id),
            None => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let digest = Sha256::digest(path.to_string_lossy().as_bytes());
                let id = tako_kv::StoreId::parse(&format!("local-{}", hex::encode(&digest[..16])))?;
                host.register(id.clone(), path.clone())?;
                registered.insert(path.clone(), id.clone());
                id
            }
        };
        let token = tako_kv::generate_token();
        bindings.push(tako_kv::Binding {
            token_hash: tako_kv::hash_token(&token),
            store: id,
            home: tako_kv::Home::Local,
        });
        connections.insert(binding.clone(), KvConnection::new(&token));
    }
    host.set_bindings(LOCAL_KV_SCOPE, bindings);
    Ok((Some(host), connections))
}

impl Drop for LocalDatabases {
    fn drop(&mut self) {
        // Stop serving before closing the stores.
        self.cache_socket = None;
        if let Some(runtime) = self.runtime.take() {
            // Safe to call from async code, unlike a blocking shutdown.
            runtime.shutdown_background();
        }
        if let Some(kv) = self.kv.take() {
            kv.close_all();
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
    fn custom_environment_migrates_isolated_files_without_fallback_bindings() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().canonicalize().unwrap();
        let cfg = config(
            r#"
name = "shop"
[envs.development]
databases = { main = "shared-name", dev_only = "dev-only" }
[envs.test-e2e]
route = "shop-test-e2e.test"
databases = { main = "shared-name" }
[envs.empty]
route = "empty.test"
"#,
        );
        std::fs::create_dir_all(project.join("migrations/main")).unwrap();
        std::fs::write(
            project.join("migrations/main/001_items.sql"),
            "CREATE TABLE items (value TEXT);",
        )
        .unwrap();
        migrate_environment(&project, &cfg, "development").unwrap();
        let dev_file = &dev_files(&project, &cfg, "development")["main"];
        let dev = tako_sqlite::open_local(dev_file).unwrap();
        dev.execute("INSERT INTO items VALUES ('keep')", [])
            .unwrap();
        migrate_environment(&project, &cfg, "test-e2e").unwrap();
        let test_files = dev_files(&project, &cfg, "test-e2e");
        assert_eq!(test_files.len(), 1);
        assert_ne!(&test_files["main"], dev_file);
        let test = tako_sqlite::open_local(&test_files["main"]).unwrap();
        assert_eq!(
            test.query_row("SELECT count(*) FROM items", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            dev.query_row("SELECT value FROM items", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "keep"
        );
        assert!(dev_files(&project, &cfg, "empty").is_empty());
    }

    #[test]
    fn long_environment_components_are_stable_distinct_and_creatable() {
        let tmp = tempfile::tempdir().unwrap();
        let boundary = "a".repeat(255);
        let literal = data_root(tmp.path(), &boundary);
        assert_eq!(literal.file_name().unwrap(), boundary.as_str());
        let long = format!("{boundary}b");
        let hashed = data_root(tmp.path(), &long);
        assert_eq!(hashed, data_root(tmp.path(), &long));
        assert_ne!(hashed, data_root(tmp.path(), &format!("{boundary}c")));
        assert!(
            hashed
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with('~')
        );
        for path in [literal, hashed] {
            std::fs::create_dir_all(path.join("app")).unwrap();
        }
    }

    #[test]
    fn local_databases_serve_their_files() {
        let dir = tempfile::tempdir().unwrap();
        let bindings = BTreeMap::from([("main".to_string(), "shop".to_string())]);
        let local = LocalDatabases::start(dir.path(), &bindings, &BTreeMap::new()).unwrap();
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

    #[test]
    fn local_kv_stores_serve_their_files() {
        let dir = tempfile::tempdir().unwrap();
        let kv = BTreeMap::from([("auth".to_string(), "sessions".to_string())]);
        let local = LocalDatabases::start(dir.path(), &BTreeMap::new(), &kv).unwrap();
        assert!(local.connections().is_empty());
        assert!(kv_path(dir.path(), "sessions").exists());
        let token = &local.kv_connections()["auth"].token;
        let host = local.kv.as_ref().unwrap();
        let put = tako_core::KvRequest::Put {
            key: "k".into(),
            json: r#""v""#.into(),
            ttl_ms: None,
        };
        host.call(LOCAL_KV_SCOPE, token, put).unwrap();
        let got = host
            .call(LOCAL_KV_SCOPE, token, tako_core::KvRequest::Get { key: "k".into() })
            .unwrap();
        assert_eq!(got, serde_json::json!(r#""v""#));
    }

    #[test]
    fn kv_bindings_fall_back_like_database_bindings() {
        let config = config(
            "name = \"shop\"\n[envs.production]\nroute = \"shop.example.com\"\nkv = { auth = \"sessions\" }\n[envs.staging]\nroute = \"staging.example.com\"\nkv = { auth = \"sessions_staging\", cache = \"cache\" }\n",
        );
        assert_eq!(
            development_kv_bindings(&config),
            BTreeMap::from([
                ("auth".to_string(), "sessions".to_string()),
                ("cache".to_string(), "cache".to_string()),
            ])
        );
        let dir = tempfile::tempdir().unwrap();
        let files = dev_kv_files(dir.path(), &config, "development");
        assert!(files["auth"].ends_with("/.tako/data/kv/sessions.redb"));
        assert!(development_bindings(&config).is_empty());
    }
}
