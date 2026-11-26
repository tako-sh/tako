use crate::app_command::load_release_manifest;
use std::path::Path;

pub(crate) fn validate_release_protocol(
    release_dir: &Path,
    allow_incompatible: bool,
) -> Result<(), String> {
    let manifest = load_release_manifest(release_dir)?;
    validate_protocol_version(manifest.protocol_version, allow_incompatible)
}

pub(crate) fn validate_active_release_protocols(
    data_dir: &Path,
    allow_incompatible: bool,
) -> Result<(), String> {
    let state_path = data_dir.join("state.redb");
    let active_releases = crate::state_store::load_persisted_releases_read_only(&state_path)
        .map_err(|error| {
            format!(
                "Failed to inspect active releases in '{}': {error}",
                state_path.display()
            )
        })?;

    for active_release in active_releases {
        let release_dir = crate::release::app_release_root(
            data_dir,
            &active_release.app_id,
            &active_release.version,
        );
        validate_release_protocol(&release_dir, allow_incompatible).map_err(|error| {
            format!(
                "Active app '{}' release '{}' is incompatible: {error}",
                active_release.app_id, active_release.version
            )
        })?;
    }

    Ok(())
}

pub(crate) fn validate_expected_server_protocol(
    expected_protocol_version: u32,
    allow_incompatible: bool,
) -> Result<(), String> {
    if expected_protocol_version == tako_core::PROTOCOL_VERSION || allow_incompatible {
        return Ok(());
    }
    Err(format!(
        "Protocol version mismatch: client={} server={}. Re-run with --force to attempt anyway.",
        expected_protocol_version,
        tako_core::PROTOCOL_VERSION
    ))
}

fn validate_protocol_version(
    release_protocol_version: u32,
    allow_incompatible: bool,
) -> Result<(), String> {
    if release_protocol_version == tako_core::PROTOCOL_VERSION {
        return Ok(());
    }
    if allow_incompatible {
        tracing::warn!(
            release_protocol_version,
            server_protocol_version = tako_core::PROTOCOL_VERSION,
            "Forcing an incompatible protocol version"
        );
        return Ok(());
    }
    Err(format!(
        "Protocol version mismatch: release={} server={}. Re-run with --force to attempt anyway.",
        release_protocol_version,
        tako_core::PROTOCOL_VERSION
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instances::AppConfig;
    use crate::state_store::StateStore;
    use tempfile::TempDir;

    fn write_manifest(release_dir: &Path, protocol_version: u32) {
        std::fs::create_dir_all(release_dir).unwrap();
        std::fs::write(
            release_dir.join("app.json"),
            serde_json::to_vec(&serde_json::json!({
                "protocol_version": protocol_version,
                "runtime": "bun",
                "main": "index.ts",
                "idle_timeout": 300
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn persist_active_release(data_dir: &Path, name: &str, environment: &str, version: &str) {
        let store = StateStore::new(data_dir.join("state.redb"), [0; 32]);
        store.init().unwrap();
        store
            .upsert_app(
                &AppConfig {
                    name: name.to_string(),
                    environment: environment.to_string(),
                    version: version.to_string(),
                    ..Default::default()
                },
                &[],
            )
            .unwrap();
    }

    #[test]
    fn active_release_scan_rejects_mismatch() {
        let temp = TempDir::new().unwrap();
        let release = temp.path().join("apps/my-app/production/releases/v1");
        write_manifest(&release, tako_core::PROTOCOL_VERSION + 1);
        persist_active_release(temp.path(), "my-app", "production", "v1");

        let error = validate_active_release_protocols(temp.path(), false).unwrap_err();

        assert!(error.contains("my-app/production"), "got: {error}");
        assert!(error.contains("release=1 server=0"), "got: {error}");
    }

    #[test]
    fn active_release_scan_allows_mismatch_when_forced() {
        let temp = TempDir::new().unwrap();
        let release = temp.path().join("apps/my-app/production/releases/v1");
        write_manifest(&release, tako_core::PROTOCOL_VERSION + 1);
        persist_active_release(temp.path(), "my-app", "production", "v1");

        validate_active_release_protocols(temp.path(), true).unwrap();
    }

    #[test]
    fn expected_server_protocol_requires_exact_match() {
        let error =
            validate_expected_server_protocol(tako_core::PROTOCOL_VERSION + 1, false).unwrap_err();

        assert!(error.contains("client=1 server=0"), "got: {error}");
        validate_expected_server_protocol(tako_core::PROTOCOL_VERSION + 1, true).unwrap();
    }
}
