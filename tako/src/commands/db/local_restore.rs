use super::{dry_run, find_record_of, project_context};
use crate::{commands::env_network, config::TakoToml, network::databases, output};
use base64::{Engine, engine::general_purpose::STANDARD};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{fs, path::Path, sync::Arc};
use tako_backup::{Replica, Target};
use tako_core::DatabaseEngine;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub(super) async fn run(
    config_path: Option<&Path>,
    name: &str,
    env: &str,
    generation: Option<String>,
    target: Target,
    output_file: &Path,
) -> Result<()> {
    tako_core::validate_database_name(name)?;
    let project = project_context::resolve_existing(config_path)?;
    let config = TakoToml::load_from_file(&project.config_path)?;
    let alias = env_network::resolve(&project.config_path, env, env_network::Purpose::Use)?;
    if !crate::commands::helpers::is_full_client(&alias.id)? {
        return Err("Restoring a database backup requires full access".into());
    }
    if output_file.try_exists()? {
        return Err(format!("{} already exists", output_file.display()).into());
    }
    let generation = match generation {
        Some(id) => id,
        None => {
            let records = databases::list_from(&alias, true)?;
            find_record_of(&records, name, DatabaseEngine::Sqlite)?
                .generation
                .clone()
        }
    };
    tako_core::validate_database_generation(&generation)?;
    if output::is_dry_run() {
        return dry_run(
            &format!("Restore {name} to {}", output_file.display()),
            serde_json::json!({"database": name, "generation": generation, "path": output_file}),
        );
    }
    let secrets = project.load_secrets_for_environment(Some(env))?;
    let storage = crate::commands::storage::decrypt_database_backup_storage(
        env,
        &config,
        name,
        &secrets,
        Some(&project.project_dir),
    )?
    .ok_or_else(|| format!("Configure backup storage for '{name}' or environment '{env}'"))?;
    let encoded = databases::generation_key(&alias, name, &generation, true)?;
    let mut bytes = zeroize::Zeroizing::new([0u8; 32]);
    let len = STANDARD
        .decode_slice(encoded.as_bytes(), bytes.as_mut_slice())
        .map_err(|_| "Invalid database recovery key")?;
    if len != bytes.len() {
        return Err("Invalid database recovery key length".into());
    }
    let replica = Replica::database(
        Arc::new(tako_storage::s3(&storage)?),
        name,
        &generation,
        *bytes,
    )?;
    let txid = output::with_spinner_async(
        &format!("Restoring {name} to {}", output_file.display()),
        &format!("Restored {name} to {}", output_file.display()),
        async {
            recover(&replica, target, output_file)
                .await
                .map_err(|e| e.to_string())
        },
    )
    .await?;
    if output::is_json() {
        return output::json_result(serde_json::json!({
            "ok": true, "command": "db", "database": name, "generation": generation,
            "transaction": txid, "path": output_file,
        }));
    }
    Ok(())
}

async fn recover(replica: &Replica, target: Target, output_file: &Path) -> Result<u64> {
    let parent = output_file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut scratch = tempfile::Builder::new();
    scratch.prefix(".tako-restore-");
    #[cfg(unix)]
    scratch.permissions(fs::Permissions::from_mode(0o700));
    let work = scratch.tempdir_in(parent)?;
    let recovered = work.path().join("recovered.sqlite");
    let segments = replica.segments().await?;
    let txid = Replica::resolve(&segments, target)?;
    replica
        .restore(&segments, txid, &recovered, &work.path().join("segments"))
        .await?;
    let output_file = output_file.to_owned();
    tokio::task::spawn_blocking(move || -> std::result::Result<(), String> {
        // A cancelled request must leave the scratch files alive until verification finishes.
        let _work = work;
        tako_db::materialize_recovery_image(&recovered)?;
        tako_db::verify_database(&recovered)?;
        #[cfg(unix)]
        {
            fs::set_permissions(&recovered, fs::Permissions::from_mode(0o600))
                .map_err(|e| e.to_string())?;
        }
        fs::File::open(&recovered)
            .and_then(|file| file.sync_all())
            .map_err(|e| e.to_string())?;
        fs::hard_link(&recovered, &output_file)
            .map_err(|e| format!("Cannot publish {}: {e}", output_file.display()))
    })
    .await??;
    Ok(txid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn recover_time_to_a_private_file_without_contacting_a_resource_group() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.sqlite");
        fs::File::create_new(&source).unwrap();
        let sql = tako_db::PortableDatabase::open(&source).unwrap();
        sql.execute_batch(
            "CREATE TABLE orders(id INTEGER PRIMARY KEY); INSERT INTO orders VALUES(1)",
        )
        .unwrap();
        let spool_dir = root.path().join("spool");
        let spool = tako_backup::CommittedSpool::open(&spool_dir).unwrap();
        for (id, at) in [(1, 1000), (2, 2000)] {
            if id == 2 {
                sql.execute_batch("INSERT INTO orders VALUES(2)").unwrap();
            }
            sql.checkpoint().unwrap();
            let page_size = sql.query("PRAGMA page_size", &[]).unwrap()[0][0]
                .as_int()
                .unwrap();
            spool
                .snapshot(fs::File::open(&source).unwrap(), id, at, page_size as u32)
                .unwrap();
        }
        let remote = Arc::new(tako_storage::local(&root.path().join("backup")).unwrap());
        let replica = Replica::database(remote.clone(), "orders", "generation", [42; 32]).unwrap();
        replica.upload(&spool_dir).await.unwrap();

        let output = root.path().join("restored.sqlite");
        assert_eq!(
            recover(&replica, Target::Time(1500), &output)
                .await
                .unwrap(),
            1
        );
        assert!(recover(&replica, Target::Latest, &output).await.is_err());
        let restored = tako_db::PortableDatabase::open(&output).unwrap();
        assert_eq!(
            restored.query("SELECT COUNT(*) FROM orders", &[]).unwrap()[0][0].as_int(),
            Some(1)
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&output).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let incorrect_key = Replica::database(remote, "orders", "generation", [7; 32]).unwrap();
        let missing = root.path().join("unverified.sqlite");
        assert!(
            recover(&incorrect_key, Target::Latest, &missing)
                .await
                .is_err()
        );
        assert!(!missing.exists());
    }
}
