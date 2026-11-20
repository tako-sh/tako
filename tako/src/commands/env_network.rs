//! The network an app environment uses. A project records each environment's
//! network in `tako.toml`, which the team shares through version
//! control, so commands inside a project name only the environment.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

use crate::config::{LocalNetwork, LocalNetworks, TakoToml};
use crate::output;

/// Whether a command may record the environment's network.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Purpose {
    /// A deploy: it records the network it uses, and moving a recorded
    /// environment to another network needs confirmation.
    Deploy,
    /// A secret change or sync: it records the network it uses when none is
    /// recorded. A different `--network` applies for this command only, so
    /// drafts can still be published to the network they were written for.
    Record,
    /// The command acts on what the environment already has. `--network`
    /// applies for this command only and changes no record.
    Use,
}

/// Networks resolved in this process, by environment, so a command asks once
/// and every later step of it uses the same answer.
static RESOLVED: Mutex<BTreeMap<String, LocalNetwork>> = Mutex::new(BTreeMap::new());

/// The network `env` uses: `--network`, then the network recorded for it,
/// then this machine's only network. With several networks and nothing
/// recorded, an interactive terminal asks and anything else fails.
pub(crate) fn resolve(
    config_path: &Path,
    env: &str,
    purpose: Purpose,
) -> Result<LocalNetwork, String> {
    if env == "development" {
        return Err(
            "The development environment runs locally with `tako dev` and has no network".into(),
        );
    }
    if let Some(local) = RESOLVED.lock().unwrap().get(env) {
        return Ok(local.clone());
    }
    let local = choose(config_path, env, purpose, crate::network::network_flag())?;
    RESOLVED
        .lock()
        .unwrap()
        .insert(env.to_string(), local.clone());
    Ok(local)
}

/// The network this command resolved for `env`, if it did. Commands resolve
/// their environment's network once, before acting, so later steps read it
/// here instead of threading it through.
pub(crate) fn resolved(env: &str) -> Option<LocalNetwork> {
    RESOLVED.lock().unwrap().get(env).cloned()
}

/// The ID of the network `env` uses, without asking: the network this
/// command resolved, else the recorded one, even if this machine hasn't
/// joined it. `None` before the environment has a network.
pub(crate) fn current_id(config: &TakoToml, env: &str) -> Option<String> {
    if env == "development" {
        return None;
    }
    if let Some(local) = RESOLVED.lock().unwrap().get(env) {
        return Some(local.id.clone());
    }
    config.envs.get(env)?.network.clone()
}

/// Resolve `env`'s network when it has one already or the command names one
/// with `--network`; `None` otherwise, without asking. Secret edits use this:
/// a draft for an environment that has no network yet stays local until the
/// environment's first deploy or sync picks one.
pub(crate) fn resolve_if_known(
    config_path: &Path,
    env: &str,
    purpose: Purpose,
) -> Result<Option<LocalNetwork>, String> {
    if env == "development" {
        return Ok(None);
    }
    let config = TakoToml::load_from_file(config_path).map_err(|error| error.to_string())?;
    if crate::network::network_flag().is_none() && current_id(&config, env).is_none() {
        return Ok(None);
    }
    resolve(config_path, env, purpose).map(Some)
}

