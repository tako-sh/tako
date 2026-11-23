//! Local KV stores for apps registered with the dev daemon.
//!
//! The daemon serves each project's store files through its internal
//! socket, as `tako-server` does. Each registration (keyed by its config
//! path) gets its own tokens, which stay stable across re-registration while
//! a binding names the same file, so running processes and workflow workers
//! keep working after a reload.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tako_core::{KvConnection, KvRequest, Response};
use tako_kv::{Binding, Home, HostConfig, KvHost, StoreId};
use tokio::sync::Mutex;

struct Server {
    host: KvHost,
    registrations: Mutex<Registrations>,
}

#[derive(Default)]
struct Registrations {
    /// Served files and their IDs.
    files: HashMap<PathBuf, StoreId>,
    /// Config path → binding → (file, token).
    scopes: HashMap<String, Scope>,
}

static SERVER: std::sync::LazyLock<Server> = std::sync::LazyLock::new(|| Server {
    host: KvHost::new(HostConfig::default()),
    registrations: Mutex::new(Registrations::default()),
});

fn server() -> &'static Server {
    &SERVER
}

/// Serves an app's request from the internal socket. `config_path` is the
/// registration of the calling app. Blocks.
pub(crate) fn call(config_path: &str, token: &str, request: KvRequest) -> Response {
    match server().host.call(config_path, token, request) {
        Ok(value) => Response::ok(value),
        Err(error) => error.into_response(),
    }
}

/// A stable ID for a store file; it never leaves this process.
fn local_id(path: &Path) -> StoreId {
    let digest = Sha256::digest(path.to_string_lossy().as_bytes());
    StoreId::parse(&format!("local-{}", hex::encode(&digest[..16])))
        .expect("a hex digest makes a valid name")
}

/// Serves `bindings` (binding name → store file) for the app registered at
/// `config_path`, replacing its previous bindings, and returns the
/// connections its processes receive.
pub(crate) async fn connect(
    config_path: &str,
    bindings: &HashMap<String, String>,
) -> Result<HashMap<String, KvConnection>, String> {
    if bindings.is_empty() {
        release(config_path).await;
        return Ok(HashMap::new());
    }
    for (binding, file) in bindings {
        if !Path::new(file).is_absolute() {
            return Err(format!(
                "KV binding {binding} must name an absolute path, got {file}"
            ));
        }
    }
    let server = server();
    let mut registrations = server.registrations.lock().await;
    let previous = registrations
        .scopes
        .get(config_path)
        .cloned()
        .unwrap_or_default();
    let (scope, host_bindings) =
        match open_bindings(server, &mut registrations, &previous, bindings).await {
            Ok(opened) => opened,
            Err(error) => {
                // No scope names the files this call opened, so this closes
                // them; the app's previous bindings stay as they were.
                close_unused(server, &mut registrations).await;
                return Err(error);
            }
        };
    server.host.set_bindings(config_path, host_bindings);
    let connections = scope
        .iter()
        .map(|(binding, (_, token))| (binding.clone(), KvConnection::new(token)))
        .collect();
    registrations.scopes.insert(config_path.to_owned(), scope);
    close_unused(server, &mut registrations).await;
    Ok(connections)
}

type Scope = BTreeMap<String, (PathBuf, String)>;

