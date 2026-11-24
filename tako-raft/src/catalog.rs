//! Name claims use one Raft group per network. A successful retry returns the
//! original bootstrap, even when the two deploys selected disjoint servers.
use crate::{
    Applied,
    disk::{decode, encode, error},
};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, io};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub name: String,
    pub engine: tako_core::DatabaseEngine,
    pub generation: String,
    /// Immutable device IDs in the initial voting membership. A deployment
    /// joining later goes through membership change on this resource.
    pub founders: BTreeSet<String>,
}

pub(crate) fn claim(db: &Connection, candidate: Claim) -> io::Result<Applied> {
    if tako_core::validate_database_name(&candidate.name).is_err()
        || candidate.generation.len() != 32
        || !candidate.generation.bytes().all(|b| b.is_ascii_hexdigit())
        || candidate.founders.is_empty()
        || candidate.founders.len() > 3
        || candidate
            .founders
            .iter()
            .any(|id| id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Ok(Applied::Invalid);
    }
    let stored: Option<Vec<u8>> = db
        .query_row(
            "SELECT claim FROM resource_names WHERE name=?1",
            [&candidate.name],
            |row| row.get(0),
        )
        .optional()
        .map_err(error)?;
    if let Some(stored) = stored {
        let owner: Claim = decode(&stored)?;
        if owner.engine != candidate.engine {
            return Ok(Applied::EngineConflict);
        }
        return Ok(Applied::Claimed(owner));
    }
    let used: bool = db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM used_generations WHERE id=?1)",
            [&candidate.generation],
            |row| row.get(0),
        )
        .map_err(error)?;
    if used {
        return Ok(Applied::Invalid);
    }
    db.execute(
        "INSERT INTO used_generations VALUES (?1)",
        [&candidate.generation],
    )
    .map_err(error)?;
    db.execute(
        "INSERT INTO resource_names VALUES (?1,?2)",
        (&candidate.name, encode(&candidate)?),
    )
    .map_err(error)?;
    Ok(Applied::Claimed(candidate))
}

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS resource_names(name TEXT PRIMARY KEY,claim BLOB NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS used_generations(id TEXT PRIMARY KEY) WITHOUT ROWID;";
