use std::collections::{BTreeMap, HashMap};

use sha2::{Digest, Sha256};

use crate::commands::sqlite::target::{Home, PlannedBinding, inventory_home, resolve_home};
use crate::config::{ServerInventory, TakoToml};
use crate::network::databases::DatabaseRecord;

/// A generated name is only a lookup key: the directory must also name the owner.
fn implicit_name(app: &str) -> String {
    let slug: String = app
        .bytes()
        .take(37)
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                char::from(c)
            } else {
                '-'
            }
        })
        .collect();
    format!(
        "channels-{slug}-{}",
        hex::encode(&Sha256::digest(app.as_bytes())[..8])
    )
}

#[derive(Default)]
pub(in crate::commands::deploy) struct Plan {
    pub sql: HashMap<String, PlannedBinding>,
    pub replay: Option<PlannedBinding>,
    /// A deploy-only client found no generated replay database it could be
    /// using. It cannot create one, so the servers keep the environment's
    /// current channel storage.
    pub unprovisioned_replay: bool,
    pub implicit_replay: bool,
    /// One creation per database, even if several bindings point at it.
    pub create: BTreeMap<String, Home>,
}

#[allow(clippy::too_many_arguments)]
pub(in crate::commands::deploy) fn plan(
    config: &TakoToml,
    servers: &ServerInventory,
    network: &str,
    env: &str,
    app: &str,
    records: &[DatabaseRecord],
    administrator: bool,
    needs_replay: bool,
    postgres: bool,
) -> Result<Plan, String> {
    let mut plan = Plan::default();
    let environment = &config.envs[env];
    let configured = environment.channels.as_ref().map(|c| c.database.as_str());
    let generated = implicit_name(app);
    // An explicit replay database selects hosted SQLite for channels only.
    // Postgres can stay configured for workflows on the same environment.
    let replay = configured.or_else(|| (needs_replay && !postgres).then_some(generated.as_str()));
    let implicit = replay.is_some() && configured.is_none();
    plan.implicit_replay = implicit;

    let sql_names_database = |name: &str| environment.databases.values().any(|db| db == name);
    if implicit && sql_names_database(&generated) {
        return Err(format!(
            "The generated channel database name '{generated}' is also an app SQL database. Select a channel database explicitly with [envs.{env}.channels].database."
        ));
    }
    let members = crate::commands::helpers::resolve_network_servers(servers, network, &[], env)?;
    let mut resolve = |name: &str, for_replay: bool| -> Result<Option<PlannedBinding>, String> {
        let home = if let Some(record) = records.iter().find(|r| r.name == name) {
            if for_replay && implicit && record.channel_owner.as_deref() != Some(app) {
                return Err(format!(
                    "Database '{name}' is not {app}'s automatically created channel database. Select an intended database explicitly with [envs.{env}.channels].database."
                ));
            }
            if !for_replay && record.channel_owner.is_some() {
                return Err(format!(
                    "Database '{name}' was created for channel replay and cannot be bound as an app SQL database."
                ));
            }
            let home = resolve_home(servers, network, &record.server, env)
                .map_err(|error| format!("The server of database '{name}': {error}"))?;
            if home.device_id != record.server {
                return Err(format!(
                    "Database '{name}' has no valid server device ID in the network directory."
                ));
            }
            home
        } else if let [key] = members.as_slice()
            && administrator
        {
            let home = inventory_home(servers, key)?;
            plan.create.insert(name.to_owned(), home.clone());
            home
        } else {
            let hint = if for_replay && implicit {
                format!(" Then set `database = \"{name}\"` under `[envs.{env}.channels]`.")
            } else {
                String::new()
            };
            let mut commands = Vec::new();
            for key in &members {
                let home = inventory_home(servers, key)?;
                // Names can be ambiguous; device IDs never are.
                let selector = if resolve_home(servers, network, &home.name, env).is_ok() {
                    home.name
                } else {
                    home.device_id
                };
                commands.push(format!(
                    "`tako sqlite create {name} --server {selector} --network {network}`"
                ));
            }
            return Err(format!(
                "Database '{name}' is not recorded in this network. With full access, choose its home and run {}.{hint} Then deploy with full access to provision its binding.",
                commands.join(" or ")
            ));
        };
        Ok(Some(PlannedBinding::new(name, home)))
    };
    for (binding, name) in &environment.databases {
        plan.sql.insert(
            binding.clone(),
            resolve(name, false)?.expect("SQL placement"),
        );
    }
    match replay {
        // A deploy-only client cannot read the Postgres credential, so it cannot
        // tell whether a full deploy moved channels to Postgres after creating
        // the generated database. It asserts nothing; the servers keep the
        // committed backend.
        Some(name) if implicit && !administrator => {
            plan.unprovisioned_replay = !records
                .iter()
                .any(|r| r.name == name && r.channel_owner.as_deref() == Some(app));
        }
        Some(name) => plan.replay = resolve(name, true)?,
        None => {}
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    const APP: &str = "shop/production";
    const NETWORK: &str = "11111111111111111111111111111111";

    fn servers(count: usize) -> ServerInventory {
        let mut servers = ServerInventory::default();
        for (server, digit) in [("west", "a"), ("east", "b")].iter().take(count) {
            servers.servers.insert(
                server.to_string(),
                crate::config::ServerEntry {
                    native: Some(crate::management_http::NativeTarget {
                        network: crate::config::LocalNetwork {
                            id: NETWORK.into(),
                            alias: "fleet".into(),
                            name: "fleet".into(),
                            device: "f".repeat(64),
                        },
                        server_name: server.parse().unwrap(),
                        server_id: digit.repeat(64).parse().unwrap(),
                    }),
                    ..Default::default()
                },
            );
        }
        servers
    }
    fn project(settings: &str) -> TakoToml {
        TakoToml::parse(&format!(
            "name='shop'\n[envs.production]\nservers=['west']\nroute='shop.example.com'\n{settings}"
        ))
        .unwrap()
    }
    fn record(name: &str, owner: Option<&str>) -> DatabaseRecord {
        DatabaseRecord {
            name: name.into(),
            server: "a".repeat(64),
            generation: "c".repeat(32),
            created_at_unix_secs: 1,
            channel_owner: owner.map(str::to_owned),
        }
    }

    #[test]
    fn implicit_creation_and_reuse_require_the_right_directory_identity_and_home() {
        let name = implicit_name(APP);
        assert_ne!(implicit_name("a-b/c"), implicit_name("a/b-c"));
        assert_ne!(
            implicit_name(&format!("{}/a", "x".repeat(60))),
            implicit_name(&format!("{}/b", "x".repeat(60)))
        );
        tako_core::validate_database_name(&implicit_name(&"X".repeat(100))).unwrap();
        let project = project("databases={orders='sales', read='sales'}");
        let placement = |records: &[DatabaseRecord]| {
            plan(
                &project,
                &servers(1),
                NETWORK,
                "production",
                APP,
                records,
                true,
                true,
                false,
            )
        };
        let first = placement(&[]).unwrap();
        assert_eq!(first.create.len(), 2);
        assert_eq!(first.sql.len(), 2);
        assert_eq!(first.replay.unwrap().database_name, name);
        let existing = [record(&name, Some(APP)), record("sales", None)];
        assert!(placement(&existing).unwrap().create.is_empty());
        for owner in [None, Some("other/production")] {
            assert!(
                placement(&[record(&name, owner)])
                    .err()
                    .unwrap()
                    .contains("not shop/production's")
            );
        }
        let mut moved = existing.clone();
        moved[0].server = "e".repeat(64);
        assert!(
            placement(&moved)
                .err()
                .unwrap()
                .contains("not a trusted server")
        );
    }

    #[test]
    fn one_deploy_target_in_a_larger_network_does_not_choose_placement() {
        let project = project("");
        let missing = plan(
            &project,
            &servers(2),
            NETWORK,
            "production",
            APP,
            &[],
            true,
            true,
            false,
        )
        .err()
        .unwrap();
        let name = implicit_name(APP);
        for server in ["east", "west"] {
            assert!(missing.contains(&format!(
                "tako sqlite create {name} --server {server} --network {NETWORK}"
            )));
        }
        assert!(missing.contains("[envs.production.channels]"));
        let mut admin_created = record(&name, None);
        admin_created.server = "b".repeat(64);
        let configured =
            super::tests::project(&format!("[envs.production.channels]\ndatabase='{name}'"));
        let placement = plan(
            &configured,
            &servers(2),
            NETWORK,
            "production",
            APP,
            &[admin_created],
            true,
            true,
            false,
        )
        .unwrap();
        assert!(placement.create.is_empty());
        assert_eq!(placement.replay.unwrap().home.name, "east");
    }

    #[test]
    fn scoped_deploy_requires_explicit_databases_and_only_asserts_provisioned_replay() {
        let name = implicit_name(APP);
        let scoped = |project: &TakoToml, records: &[DatabaseRecord]| {
            plan(
                project,
                &servers(1),
                NETWORK,
                "production",
                APP,
                records,
                false,
                true,
                false,
            )
        };
        // Even with the generated record, the environment may have moved to
        // Postgres since; only an explicit database is asserted.
        let found = scoped(&project(""), &[record(&name, Some(APP))]).unwrap();
        assert!(found.create.is_empty() && found.replay.is_none());
        assert!(!found.unprovisioned_replay);
        assert!(scoped(&project(""), &[]).unwrap().unprovisioned_replay);
        let explicit = scoped(
            &project(&format!("[envs.production.channels]\ndatabase='{name}'")),
            &[record(&name, None)],
        )
        .unwrap();
        assert!(explicit.replay.unwrap().binding.peer_key.is_none());
        for settings in [
            "databases={main='orders'}",
            "[envs.production.channels]\ndatabase='orders'",
        ] {
            assert!(
                scoped(&project(settings), &[])
                    .err()
                    .unwrap()
                    .contains("full access")
            );
        }
        let pg = plan(
            &project(""),
            &servers(2),
            NETWORK,
            "production",
            APP,
            &[],
            true,
            true,
            true,
        )
        .unwrap();
        assert!(pg.replay.is_none() && pg.create.is_empty());
        // Channels can use hosted SQLite while workflows keep Postgres.
        let both = plan(
            &project("[envs.production.channels]\ndatabase='other'"),
            &servers(1),
            NETWORK,
            "production",
            APP,
            &[],
            true,
            true,
            true,
        )
        .unwrap();
        assert_eq!(both.replay.unwrap().database_name, "other");
        assert!(!both.unprovisioned_replay);
    }
}
