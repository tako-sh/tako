//! Name claims use one Raft group per network. A successful retry returns the
//! original bootstrap, even when the two deploys selected disjoint servers.
use crate::{
    Applied,
    disk::{decode, encode, error},
};
use rusqlite::{Connection, OptionalExtension};
use std::io;

pub use tako_core::StorageClaim as Claim;

pub(crate) fn claim(db: &Connection, candidate: Claim) -> io::Result<Applied> {
    if tako_core::validate_database_name(&candidate.name).is_err()
        || tako_core::validate_database_generation(&candidate.generation).is_err()
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
        if retirement(db, &owner.generation)?.is_some() {
            return Ok(Applied::Invalid);
        }
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
CREATE TABLE IF NOT EXISTS retirements(id TEXT PRIMARY KEY,targets BLOB NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS used_generations(id TEXT PRIMARY KEY) WITHOUT ROWID;";

pub(crate) fn retire(db: &Connection, name: &str, generation: &str) -> io::Result<Applied> {
    let data: Option<Vec<u8>> = db
        .query_row(
            "SELECT claim FROM resource_names WHERE name=?1",
            [name],
            |r| r.get(0),
        )
        .optional()
        .map_err(error)?;
    if retirement(db, generation)?.is_none() {
        return Ok(Applied::Invalid);
    }
    if let Some(data) = data
        && decode::<Claim>(&data)?.generation == generation
    {
        db.execute("DELETE FROM resource_names WHERE name=?1", [name])
            .map_err(error)?;
    }
    Ok(Applied::NoChange)
}

pub(crate) fn retirement(
    db: &Connection,
    generation: &str,
) -> io::Result<Option<std::collections::BTreeSet<String>>> {
    let bytes: Option<Vec<u8>> = db
        .query_row(
            "SELECT targets FROM retirements WHERE id=?1",
            [generation],
            |r| r.get(0),
        )
        .optional()
        .map_err(error)?;
    bytes.map(|d| decode(&d)).transpose()
}
pub(crate) fn begin_retirement(
    db: &Connection,
    generation: &str,
    targets: std::collections::BTreeSet<String>,
) -> io::Result<Applied> {
    if let Some(stored) = retirement(db, generation)? {
        return Ok(if stored == targets {
            Applied::NoChange
        } else {
            Applied::Invalid
        });
    }
    db.execute(
        "INSERT INTO retirements VALUES(?1,?2)",
        (generation, encode(&targets)?),
    )
    .map_err(error)?;
    Ok(Applied::NoChange)
}
