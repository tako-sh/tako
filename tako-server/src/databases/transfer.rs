//! Importing a SQLite file into a database, and exporting a consistent copy,
//! in chunks over the management protocol.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use sha2::{Digest, Sha256};
use tako_core::{DATABASE_TRANSFER_CHUNK_BYTES, DatabaseExport};
use tako_database::DatabaseId;

use super::snapshots::{self, WorkDir, fresh_work_dir, sha256_file};
use crate::ServerState;

/// Transfers idle for longer are abandoned and their files removed.
const TRANSFER_IDLE: Duration = Duration::from_secs(60 * 60);
/// The largest database file accepted by an import.
const MAX_IMPORT_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const DISK_RESERVE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Default)]
pub(crate) struct Transfers {
    imports: HashMap<String, Import>,
    exports: HashMap<String, Export>,
}

struct Import {
    database: String,
    work: WorkDir,
    file: std::fs::File,
    size: u64,
    sha256: String,
    received: u64,
    hasher: Sha256,
    touched: Instant,
}

struct Export {
    database: String,
    work: WorkDir,
    path: PathBuf,
    size: u64,
    touched: Instant,
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

fn transfer_id() -> String {
    nanoid::nanoid!(24, &nanoid::alphabet::SAFE[..])
}

pub(crate) async fn begin_import(
    state: &ServerState,
    id: &str,
    size: u64,
    sha256: &str,
) -> Result<serde_json::Value, String> {
    state.hosted_database(id)?;
    if size == 0 || size > MAX_IMPORT_BYTES {
        return Err(format!(
            "An import must be between 1 byte and {MAX_IMPORT_BYTES} bytes."
        ));
    }
    if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("Import checksum must be a SHA-256 hex digest.".into());
    }
    let work = fresh_work_dir(state, id, "import")?;
    let available = crate::operations::available_bytes(&work.path)?;
    if available < size.saturating_mul(2).saturating_add(DISK_RESERVE_BYTES) {
        return Err(format!(
            "Not enough free disk space for this import ({available} bytes available)."
        ));
    }
    let path = work.path.join("import.sqlite");
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
            work,
            file,
            size,
            sha256: sha256.to_ascii_lowercase(),
            received: 0,
            hasher: Sha256::new(),
            touched: Instant::now(),
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
    Ok(serde_json::json!({"received": import.received}))
}

pub(crate) async fn finish_import(
    state: &ServerState,
    id: &str,
    upload_id: &str,
) -> Result<serde_json::Value, String> {
    let import = state
        .databases
        .transfers()
        .imports
        .remove(upload_id)
        .filter(|import| import.database == id)
        .ok_or("Import not found or expired.")?;
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
    let path = import.work.path.join("import.sqlite");
    let verify = path.clone();
    tokio::task::spawn_blocking(move || tako_database::verify_database(&verify))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("The uploaded file is not a usable SQLite database: {e}"))?;

    let stored = state.hosted_database(id)?;
    let warnings = snapshots::replace(
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
    state.hosted_database(id)?;
    let database = DatabaseId::parse(id)?;
    let lock = state.databases.lock(id);
    let _guard = lock.lock().await;
    let work = fresh_work_dir(state, id, "export")?;
    let size = state.databases.host.size_bytes(&database).unwrap_or(0);
    let available = crate::operations::available_bytes(&work.path)?;
    if available < size.saturating_add(DISK_RESERVE_BYTES) {
        return Err(format!(
            "Not enough free disk space for an export ({available} bytes available)."
        ));
    }
    let path = work.path.join("export.sqlite");
    // A snapshot copy marks the database unchanged; restore that afterwards
    // so the scheduler still uploads the changes.
    let changed = state.databases.host.has_changes(&database);
    let snapshot = state.databases.host.snapshot(&database, &path).await;
    if changed {
        state.databases.host.mark_changed(&database);
    }
    snapshot.map_err(|e| format!("copy database: {e}"))?;
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
    let removed = state
        .databases
        .transfers()
        .exports
        .remove(export_id)
        .filter(|export| export.database == id);
    if let Some(export) = removed {
        drop(export.work);
    }
    Ok(serde_json::json!({"finished": export_id}))
}