fn choose(
    config_path: &Path,
    env: &str,
    purpose: Purpose,
    flag: Option<&str>,
) -> Result<LocalNetwork, String> {
    let config = TakoToml::load_from_file(config_path).map_err(|error| error.to_string())?;
    let networks = LocalNetworks::load().map_err(|error| error.to_string())?;
    let recorded = config
        .envs
        .get(env)
        .and_then(|spec| spec.network.as_deref());
    let local = match (flag, recorded) {
        (Some(flag), recorded) => {
            let local = networks
                .select(Some(flag))
                .map_err(|error| error.to_string())?
                .clone();
            match recorded {
                Some(id) if id == local.id => return Ok(local),
                None if purpose == Purpose::Use => return Ok(local),
                Some(_) if purpose != Purpose::Deploy => return Ok(local),
                Some(id) => {
                    let from = label(&networks, id);
                    // Refuse before asking, and before recording anything:
                    // a refused move leaves the environment where it was.
                    if crate::network::secrets::unpublished_elsewhere(
                        config_path.parent().unwrap_or(Path::new(".")),
                        env,
                        &local.id,
                    )
                    .map_err(|error| error.to_string())?
                    {
                        return Err(format!(
                            "This checkout has unpublished {env} secrets for network {from}. Publish them with `tako secrets sync --env {env}` before moving {env}, or remove them."
                        ));
                    }
                    confirm_move(config_path, env, &from, &local)?;
                    local
                }
                None => local,
            }
        }
        (None, Some(id)) => {
            return networks.networks.get(id).cloned().ok_or_else(|| {
                format!(
                    "Environment '{env}' uses network {id} (recorded in {}), which this machine hasn't joined. Join it with `tako network join`, or pass --network to use another network.",
                    config_path.display()
                )
            });
        }
        (None, None) => match networks.networks.len() {
            0 | 1 => networks
                .select(None)
                .map_err(|error| error.to_string())?
                .clone(),
            _ if !output::is_interactive() => {
                return Err(format!(
                    "This machine has several networks and '{env}' has none recorded yet. Pass --network with one of: {}.",
                    networks.aliases().join(", ")
                ));
            }
            _ => crate::network::pick_network(&networks, &format!("Network for {env}"))
                .map_err(|error| error.to_string())?,
        },
    };
    if purpose != Purpose::Use && !output::is_dry_run() {
        TakoToml::record_network(config_path, env, &local.id).map_err(|error| error.to_string())?;
        output::hint(&format!(
            "Recorded {env} on network {} in {}. Commit it so everyone uses the same network for {env}.",
            local.alias,
            config_path.display()
        ));
    }
    Ok(local)
}

/// Moving an environment changes where everyone deploys it, so it needs an
/// interactive confirmation.
fn confirm_move(
    config_path: &Path,
    env: &str,
    from: &str,
    to: &LocalNetwork,
) -> Result<(), String> {
    if !output::is_interactive() {
        return Err(format!(
            "Environment '{env}' uses network {from} (recorded in {}). Run this command in a terminal to move it to {}, or drop --network.",
            config_path.display(),
            to.alias
        ));
    }
    let moved = output::confirm_with_description(
        &format!("Move {env} from network {from} to {}?", to.alias),
        Some(&format!(
            "Secrets stay on {from}; set {env}'s secrets on {} before it serves traffic.",
            to.alias
        )),
        false,
    )
    .map_err(|error| error.to_string())?;
    if moved {
        Ok(())
    } else {
        Err(output::operation_cancelled_error().to_string())
    }
}

/// A recorded network's alias on this machine, or its ID.
fn label(networks: &LocalNetworks, id: &str) -> String {
    networks
        .networks
        .get(id)
        .map_or_else(|| id.to_string(), |local| local.alias.clone())
}

/// Resolve `env`'s network and load its servers.
pub(crate) async fn servers(
    config_path: &Path,
    env: &str,
    purpose: Purpose,
) -> Result<crate::config::ServerInventory, String> {
    let network = resolve(config_path, env, purpose)?;
    super::helpers::load_management_servers(Some(vec![network])).await
}

