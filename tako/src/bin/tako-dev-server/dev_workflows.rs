//! Workflow storage for `tako dev`.
//!
//! Each registered app keeps its runs in the project's
//! `.tako/data/tako/workflows.sqlite`, served by the daemon's database host
//! over the same protocol as on servers, so the workflow store behaves as it
//! does in production.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use tako_workflows::{StoreResolver, WorkflowStoreConfig};

#[cfg_attr(test, allow(dead_code))]
const WORKFLOWS_FILE: &str = ".tako/data/tako/workflows.sqlite";

/// App name → the store its workflow runtime opens.
fn stores() -> &'static Mutex<HashMap<String, WorkflowStoreConfig>> {
    static STORES: OnceLock<Mutex<HashMap<String, WorkflowStoreConfig>>> = OnceLock::new();
    STORES.get_or_init(Mutex::default)
}

/// The workflow manager's view of [`provision`]ed stores.
pub(crate) fn resolver() -> StoreResolver {
    Arc::new(|app| stores().lock().unwrap().get(app).cloned())
}

/// The host scope of a registration's workflow binding.
fn scope(config_path: &str) -> String {
    format!("{config_path}#workflows")
}

/// Serve the project's workflow database to the app registered at
/// `config_path`, creating the file on first use. Its token stays stable
/// while the file does, so running workers keep working across reloads.
pub(crate) async fn provision(
    config_path: &str,
    project_dir: &str,
    app_name: &str,
) -> Result<(), String> {
    let store = serve(config_path, project_dir, app_name).await?;
    stores().lock().unwrap().insert(app_name.to_owned(), store);
    Ok(())
}

#[cfg(not(test))]
async fn serve(
    config_path: &str,
    project_dir: &str,
    app_name: &str,
) -> Result<WorkflowStoreConfig, String> {
    let file = std::path::Path::new(project_dir).join(WORKFLOWS_FILE);
    let bindings = HashMap::from([(
        tako_core::WORKFLOWS_BINDING.to_string(),
        file.display().to_string(),
    )]);
    let connection = crate::databases::connect(&scope(config_path), &bindings)
        .await?
        .remove(tako_core::WORKFLOWS_BINDING)
        .ok_or("the workflow database binding was not served")?;
    Ok(WorkflowStoreConfig::hosted(
        connection.url,
        connection.auth_token,
        app_name,
    ))
}

/// The daemon's database host lives on the runtime that first used it,
/// which a test's runtime is not; tests share one host of their own.
#[cfg(test)]
async fn serve(
    _config_path: &str,
    _project_dir: &str,
    app_name: &str,
) -> Result<WorkflowStoreConfig, String> {
    static SERVER: OnceLock<(tako_db::testing::TestServer, String)> = OnceLock::new();
    let (server, token) = SERVER.get_or_init(|| {
        let server = tako_db::testing::TestServer::start();
        let token = server.database("tests#workflows").token;
        (server, token)
    });
    Ok(WorkflowStoreConfig::hosted(
        server.url.clone(),
        token.clone(),
        app_name,
    ))
}

/// Stop serving the workflow database of the app registered at `config_path`.
pub(crate) async fn release(config_path: &str, app_name: &str) {
    if !app_name.is_empty() {
        stores().lock().unwrap().remove(app_name);
    }
    crate::databases::release(&scope(config_path)).await;
}
