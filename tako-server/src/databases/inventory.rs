//! Management status exposes resource identities and binding aliases. Credentials stay server-side.
use super::*;
use std::collections::BTreeMap;
use tako_core::{
    DatabaseReplicaStatus, ResourceBindingStatus, ServerUserResources, StorageBindingStatus,
};

impl ServerState {
    pub(crate) fn user_resources(&self, app: Option<&str>) -> Result<ServerUserResources, String> {
        let mut databases = BTreeMap::new();
        if app.is_none() {
            for db in self
                .state_store
                .list_databases()
                .map_err(|e| e.to_string())?
            {
                databases.insert((db.name, db.generation), (db.engine, BTreeMap::new()));
            }
        }
        self.visit_persisted_database_bindings(app, |scope, set, retained| {
            for (alias, entry) in set {
                let db = &entry.binding;
                let (_, uses) = databases
                    .entry((db.name.clone(), db.generation.clone()))
                    .or_insert_with(|| (db.engine, BTreeMap::new()));
                uses.entry((scope.to_owned(), alias.clone()))
                    .and_modify(|was_retained| *was_retained &= retained)
                    .or_insert(retained);
            }
            Ok(())
        })?;
        let proxy = self.databases.proxy.read().clone();
        let replicas = databases
            .into_iter()
            .map(|((name, generation), (engine, uses))| {
                let election = self
                    .databases
                    .raft
                    .resource(&generation)
                    .and_then(|resource| resource.leadership());
                let leader = election.map(|(id, _)| {
                    let address = std::net::Ipv6Addr::from(id);
                    proxy
                        .as_ref()
                        .and_then(|proxy| proxy.server_name_at(address))
                        .unwrap_or_else(|| address.to_string())
                });
                DatabaseReplicaStatus {
                    name,
                    engine,
                    generation,
                    leader,
                    is_leader: election.map(|(_, local)| local),
                    bindings: uses
                        .into_iter()
                        .map(|((app, binding), retained)| ResourceBindingStatus {
                            app,
                            binding,
                            retained,
                        })
                        .collect(),
                }
            })
            .collect();
        let stores = match app {
            Some(app) => BTreeMap::from([(
                app.to_owned(),
                self.state_store
                    .get_storages(app)
                    .map_err(|e| e.to_string())?,
            )]),
            None => self
                .state_store
                .all_app_storages()
                .map_err(|e| e.to_string())?,
        };
        let mut storages = Vec::new();
        for (app, set) in stores {
            for (binding, store) in set {
                let resource = store
                    .resource
                    .or_else(|| {
                        store
                            .path
                            .as_deref()
                            .and_then(|path| path.strip_prefix("storage/"))
                            .map(str::to_owned)
                    })
                    .unwrap_or_else(|| binding.clone());
                storages.push(StorageBindingStatus {
                    app: app.clone(),
                    binding,
                    resource,
                    provider: store.provider,
                });
            }
        }
        storages.sort_by(|a, b| (&a.app, &a.binding).cmp(&(&b.app, &b.binding)));
        Ok(ServerUserResources {
            databases: replicas,
            storages,
        })
    }
}
