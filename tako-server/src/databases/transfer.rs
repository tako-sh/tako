//! Importing a SQLite file (or a KV export) into a database, and exporting
//! a consistent copy, in chunks over the management protocol.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use sha2::{Digest, Sha256};
use tako_core::{DATABASE_TRANSFER_CHUNK_BYTES, DatabaseExport};

use super::backups::{self, SpaceReservation, WorkDir, fresh_work_dir, sha256_file};
use super::snapshots;
use crate::ServerState;

/// Transfers idle for longer are abandoned and their files removed.
const TRANSFER_IDLE: Duration = Duration::from_secs(60 * 60);
/// The largest database file accepted by an import.
const MAX_IMPORT_BYTES: u64 = 64 * 1024 * 1024 * 1024;

#[derive(Default)]
pub(crate) struct Transfers {
    imports: HashMap<String, Import>,
    exports: HashMap<String, Export>,
}

struct Import {
    database: String,
    path: PathBuf,
    /// The generation the import was started for; a database deleted and
    /// created again under the same name does not receive it.
    generation: String,
    work: WorkDir,
    file: std::fs::File,
    size: u64,
    sha256: String,
    received: u64,
    hasher: Sha256,
    touched: Instant,
    /// Disk space held for the upload, in the ledger snapshots and restores
    /// share. Last, so it is released after the file and its directory.
    _space: SpaceReservation,
}

struct Export {
    database: String,
    work: WorkDir,
    path: PathBuf,
    size: u64,
    touched: Instant,
    _space: SpaceReservation,
}

pub(crate) fn expire(state: &ServerState) {
    let mut transfers = state.databases.transfers();
    transfers
        .imports
        .retain(|_, import| import.touched.elapsed() < TRANSFER_IDLE);
    transfers
        .exports
        .retain(|_, export| export.touched.elapsed() < TRANSFER_IDLE);
}

/// Ends a database's imports and exports, removing their files, for a caller
/// that holds the database's lock and is about to delete it. A chunk that
/// arrives afterwards finds no transfer.
pub(crate) fn cancel(state: &ServerState, id: &str) {
    let (imports, exports) = {
        let mut transfers = state.databases.transfers();
        let imports: Vec<String> = transfers
            .imports
            .iter()
            .filter(|(_, import)| import.database == id)
            .map(|(key, _)| key.clone())
            .collect();
        let exports: Vec<String> = transfers
            .exports
            .iter()
            .filter(|(_, export)| export.database == id)
            .map(|(key, _)| key.clone())
            .collect();
        (
            imports
                .iter()
                .filter_map(|key| transfers.imports.remove(key))
                .collect::<Vec<_>>(),
            exports
                .iter()
                .filter_map(|key| transfers.exports.remove(key))
                .collect::<Vec<_>>(),
        )
    };
    // Closing the files and removing their directories happens outside the
    // transfer table's lock.
    drop(imports);
    drop(exports);
}

fn transfer_id() -> String {
    nanoid::nanoid!(24, &nanoid::alphabet::SAFE[..])
}

pub(crate) async fn begin_import(
    state: &ServerState,
    id: &str,
    size: u64,
    sha256: &str,
) -> Result<serde_json::Value, String> {
    if size == 0 || size > MAX_IMPORT_BYTES {
        return Err(format!(
            "An import must be between 1 byte and {MAX_IMPORT_BYTES} bytes."
        ));
    }
    if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("Import checksum must be a SHA-256 hex digest.".into());
    }
    // Held until the import is registered, so a delete either cancels it or
    // has already removed the database.
    let lock = state.databases.lock(id);
    let _guard = lock.lock().await;
    let stored = state.hosted_database(id)?;
    let work = fresh_work_dir(state, id, "import")?;
    let available = crate::operations::available_bytes(&work.path)?;
    // Held while the upload lasts, so imports started together cannot each
    // count the same free space.
    let space = state
        .databases
        .snapshot_space
        .reserve(size.saturating_mul(2), available, "the import")
        .map_err(|error| format!("Cannot import into {id}: {error}."))?;
    let path = work
        .path
        .join(format!("import.{}", snapshots::extension(&stored)));
    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .map_err(|e| format!("create import file: {e}"))?;
    let upload_id = transfer_id();
    state.databases.transfers().imports.insert(
        upload_id.clone(),
        Import {
            database: id.to_owned(),
            path,
            generation: stored.generation,
            work,
            file,
            size,
            sha256: sha256.to_ascii_lowercase(),
            received: 0,
            hasher: Sha256::new(),
            touched: Instant::now(),
            _space: space,
        },
    );
    Ok(serde_json::json!({
        "upload_id": upload_id,
        "chunk_bytes": DATABASE_TRANSFER_CHUNK_BYTES,
    }))
}

pub(crate) async fn import_chunk(
    state: &ServerState,
    id: &str,
    upload_id: &str,
    offset: u64,
    data_base64: &str,
) -> Result<serde_json::Value, String> {
    let data = BASE64
        .decode(data_base64)
        .map_err(|_| "Import chunk is not valid base64.".to_string())?;
    if data.is_empty() || data.len() as u64 > DATABASE_TRANSFER_CHUNK_BYTES {
        return Err(format!(
            "Import chunks must hold 1 to {DATABASE_TRANSFER_CHUNK_BYTES} bytes."
        ));
    }
    let mut transfers = state.databases.transfers();
    let import = transfers
        .imports
        .get_mut(upload_id)
        .filter(|import| import.database == id)
        .ok_or("Import not found or expired.")?;
    if offset != import.received {
        return Err(format!(
            "Import chunk starts at {offset}, but {} bytes were received.",
            import.received
        ));
    }
    if import.received + data.len() as u64 > import.size {
        return Err("Import is larger than announced.".into());
    }
    import
        .file
        .write_all(&data)
        .map_err(|e| format!("write import: {e}"))?;
    import.hasher.update(&data);
    import.received += data.len() as u64;
    import.touched = Instant::now();
    // A transfer stays alive while chunks arrive; so does its directory.
    import.work.touch();
    Ok(serde_json::json!({"received": import.received}))
}

