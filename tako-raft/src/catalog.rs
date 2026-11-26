//! Name claims use one Raft group per network. A successful retry returns the
//! original bootstrap, even when the two deploys selected disjoint servers.
use crate::{
    Applied,
    disk::{RecordReader, Records, error, lookup, put},
};
use redb::{TableDefinition, WriteTransaction};
pub(crate) const NAMES: Records = TableDefinition::new("resource_names");
pub(crate) const GENERATIONS: Records = TableDefinition::new("used_generations");
pub(crate) const RETIREMENTS: Records = TableDefinition::new("retirements");
use std::io;

pub use tako_core::StorageClaim as Claim;

pub(crate) fn claim(db: &WriteTransaction, candidate: Claim) -> io::Result<Applied> {
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
    if let Some(owner) = lookup::<Claim>(db, NAMES, &candidate.name)? {
        if retirement(db, &owner.generation)?.is_some() {
            return Ok(Applied::Invalid);
        }
        if owner.engine != candidate.engine {
            return Ok(Applied::EngineConflict);
        }
        return Ok(Applied::Claimed(owner));
    }
    if lookup::<()>(db, GENERATIONS, &candidate.generation)?.is_some() {
        return Ok(Applied::Invalid);
    }
    put(db, GENERATIONS, &candidate.generation, &())?;
    put(db, NAMES, &candidate.name, &candidate)?;
    Ok(Applied::Claimed(candidate))
}

pub(crate) fn retire(db: &WriteTransaction, name: &str, generation: &str) -> io::Result<Applied> {
    if retirement(db, generation)?.is_none() {
        return Ok(Applied::Invalid);
    }
    if lookup::<Claim>(db, NAMES, name)?.is_some_and(|claim| claim.generation == generation) {
        db.open_table(NAMES)
            .map_err(error)?
            .remove(name)
            .map_err(error)?;
    }
    Ok(Applied::NoChange)
}

pub(crate) fn retirement(
    db: &impl RecordReader,
    generation: &str,
) -> io::Result<Option<std::collections::BTreeSet<String>>> {
    lookup(db, RETIREMENTS, generation)
}
pub(crate) fn begin_retirement(
    db: &WriteTransaction,
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
    put(db, RETIREMENTS, generation, &targets)?;
    Ok(Applied::NoChange)
}
