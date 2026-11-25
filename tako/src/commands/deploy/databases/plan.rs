use std::collections::{BTreeMap, HashMap};

use sha2::{Digest, Sha256};
use tako_core::DatabaseEngine;

use crate::commands::db::target::{PlannedBinding, inventory_home};
use crate::config::{ServerInventory, TakoToml};
use crate::network::databases::DatabaseRecord;

/// A database tako-server uses on the app's behalf rather than one the app
/// binds for SQL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::commands::deploy) enum Service {
    /// Durable channel replay and Store invalidations.
    Channels,
    /// Workflow runs, steps and schedules.
    Workflows,
}

impl Service {
    /// The `[envs.<env>.<table>]` that names the service's database.
    pub(in crate::commands::deploy) fn table(self) -> &'static str {
        match self {
            Self::Channels => "channels",
            Self::Workflows => "workflows",
        }
    }

    fn noun(self) -> &'static str {
        match self {
            Self::Channels => "channel replay",
            Self::Workflows => "workflow runs",
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Self::Channels => "channels-",
            Self::Workflows => "workflows-",
        }
    }

    fn owner(self, record: &DatabaseRecord) -> Option<&str> {
        match self {
            Self::Channels => record.channel_owner.as_deref(),
            Self::Workflows => record.workflows_owner.as_deref(),
        }
    }

    fn configured(self, environment: &crate::config::EnvConfig) -> Option<&str> {
        match self {
            Self::Channels => environment.channels.as_ref().map(|c| c.db.as_str()),
            Self::Workflows => environment.workflows.as_ref().map(|w| w.db.as_str()),
        }
    }
}

/// A generated name is only a lookup key: the directory must also name the owner.
pub(in crate::commands::deploy) fn implicit_name(service: Service, app: &str) -> String {
    let prefix = service.prefix();
    // Prefix, slug, `-` and 16 hex digits fit the 63-character name limit.
    let slug: String = app
        .bytes()
        .take(63 - prefix.len() - 17)
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                char::from(c)
            } else {
                '-'
            }
        })
        .collect();
    format!(
        "{prefix}{slug}-{}",
        hex::encode(&Sha256::digest(app.as_bytes())[..8])
    )
}

/// Where one service's database lives for this deploy.
#[derive(Default)]
pub(in crate::commands::deploy) struct ServicePlan {
    pub binding: Option<PlannedBinding>,
    /// The database is the generated one, not named in `tako.toml`.
    pub implicit: bool,
    /// A deploy-only client found no generated database it could be using.
    /// It cannot create one, so the servers keep the environment's current
    /// binding.
    pub unprovisioned: bool,
}

#[derive(Default)]
pub(in crate::commands::deploy) struct Plan {
    pub sql: HashMap<String, PlannedBinding>,
    /// KV store bindings share database grants and backup configuration.
    pub kv: HashMap<String, PlannedBinding>,
    pub replay: ServicePlan,
    pub workflows: ServicePlan,
    /// One creation per database, even if several bindings point at it.
    pub create: BTreeMap<String, PlannedBinding>,
    pub reservations: BTreeMap<String, String>,
}

impl Plan {
    pub(in crate::commands::deploy) fn service(&self, service: Service) -> &ServicePlan {
        match service {
            Service::Channels => &self.replay,
            Service::Workflows => &self.workflows,
        }
    }