pub(crate) async fn finish_import(
    state: &ServerState,
    id: &str,
    upload_id: &str,
) -> Result<serde_json::Value, String> {
    // Checked before removing, so naming the wrong database leaves the
    // upload in place.
    let import = {
        let mut transfers = state.databases.transfers();
        if !transfers
            .imports
            .get(upload_id)
            .is_some_and(|import| import.database == id)
        {
            return Err("Import not found or expired.".into());
        }
        transfers
            .imports
            .remove(upload_id)
            .ok_or("Import not found or expired.")?
    };
    if import.received != import.size {
        return Err(format!(
            "Import is incomplete: {} of {} bytes received.",
            import.received, import.size
        ));
    }
    if hex::encode(import.hasher.clone().finalize()) != import.sha256 {
        return Err("Import does not match its checksum.".into());
    }
    import
        .file
        .sync_all()
        .map_err(|e| format!("flush import: {e}"))?;
    let path = import.path.clone();
    let engine = state.hosted_database(id)?.engine;
    let verify = path.clone();
    tokio::task::spawn_blocking(move || snapshots::verify_copy(engine, &verify))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| match engine {
            tako_core::DatabaseEngine::Turso => {
                format!("The uploaded file is not a usable SQLite database: {e}")
            }
            tako_core::DatabaseEngine::Kv => {
                format!("The uploaded file is not a usable KV store export: {e}")
            }
        })?;

    let lock = state.databases.lock(id);
    let _guard = lock.lock().await;
    let stored = state.hosted_database(id)?;
    if stored.engine != engine {
        return Err(format!(
            "Database {id} was replaced by a new database of the same name during the import; nothing was imported."
        ));
    }
    if stored.generation != import.generation {
        return Err(format!(
            "Database {id} was replaced by a new database of the same name during the import; nothing was imported."
        ));
    }
    let warnings = backups::replace_locked(
        state,
        &stored,
        path,
        "database is being replaced by an import",
    )
    .await?;
    drop(import.work);
    tracing::info!(database = %id, bytes = import.size, "Imported database");
    Ok(serde_json::json!({
        "database": id,
        "name": stored.name,
        "size_bytes": import.size,
        "warnings": warnings,
    }))
}

pub(crate) async fn begin_export(
    state: &ServerState,
    id: &str,
) -> Result<serde_json::Value, String> {
    // Held until the export is registered, so a delete either cancels it or
    // has already removed the database.
    let lock = state.databases.lock(id);
    let _guard = lock.lock().await;
    let stored = state.hosted_database(id)?;
    let work = fresh_work_dir(state, id, "export")?;
    let size = snapshots::size_bytes(state, &stored);
    let available = crate::operations::available_bytes(&work.path)?;
    let space = state
        .databases
        .snapshot_space
        .reserve(size, available, "the export")
        .map_err(|error| format!("Cannot export {id}: {error}."))?;
    let path = work
        .path
        .join(format!("export.{}", snapshots::extension(&stored)));
    snapshots::copy_contents(state, &stored, &path, false).await?;
    let hash_path = path.clone();
    let (sha256, size) = tokio::task::spawn_blocking(move || sha256_file(&hash_path))
        .await
        .map_err(|e| e.to_string())??;
    let export_id = transfer_id();
    state.databases.transfers().exports.insert(
        export_id.clone(),
        Export {
            database: id.to_owned(),
            work,
            path,
            size,
            touched: Instant::now(),
            _space: space,
        },
    );
    Ok(serde_json::to_value(DatabaseExport {
        export_id,
        size_bytes: size,
        sha256,
    })
    .expect("export serializes"))
}

pub(crate) async fn export_chunk(
    state: &ServerState,
    id: &str,
    export_id: &str,
    offset: u64,
) -> Result<serde_json::Value, String> {
    let (path, size) = {
        let mut transfers = state.databases.transfers();
        let export = transfers
            .exports
            .get_mut(export_id)
            .filter(|export| export.database == id)
            .ok_or("Export not found or expired.")?;
        export.touched = Instant::now();
        export.work.touch();
        (export.path.clone(), export.size)
    };
    if offset >= size {
        return Err(format!(
            "Export offset {offset} is beyond its {size} bytes."
        ));
    }
    let data = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
        let mut file = std::fs::File::open(&path).map_err(|e| format!("open export: {e}"))?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| format!("read export: {e}"))?;
        let mut data = vec![0u8; DATABASE_TRANSFER_CHUNK_BYTES.min(size - offset) as usize];
        file.read_exact(&mut data)
            .map_err(|e| format!("read export: {e}"))?;
        Ok(data)
    })
    .await
    .map_err(|e| e.to_string())??;
    Ok(serde_json::json!({"offset": offset, "data_base64": BASE64.encode(data)}))
}

pub(crate) fn finish_export(
    state: &ServerState,
    id: &str,
    export_id: &str,
) -> Result<serde_json::Value, String> {
    let removed = {
        let mut transfers = state.databases.transfers();
        if !transfers
            .exports
            .get(export_id)
            .is_some_and(|export| export.database == id)
        {
            return Err("Export not found or expired.".into());
        }
        transfers.exports.remove(export_id)
    };
    if let Some(export) = removed {
        drop(export.work);
    }
    Ok(serde_json::json!({"finished": export_id}))
}
