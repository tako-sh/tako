//! Local databases for apps registered with the dev daemon.
//!
//! The daemon serves each project's SQLite files over the same protocol as
//! `tako-server`, on one loopback port shared by every registered app. Each
//! registration (keyed by its config path) gets its own tokens, which stay
//! stable across re-registration while a binding names the same file, so
//! running workflow workers keep working after a reload.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tako_core::DatabaseConnection;
use tako_database::{Binding, DatabaseHost, DatabaseId, Home, HostConfig};
use tokio::sync::{Mutex, OnceCell};

struct Server {
    host: DatabaseHost,
    url: String,
    registrations: Mutex<Registrations>,
}

#[derive(Default)]
struct Registrations {
    /// Served files and their IDs.
    files: HashMap<PathBuf, DatabaseId>,
    /// Config path → binding → (file, token).
    scopes: HashMap<String, BTreeMap<String, (PathBuf, String)>>,
}

static SERVER: OnceCell<Server> = OnceCell::const_new();

async fn server() -> Result<&'static Server, String> {
    SERVER
        .get_or_try_init(|| async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .map_err(|e| format!("bind local database port: {e}"))?;
            let url = format!(
                "http://{}",
                listener.local_addr().map_err(|e| e.to_string())?
            );
            let host = DatabaseHost::new(HostConfig::default());
            let serve_host = host.clone();
            tokio::spawn(async move {
                tako_database::http::serve(serve_host, listener, tako_database::http::Access::Apps)
                    .await;
            });
            let _reaper = host.spawn_reaper();
            tracing::info!(url = %url, "Serving local databases");
            Ok(Server {
                host,
                url,
                registrations: Mutex::new(Registrations::default()),
            })
        })
        .await
}

/// A stable ID for a database file, so logs name the same database across
/// daemon restarts.
fn local_id(path: &Path) -> DatabaseId {
    let digest = Sha256::digest(path.to_string_lossy().as_bytes());
    // A name the file's path determines; it never leaves this process.
    DatabaseId::parse(&format!("local-{}", hex::encode(&digest[..16])))
        .expect("a hex digest makes a valid name")
}

/// Serves `bindings` (binding name → SQLite file) for the app registered at
/// `config_path`, replacing its previous bindings, and returns the
/// connections its processes receive.
pub(crate) async fn connect(
    config_path: &str,
    bindings: &HashMap<String, String>,
) -> Result<HashMap<String, DatabaseConnection>, String> {
    if bindings.is_empty() {
        release(config_path).await;
        return Ok(HashMap::new());
    }
    for (binding, file) in bindings {
        if !Path::new(file).is_absolute() {
            return Err(format!(
                "database binding {binding} must name an absolute path, got {file}"
            ));
        }
    }
    let server = server().await?;
    let mut registrations = server.registrations.lock().await;
    let previous = registrations
        .scopes
        .get(config_path)
        .cloned()
        .unwrap_or_default();

    let mut scope = BTreeMap::new();
    let mut host_bindings = Vec::new();
    for (binding, file) in bindings {
        let path = PathBuf::from(file);
        let id = match registrations.files.get(&path) {
            Some(id) => id.clone(),
            None => {
                let id = local_id(&path);
                let host = server.host.clone();
                let register_path = path.clone();
                let register_id = id.clone();
                // Creating and locking the file blocks.
                tokio::task::spawn_blocking(move || {
                    if !register_path.exists() {
                        DatabaseHost::create_file(&register_path)?;
                    }
                    host.register(register_id, register_path)
                })
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| format!("open {}: {e}", path.display()))?;
                registrations.files.insert(path.clone(), id.clone());
                id
            }
        };
        let token = match previous.get(binding) {
            Some((previous_path, token)) if *previous_path == path => token.clone(),
            _ => tako_database::generate_token(),
        };
        host_bindings.push(Binding {
            token_hash: tako_database::hash_token(&token),
            database: id,
            home: Home::Local,
        });
        scope.insert(binding.clone(), (path, token));
    }
    server.host.set_bindings(config_path, host_bindings);
    let connections = scope
        .iter()
        .map(|(binding, (_, token))| {
            (
                binding.clone(),
                DatabaseConnection {
                    url: server.url.clone(),
                    auth_token: token.clone(),
                },
            )
        })
        .collect();
    registrations.scopes.insert(config_path.to_owned(), scope);
    close_unused(server, &mut registrations).await;
    Ok(connections)
}

/// Revokes the bindings of the app registered at `config_path` and closes
/// files no other app uses.
pub(crate) async fn release(config_path: &str) {
    let Some(server) = SERVER.get() else {
        return;
    };
    let mut registrations = server.registrations.lock().await;
    if registrations.scopes.remove(config_path).is_none() {
        return;
    }
    server.host.set_bindings(config_path, Vec::new());
    close_unused(server, &mut registrations).await;
}

async fn close_unused(server: &Server, registrations: &mut Registrations) {
    let used: std::collections::HashSet<&PathBuf> = registrations
        .scopes
        .values()
        .flat_map(|scope| scope.values().map(|(path, _)| path))
        .collect();
    let unused: Vec<(PathBuf, DatabaseId)> = registrations
        .files
        .iter()
        .filter(|(path, _)| !used.contains(path))
        .map(|(path, id)| (path.clone(), id.clone()))
        .collect();
    for (path, id) in unused {
        match server.host.unregister(&id).await {
            Ok(()) => {
                registrations.files.remove(&path);
            }
            // Still registered: keep the mapping, so binding the file again
            // reuses it and a later release retries the close.
            Err(error) => {
                tracing::warn!(path = %path.display(), "Failed to close local database: {error}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn query(connection: &DatabaseConnection, sql: &str) -> reqwest::Response {
        let body = serde_json::json!({"baton": null, "requests": [
            {"type": "execute", "stmt": {"sql": sql}}
        ]});
        reqwest::Client::new()
            .post(format!("{}/v2/pipeline", connection.url))
            .bearer_auth(&connection.auth_token)
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn registrations_share_files_and_keep_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("shop.sqlite").display().to_string();
        let bindings = HashMap::from([("db".to_string(), file.clone())]);

        let first = connect("/a/tako.toml", &bindings).await.unwrap();
        let other = connect("/b/tako.toml", &bindings).await.unwrap();
        assert_ne!(first["db"].auth_token, other["db"].auth_token);
        let response = query(&first["db"], "CREATE TABLE t (x)").await;
        assert!(response.status().is_success());

        let again = connect("/a/tako.toml", &bindings).await.unwrap();
        assert_eq!(again["db"].auth_token, first["db"].auth_token);

        release("/a/tako.toml").await;
        assert_eq!(query(&first["db"], "SELECT 1").await.status(), 401);
        let response = query(&other["db"], "SELECT count(*) FROM t").await;
        assert!(response.status().is_success(), "the file stays open for /b");

        release("/b/tako.toml").await;
        assert!(
            SERVER
                .get()
                .unwrap()
                .registrations
                .lock()
                .await
                .files
                .is_empty()
        );
    }

    #[tokio::test]
    async fn relative_paths_are_rejected() {
        let bindings = HashMap::from([("db".to_string(), "shop.sqlite".to_string())]);
        assert!(connect("/c/tako.toml", &bindings).await.is_err());
    }
}