/// Opens the files `bindings` name that are not open yet, in binding
/// order, and returns the app's new scope with the host's bindings for it.
/// Tokens are kept for bindings that still name the same file.
async fn open_bindings(
    server: &Server,
    registrations: &mut Registrations,
    previous: &Scope,
    bindings: &HashMap<String, String>,
) -> Result<(Scope, Vec<Binding>), String> {
    let mut scope = BTreeMap::new();
    let mut host_bindings = Vec::new();
    let ordered: BTreeMap<&String, &String> = bindings.iter().collect();
    for (binding, file) in ordered {
        let path = PathBuf::from(file);
        let id = match registrations.files.get(&path) {
            Some(id) => id.clone(),
            None => {
                let id = local_id(&path);
                let host = server.host.clone();
                let register_path = path.clone();
                let register_id = id.clone();
                // Creating and opening the file blocks.
                tokio::task::spawn_blocking(move || {
                    if let Some(parent) = register_path.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                    }
                    host.register(register_id, register_path)
                        .map_err(|e| e.to_string())
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
            _ => tako_kv::generate_token(),
        };
        host_bindings.push(Binding {
            token_hash: tako_kv::hash_token(&token),
            store: id,
            home: Home::Local,
        });
        scope.insert(binding.clone(), (path, token));
    }
    Ok((scope, host_bindings))
}

/// Revokes the bindings of the app registered at `config_path` and closes
/// files no other app uses.
pub(crate) async fn release(config_path: &str) {
    let server = server();
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
    let unused: Vec<(PathBuf, StoreId)> = registrations
        .files
        .iter()
        .filter(|(path, _)| !used.contains(path))
        .map(|(path, id)| (path.clone(), id.clone()))
        .collect();
    for (path, id) in unused {
        let host = server.host.clone();
        // Closing waits for the store's pending writes.
        match tokio::task::spawn_blocking(move || host.unregister(&id)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(path = %path.display(), "Failed to close local KV store: {error}");
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), "Failed to close local KV store: {error}");
            }
        }
        registrations.files.remove(&path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs `request` as the app registered at `config_path`.
    async fn send(config_path: &str, connection: &KvConnection, request: KvRequest) -> Response {
        let (config_path, token) = (config_path.to_owned(), connection.token.clone());
        tokio::task::spawn_blocking(move || call(&config_path, &token, request))
            .await
            .unwrap()
    }

    fn put(key: &str) -> KvRequest {
        KvRequest::Put {
            key: key.into(),
            json: r#""v""#.into(),
            ttl_ms: None,
        }
    }

    fn get(key: &str) -> KvRequest {
        KvRequest::Get { key: key.into() }
    }

    #[tokio::test]
    async fn registrations_share_files_and_keep_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir
            .path()
            .join("kv")
            .join("sessions.redb")
            .display()
            .to_string();
        let bindings = HashMap::from([("auth".to_string(), file.clone())]);

        let first = connect("/a/tako.toml", &bindings).await.unwrap();
        let other = connect("/b/tako.toml", &bindings).await.unwrap();
        assert_ne!(first["auth"].token, other["auth"].token);
        assert!(matches!(
            send("/a/tako.toml", &first["auth"], put("k")).await,
            Response::Ok { .. }
        ));
        // A registration cannot use another's token.
        assert!(send("/b/tako.toml", &first["auth"], get("k")).await.error_message().is_some());

        let again = connect("/a/tako.toml", &bindings).await.unwrap();
        assert_eq!(again["auth"].token, first["auth"].token);

        release("/a/tako.toml").await;
        assert!(send("/a/tako.toml", &first["auth"], get("k")).await.error_message().is_some());
        assert!(
            matches!(
                send("/b/tako.toml", &other["auth"], get("k")).await,
                Response::Ok { data } if data == r#""v""#
            ),
            "the file stays open for /b"
        );
        release("/b/tako.toml").await;
        assert!(!is_open(&file).await);
    }

    /// Whether the daemon has `file` open.
    async fn is_open(file: &str) -> bool {
        server()
            .registrations
            .lock()
            .await
            .files
            .contains_key(Path::new(file))
    }

    #[tokio::test]
    async fn failed_registrations_close_the_files_they_opened() {
        let dir = tempfile::tempdir().unwrap();
        let path = |name: &str| dir.path().join(name).display().to_string();
        let (kept, opened) = (path("kept.redb"), path("opened.redb"));
        // A file's directory cannot be created under a regular file.
        std::fs::write(dir.path().join("plain"), "").unwrap();
        let broken = path("plain/broken.redb");
        let before = HashMap::from([("a".to_string(), kept.clone())]);
        let first = connect("/d/tako.toml", &before).await.unwrap();

        // Bindings open in name order: `b` opens, then `c` fails.
        let after = HashMap::from([
            ("a".to_string(), kept.clone()),
            ("b".to_string(), opened.clone()),
            ("c".to_string(), broken),
        ]);
        assert!(connect("/d/tako.toml", &after).await.is_err());
        assert!(
            !is_open(&opened).await,
            "the file the failed call opened is closed"
        );
        assert!(is_open(&kept).await);
        assert!(
            matches!(send("/d/tako.toml", &first["a"], put("k")).await, Response::Ok { .. }),
            "the previous binding still works"
        );
        release("/d/tako.toml").await;
        assert!(!is_open(&kept).await);
    }

    #[tokio::test]
    async fn relative_paths_are_rejected() {
        let bindings = HashMap::from([("auth".to_string(), "sessions.redb".to_string())]);
        assert!(connect("/c/tako.toml", &bindings).await.is_err());
    }
}
