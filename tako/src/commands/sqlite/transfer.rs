//! Importing a local SQLite file into a database, and exporting a copy.

use std::io::{Read, Write};
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use tako_core::{Command, DATABASE_TRANSFER_CHUNK_BYTES, DatabaseExport, Response};

use super::target::Home;
use crate::management_http::ManagementClient;
use crate::output;

const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";

async fn send<T: DeserializeOwned>(
    client: &mut ManagementClient,
    command: Command,
) -> Result<T, Box<dyn std::error::Error>> {
    match client.send(&command).await? {
        Response::Ok { data } => Ok(serde_json::from_value(data)?),
        Response::Error { message, .. } => Err(message.into()),
    }
}

fn sha256_file(path: &Path) -> std::io::Result<(String, u64)> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        total += read as u64;
    }
    Ok((hex::encode(hasher.finalize()), total))
}

pub(super) async fn import(
    home: &Home,
    name: &str,
    file: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut header = [0u8; 16];
    std::fs::File::open(file)
        .and_then(|mut f| f.read_exact(&mut header))
        .map_err(|e| format!("Read {}: {e}", file.display()))?;
    if &header != SQLITE_HEADER {
        return Err(format!("{} is not a SQLite database file.", file.display()).into());
    }
    let path = file.to_path_buf();
    let (sha256, size) = tokio::task::spawn_blocking(move || sha256_file(&path)).await??;
    let mut client = ManagementClient::for_server(&home.entry).await?;
    let _t = output::timed(&format!("Import {name} ({size} bytes)"));
    let response: serde_json::Value = output::with_spinner_async(
        &format!("Uploading {} to {name}", output::format_size(size)),
        &format!("Imported {} into {name}", output::format_size(size)),
        async {
            let begin: serde_json::Value = send(
                &mut client,
                Command::BeginDatabaseImport {
                    name: name.to_owned(),
                    size_bytes: size,
                    sha256: sha256.clone(),
                },
            )
            .await
            .map_err(|e| e.to_string())?;
            let upload_id = begin["upload_id"]
                .as_str()
                .ok_or("invalid import response")?
                .to_owned();
            let mut source = std::fs::File::open(file).map_err(|e| e.to_string())?;
            let mut offset = 0u64;
            let mut chunk = vec![0u8; DATABASE_TRANSFER_CHUNK_BYTES as usize];
            while offset < size {
                let want = (size - offset).min(DATABASE_TRANSFER_CHUNK_BYTES) as usize;
                source
                    .read_exact(&mut chunk[..want])
                    .map_err(|e| e.to_string())?;
                let _: serde_json::Value = send(
                    &mut client,
                    Command::DatabaseImportChunk {
                        name: name.to_owned(),
                        upload_id: upload_id.clone(),
                        offset,
                        data_base64: BASE64.encode(&chunk[..want]),
                    },
                )
                .await
                .map_err(|e| e.to_string())?;
                offset += want as u64;
            }
            send(
                &mut client,
                Command::FinishDatabaseImport {
                    name: name.to_owned(),
                    upload_id,
                },
            )
            .await
            .map_err(|e| e.to_string())
        },
    )
    .await?;
    for warning in response["warnings"].as_array().into_iter().flatten() {
        if let Some(warning) = warning.as_str() {
            output::warning(warning);
        }
    }
    if output::is_json() {
        return output::json_result(
            serde_json::json!({"ok": true, "command": "sqlite", "imported": {"database": name, "size_bytes": size, "sha256": sha256}}),
        );
    }
    Ok(())
}

pub(super) async fn export(
    home: &Home,
    name: &str,
    file: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    if file.exists() {
        return Err(format!("{} already exists.", file.display()).into());
    }
    // A new, uniquely named file next to the destination, removed unless the
    // export completes; nothing that already exists is touched.
    let directory = match file.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let file_name = file
        .file_name()
        .ok_or_else(|| format!("{} is not a file path.", file.display()))?;
    let partial = tempfile::Builder::new()
        .prefix(&format!(".{}.", file_name.to_string_lossy()))
        .suffix(".partial")
        .tempfile_in(directory)
        .map_err(|e| format!("Create a file in {}: {e}", directory.display()))?;
    let mut client = ManagementClient::for_server(&home.entry).await?;
    let _t = output::timed(&format!("Export {name}"));
    let export: DatabaseExport = output::with_spinner_async(
        &format!("Exporting {name}"),
        &format!("Exported {name} to {}", file.display()),
        async {
            let export: DatabaseExport = send(
                &mut client,
                Command::BeginDatabaseExport {
                    name: name.to_owned(),
                },
            )
            .await
            .map_err(|e| e.to_string())?;
            let result = async {
                let mut out = partial;
                let mut hasher = Sha256::new();
                let mut offset = 0u64;
                while offset < export.size_bytes {
                    let chunk: serde_json::Value = send(
                        &mut client,
                        Command::DatabaseExportChunk {
                            name: name.to_owned(),
                            export_id: export.export_id.clone(),
                            offset,
                        },
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                    let data = BASE64
                        .decode(
                            chunk["data_base64"]
                                .as_str()
                                .ok_or("invalid export chunk")?,
                        )
                        .map_err(|e| e.to_string())?;
                    if data.is_empty() {
                        return Err("the export ended early".to_string());
                    }
                    hasher.update(&data);
                    out.write_all(&data).map_err(|e| e.to_string())?;
                    offset += data.len() as u64;
                }
                out.as_file().sync_all().map_err(|e| e.to_string())?;
                if hex::encode(hasher.finalize()) != export.sha256 {
                    return Err("the downloaded copy does not match its checksum".to_string());
                }
                out.persist_noclobber(file)
                    .map(drop)
                    .map_err(|e| match e.error.kind() {
                        std::io::ErrorKind::AlreadyExists => {
                            format!("{} already exists", file.display())
                        }
                        _ => e.error.to_string(),
                    })
            }
            .await;
            let _: Result<serde_json::Value, _> = send(
                &mut client,
                Command::FinishDatabaseExport {
                    name: name.to_owned(),
                    export_id: export.export_id.clone(),
                },
            )
            .await;
            result.map(|()| export)
        },
    )
    .await?;
    if output::is_json() {
        return output::json_result(serde_json::json!({
            "ok": true, "command": "sqlite",
            "exported": {"database": name, "path": file, "size_bytes": export.size_bytes, "sha256": export.sha256},
        }));
    }
    Ok(())
}
