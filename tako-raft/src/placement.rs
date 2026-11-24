//! Deploy reservations stay in the resource log through the rollout. A sweep
//! can release only the reservation it inspected and only after confirmation.
use crate::{
    Applied,
    disk::{get, set},
};
use rusqlite::Connection;
use std::io;
use tako_core::{PlacementChange, ResourcePlacement};

pub(crate) fn load(db: &Connection) -> io::Result<ResourcePlacement> {
    Ok(get(db, "placement")?.unwrap_or_default())
}

pub(crate) fn apply(
    db: &Connection,
    change: PlacementChange,
    revision: u64,
) -> io::Result<Applied> {
    let mut placement = load(db)?;
    match change {
        PlacementChange::Reserve {
            app,
            servers,
            founders,
            ticket,
        } => {
            if placement.retired
                || servers.is_empty()
                || servers.iter().any(|s| placement.removing.contains(s))
            {
                return Ok(Applied::Invalid);
            }
            if placement.revision == 0 {
                placement.known.extend(founders);
            }
            for server in servers {
                placement.known.insert(server.clone());
                placement.uses.entry(server).or_default().insert(
                    app.clone(),
                    tako_core::StorageUse {
                        ticket: ticket.clone(),
                        pending: true,
                    },
                );
            }
        }
        PlacementChange::Confirm {
            app,
            server,
            ticket,
        } => {
            let Some(usage) = placement
                .uses
                .get_mut(&server)
                .and_then(|apps| apps.get_mut(&app))
            else {
                return Ok(Applied::Invalid);
            };
            if usage.ticket != ticket {
                return Ok(Applied::Invalid);
            }
            usage.pending = false;
        }
        PlacementChange::Release {
            app,
            server,
            ticket,
        } => {
            let Some(apps) = placement.uses.get_mut(&server) else {
                return Ok(Applied::Invalid);
            };
            if !apps
                .get(&app)
                .is_some_and(|u| u.ticket == ticket && !u.pending)
            {
                return Ok(Applied::Invalid);
            }
            apps.remove(&app);
            if apps.is_empty() {
                placement.uses.remove(&server);
                placement.removing.insert(server);
            }
            if placement.uses.is_empty() {
                placement.retired = true;
                placement.removing = placement.known.clone();
            }
        }
        PlacementChange::Removed { server } => {
            if placement.retired {
                return Ok(Applied::Invalid);
            }
            placement.removing.remove(&server);
            placement.known.remove(&server);
        }
    }
    placement.revision = revision;
    set(db, "placement", &placement)?;
    Ok(Applied::Placement(placement))
}
