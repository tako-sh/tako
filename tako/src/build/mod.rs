//! Build system for Tako
//!
//! Handles:
//! - Running build commands
//! - Creating deployment archives
//! - Build caching

mod adapter;
mod artifact;
mod container;
mod executor;
pub mod go;
pub mod js;
mod preset;
pub(crate) mod preset_cache;
pub mod schemas;
mod workdir;

pub use adapter::*;
pub use artifact::*;
pub use container::*;
pub use executor::*;
pub use preset::*;
pub use workdir::*;

/// Secret names for generated types: the development environment's, or every
/// environment's when there is none. A project without secrets has none;
/// secrets that cannot be read are an error, so generated types never claim
/// there are none.
pub(crate) fn secret_names(
    project_dir: &std::path::Path,
    config: Option<&crate::config::TakoToml>,
    environment: Option<&str>,
) -> std::io::Result<Vec<String>> {
    use crate::config::SecretsStore;
    let store = match (config, environment) {
        (Some(config), Some(env)) => SecretsStore::load_for_environment(project_dir, config, env),
        (Some(config), None) => SecretsStore::load_with_config(project_dir, config),
        (None, _) => SecretsStore::load_from_dir(project_dir),
    }
    .map_err(|error| std::io::Error::other(format!("Could not read secrets: {error}")))?;
    Ok(match store.get_env("development") {
        Some(development) => {
            let mut names: Vec<_> = development.keys().cloned().collect();
            names.sort();
            names
        }
        None => store.all_secret_names(),
    })
}