    /// The services' bindings, with the deploy binding name each uses.
    pub(in crate::commands::deploy) fn service_bindings(
        &self,
    ) -> impl Iterator<Item = (&'static str, &PlannedBinding)> {
        self.replay
            .binding
            .iter()
            .map(|binding| (tako_core::CHANNEL_REPLAY_BINDING, binding))
            .chain(
                self.workflows
                    .binding
                    .iter()
                    .map(|binding| (tako_core::WORKFLOWS_BINDING, binding)),
            )
    }
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
    needs_workflows: bool,
) -> Result<Plan, String> {
    let mut plan = Plan::default();
    let environment = &config.envs[env];
    let members = crate::commands::helpers::resolve_network_servers(
        servers,
        network,
        &environment.servers,
        env,
    )?;
    let sql_names_database = |name: &str| environment.databases.values().any(|db| db == name);

    let first = members.first().ok_or("No storage target selected")?;
    let coordinator = inventory_home(servers, first)?;
    let mut resolve = |name: &str,
                       engine: DatabaseEngine,
                       usage: Option<(Service, bool)>|
     -> Result<PlannedBinding, String> {
        let mut planned = PlannedBinding::new(name, coordinator.clone(), engine);
        if let Some(record) = records.iter().find(|r| r.name == name) {
            if record.engine != engine {
                if engine == DatabaseEngine::Kv {
                    return Err(format!(
                        "'{name}' is a SQLite database. Bind it under [envs.{env}].db instead of kv."
                    ));
                }
                return Err(match usage {
                    Some((service, _)) => format!(
                        "'{name}' is a KV store, so it cannot hold {}. Select a SQLite database with [envs.{env}.{}].db.",
                        service.noun(),
                        service.table()
                    ),
                    None => format!(
                        "'{name}' is a KV store. Bind it under [envs.{env}].kv instead of db."
                    ),
                });
            }
            match usage {
                Some((service, true)) if service.owner(record) != Some(app) => {
                    return Err(format!(
                        "Database '{name}' is not {app}'s automatically created {} database. Select an intended database explicitly with [envs.{env}.{}].db.",
                        service.noun(),
                        service.table()
                    ));
                }
                None if record.channel_owner.is_some() || record.workflows_owner.is_some() => {
                    return Err(format!(
                        "Database '{name}' was created for {} and cannot be bound as an app SQL database.",
                        if record.channel_owner.is_some() {
                            Service::Channels.noun()
                        } else {
                            Service::Workflows.noun()
                        }
                    ));
                }
                _ => {}
            }
            planned.binding.generation = Some(record.generation.clone());
            planned.binding.server = record.server.clone();
        } else if administrator {
            plan.create
                .entry(name.to_owned())
                .or_insert_with(|| planned.clone());
        } else {
            return Err(format!(
                "Resource '{name}' has not been provisioned. Deploy environment '{env}' with full access to create its replicas and grant this binding."
            ));
        }
        Ok(planned)
    };

    // The services' choices are checked before anything is resolved, so a
    // conflicting configuration fails without planning creations.
    let mut services = Vec::new();
    for (service, needed) in [
        (Service::Channels, needs_replay),
        (Service::Workflows, needs_workflows),
    ] {
        let configured = service.configured(environment);
        let generated = implicit_name(service, app);
        let Some(name) = configured
            .map(str::to_owned)
            .or_else(|| needed.then_some(generated.clone()))
        else {
            continue;
        };
        let implicit = configured.is_none();
        if implicit && sql_names_database(&generated) {
            return Err(format!(
                "The generated {} database name '{generated}' is also an app SQL database. Select a database explicitly with [envs.{env}.{}].db.",
                service.noun(),
                service.table()
            ));
        }
        if environment.kv.values().any(|store| *store == name) {
            return Err(format!(
                "'{name}' is bound as a KV store in {env}, so it cannot also hold {}. Select another database with [envs.{env}.{}].db.",
                service.noun(),
                service.table()
            ));
        }
        services.push((service, name, implicit));
    }

    for (binding, name) in &environment.databases {
        if environment.kv.values().any(|store| store == name) {
            return Err(format!(
                "'{name}' is bound as both a SQLite database and a KV store in {env}. Use separate resource names."
            ));
        }
        plan.sql.insert(
            binding.clone(),
            resolve(name, DatabaseEngine::Sqlite, None)?,
        );
    }
    for (binding, name) in &environment.kv {
        plan.kv
            .insert(binding.clone(), resolve(name, DatabaseEngine::Kv, None)?);
    }
    for (service, name, implicit) in services {
        let planned = if implicit && !administrator {
            // A deploy-only client cannot create or grant the generated
            // database; it asserts nothing and the servers keep the binding
            // a full deploy provisioned.
            ServicePlan {
                binding: None,
                implicit,
                unprovisioned: !records
                    .iter()
                    .any(|r| r.name == name && service.owner(r) == Some(app)),
            }
        } else {
            ServicePlan {
                binding: Some(resolve(
                    &name,
                    DatabaseEngine::Sqlite,
                    Some((service, implicit)),
                )?),
                implicit,
                unprovisioned: false,
            }
        };
        match service {
            Service::Channels => plan.replay = planned,
            Service::Workflows => plan.workflows = planned,
        }
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
            founders: ["a".repeat(64)].into(),
            server: "a".repeat(64),
            generation: "c".repeat(32),
            created_at_unix_secs: 1,
            engine: tako_core::DatabaseEngine::Sqlite,
            channel_owner: owner.map(str::to_owned),
            workflows_owner: None,
        }
    }
    fn workflows_record(name: &str, owner: Option<&str>) -> DatabaseRecord {
        DatabaseRecord {
            channel_owner: None,
            workflows_owner: owner.map(str::to_owned),
            ..record(name, None)
        }
    }

    #[test]
    fn implicit_creation_and_reuse_require_the_right_directory_identity_and_home() {
        let name = implicit_name(Service::Channels, APP);
        for service in [Service::Channels, Service::Workflows] {
            assert_ne!(
                implicit_name(service, "a-b/c"),
                implicit_name(service, "a/b-c")
            );
            assert_ne!(
                implicit_name(service, &format!("{}/a", "x".repeat(60))),
                implicit_name(service, &format!("{}/b", "x".repeat(60)))
            );
            tako_core::validate_database_name(&implicit_name(service, &"X".repeat(100))).unwrap();
        }
        assert_ne!(
            implicit_name(Service::Channels, APP),
            implicit_name(Service::Workflows, APP)
        );
        let project = project("db={orders='sales', read='sales'}");
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
        assert_eq!(first.replay.binding.unwrap().database_name, name);
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
        let planned = placement(&moved).unwrap();
        let replay = planned.replay.binding.unwrap();
        assert_eq!(replay.home.name, "west");
        assert_eq!(
            replay.binding.generation.as_ref(),
            Some(&moved[0].generation)
        );
    }

    #[test]
    fn only_selected_targets_host_new_resources() {
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
        .unwrap();
        let name = implicit_name(Service::Channels, APP);
        assert_eq!(missing.create[&name].home.name, "west");
        let mut admin_created = record(&name, None);
        admin_created.server = "b".repeat(64);
        let configured = super::tests::project(&format!("[envs.production.channels]\ndb='{name}'"));
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
        assert_eq!(placement.replay.binding.unwrap().home.name, "west");
    }

    #[test]
    fn scoped_deploy_requires_explicit_databases_and_only_asserts_provisioned_replay() {
        let name = implicit_name(Service::Channels, APP);
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
        // A deploy-only client cannot grant the generated database, so it
        // asserts only an explicit one.
        let found = scoped(&project(""), &[record(&name, Some(APP))]).unwrap();
        assert!(found.create.is_empty() && found.replay.binding.is_none());
        assert!(!found.replay.unprovisioned);
        assert!(scoped(&project(""), &[]).unwrap().replay.unprovisioned);
        let explicit = scoped(
            &project(&format!("[envs.production.channels]\ndb='{name}'")),
            &[record(&name, None)],
        )
        .unwrap();
        assert!(explicit.replay.binding.unwrap().binding.peer_key.is_none());
        for settings in [
            "db={main='orders'}",
            "kv={auth='sessions'}",
            "[envs.production.channels]\ndb='orders'",
        ] {
            assert!(
                scoped(&project(settings), &[])
                    .err()
                    .unwrap()
                    .contains("full access")
            );
        }
    }
    #[test]
    fn kv_stores_are_created_once_with_their_own_engine_and_never_hold_replay() {
        let kv = |name: &str| DatabaseRecord {
            engine: tako_core::DatabaseEngine::Kv,
            ..record(name, None)
        };
        let place = |settings: &str, records: &[DatabaseRecord]| {
            plan(
                &project(settings),
                &servers(1),
                NETWORK,
                "production",
                APP,
                records,
                true,
                false,
                false,
            )
        };
        // A KV binding names an existing store and is planned apart from SQL.
        let planned = place("kv={auth='sessions'}", &[kv("sessions")]).unwrap();
        assert_eq!(
            planned.kv["auth"].binding.engine,
            tako_core::DatabaseEngine::Kv
        );
        assert!(planned.sql.is_empty() && planned.create.is_empty());
        let first = place("kv={auth='sessions', shared='sessions'}", &[]).unwrap();
        assert_eq!(first.create.len(), 1);
        assert_eq!(first.kv.len(), 2);
        assert_eq!(first.create["sessions"].binding.engine, DatabaseEngine::Kv);
        assert_eq!(first.create["sessions"].home.name, "west");
        assert!(
            place("db={main='sessions'}\nkv={auth='sessions'}", &[])
                .err()
                .unwrap()
                .contains("both")
        );
        // Each kind of binding names a record of its own kind.
        assert!(
            place("kv={auth='main'}", &[record("main", None)])
                .err()
                .unwrap()
                .contains("is a SQLite database")
        );
        assert!(
            place("db={main='sessions'}", &[kv("sessions")])
                .err()
                .unwrap()
                .contains("is a KV store")
        );
        // Channel replay needs a SQLite database.
        assert!(
            place("channels.db='sessions'", &[kv("sessions")])
                .err()
                .unwrap()
                .contains("cannot hold channel replay")
        );
    }

    #[test]
    fn workflows_get_a_database_like_channel_replay() {
        let name = implicit_name(Service::Workflows, APP);
        let place =
            |settings: &str, servers_count: usize, records: &[DatabaseRecord], admin: bool| {
                plan(
                    &project(settings),
                    &servers(servers_count),
                    NETWORK,
                    "production",
                    APP,
                    records,
                    admin,
                    false,
                    true,
                )
            };
        // One server: the deploy creates the generated database.
        let first = place("", 1, &[], true).unwrap();
        assert_eq!(
            first.workflows.binding.as_ref().unwrap().database_name,
            name
        );
        assert!(first.workflows.implicit && first.create.contains_key(&name));
        assert!(first.replay.binding.is_none());
        // It is reused only while the directory names this app as owner.
        let owned = [workflows_record(&name, Some(APP))];
        assert!(place("", 1, &owned, true).unwrap().create.is_empty());
        assert!(
            place(
                "",
                1,
                &[workflows_record(&name, Some("other/production"))],
                true
            )
            .err()
            .unwrap()
            .contains("automatically created workflow runs database")
        );
        // A database created for workflows is never app SQL.
        assert!(
            place(
                "db={main='runs'}",
                1,
                &[workflows_record("runs", Some(APP))],
                true
            )
            .err()
            .unwrap()
            .contains("created for workflow runs")
        );
        // The selected target coordinates both generated and explicitly named groups.
        assert_eq!(
            place("", 2, &[], true).unwrap().create[&name].home.name,
            "west"
        );
        let mut runs = record("runs", None);
        runs.server = "b".repeat(64);
        let named = place(
            "[envs.production.workflows]\ndb='runs'",
            2,
            &[runs.clone()],
            true,
        )
        .unwrap();
        assert_eq!(named.workflows.binding.unwrap().home.name, "west");
        // Channels and workflows may share a named database.
        let shared = place(
            "[envs.production.channels]\ndb='runs'\n[envs.production.workflows]\ndb='runs'",
            2,
            &[runs],
            true,
        )
        .unwrap();
        assert_eq!(shared.replay.binding.unwrap().database_name, "runs");
        assert_eq!(shared.workflows.binding.unwrap().database_name, "runs");
        // A deploy-only client asserts only a database named in tako.toml.
        let scoped = place("", 1, &owned, false).unwrap();
        assert!(scoped.workflows.binding.is_none() && !scoped.workflows.unprovisioned);
        assert!(place("", 1, &[], false).unwrap().workflows.unprovisioned);
    }
}