/// Servers for a command that runs inside a project or names an app: those of
/// the project environment's network when the command has one, else those of
/// every network on this machine.
pub(crate) async fn servers_for_target(
    config_path: Option<&Path>,
    config: Option<&crate::config::TakoToml>,
    env: Option<&str>,
) -> Result<crate::config::ServerInventory, String> {
    match (config_path, config, env) {
        (Some(config_path), Some(config), Some(env)) if config.envs.contains_key(env) => {
            servers(config_path, env, Purpose::Use).await
        }
        _ => super::helpers::load_management_servers(None).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn network() -> LocalNetwork {
        let fixture = crate::network::tests::fixture();
        crate::network::store_keys(&fixture.profile.request().request_id, &fixture.keys).unwrap();
        crate::network::save_profile(&fixture.profile).unwrap()
    }

    fn project() -> tempfile::TempDir {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            project.path().join("tako.toml"),
            "name = 'shop'\n[envs.production]\nroute = 'shop.example.com'\n",
        )
        .unwrap();
        project
    }

    fn recorded(path: &Path, env: &str) -> Option<String> {
        TakoToml::load_from_file(path)
            .unwrap()
            .envs
            .get(env)?
            .network
            .clone()
    }

    #[test]
    fn the_only_network_is_used_and_recorded_by_commands_that_record() {
        let _home = crate::network::tests::TestHome::new();
        let project = project();
        let path = project.path().join("tako.toml");
        let only = network();
        assert_eq!(
            choose(&path, "production", Purpose::Use, None).unwrap(),
            only
        );
        assert_eq!(recorded(&path, "production"), None);
        assert_eq!(
            choose(&path, "production", Purpose::Record, None).unwrap(),
            only
        );
        assert_eq!(recorded(&path, "production"), Some(only.id));
        assert!(
            choose(&path, "development", Purpose::Use, None).is_ok(),
            "choose itself does not special-case development"
        );
        assert!(resolve(&path, "development", Purpose::Use).is_err());
    }

    #[test]
    fn several_networks_need_a_record_or_a_flag_without_a_terminal() {
        let _home = crate::network::tests::TestHome::new();
        let project = project();
        let path = project.path().join("tako.toml");
        let first = network();
        let second = network();
        assert_ne!(first.alias, second.alias, "same names get distinct aliases");
        let error = choose(&path, "production", Purpose::Record, None).unwrap_err();
        assert!(
            error.contains(&first.alias) && error.contains(&second.alias),
            "{error}"
        );

        assert_eq!(
            choose(&path, "production", Purpose::Record, Some(&second.alias)).unwrap(),
            second
        );
        assert_eq!(recorded(&path, "production"), Some(second.id.clone()));
        // The record decides from now on, without a flag.
        assert_eq!(
            choose(&path, "production", Purpose::Record, None).unwrap(),
            second
        );
        // A flag for another network applies once to commands that only use it.
        assert_eq!(
            choose(&path, "production", Purpose::Use, Some(&first.alias)).unwrap(),
            first
        );
        assert_eq!(recorded(&path, "production"), Some(second.id.clone()));
        // A secret sync to another network publishes there once.
        assert_eq!(
            choose(&path, "production", Purpose::Record, Some(&first.alias)).unwrap(),
            first
        );
        assert_eq!(recorded(&path, "production"), Some(second.id.clone()));
        // Only a deploy moves a recorded environment, and it needs a terminal
        // to confirm.
        let error = choose(&path, "production", Purpose::Deploy, Some(&first.alias)).unwrap_err();
        assert!(error.contains("move it"), "{error}");
        assert_eq!(recorded(&path, "production"), Some(second.id));
    }

    #[test]
    fn a_recorded_network_this_machine_has_not_joined_fails() {
        let _home = crate::network::tests::TestHome::new();
        let project = project();
        let path = project.path().join("tako.toml");
        network();
        TakoToml::record_network(&path, "production", &"f".repeat(32)).unwrap();
        let error = choose(&path, "production", Purpose::Use, None).unwrap_err();
        assert!(error.contains("hasn't joined"), "{error}");
        assert_eq!(
            current_id(&TakoToml::load_from_file(&path).unwrap(), "production"),
            Some("f".repeat(32))
        );
        assert_eq!(
            current_id(&TakoToml::load_from_file(&path).unwrap(), "staging"),
            None
        );
    }
}
