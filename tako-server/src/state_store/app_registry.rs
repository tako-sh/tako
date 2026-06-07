use crate::instances::AppConfig;

use super::{PersistedApp, SqliteStateStore, StateStoreError};

impl SqliteStateStore {
    pub fn upsert_app(&self, config: &AppConfig, routes: &[String]) -> Result<(), StateStoreError> {
        let conn = self.open_connection()?;
        let tx = conn
            .unchecked_transaction()
            .map_err(StateStoreError::from)?;
        upsert_app_on(&tx, config, routes)?;

        tx.commit().map_err(StateStoreError::from)?;
        Ok(())
    }

    pub fn delete_app(&self, name: &str, environment: &str) -> Result<(), StateStoreError> {
        let conn = self.open_connection()?;
        // Delete secrets for this app to prevent leaking to a future app with the same name.
        let secret_key = format!("{name}/{environment}");
        conn.execute("DELETE FROM app_secrets WHERE app = ?1;", [&secret_key])
            .map_err(StateStoreError::from)?;
        conn.execute(
            "DELETE FROM app_runtime_credentials WHERE app = ?1;",
            [&secret_key],
        )
        .map_err(StateStoreError::from)?;
        conn.execute("DELETE FROM app_storages WHERE app = ?1;", [&secret_key])
            .map_err(StateStoreError::from)?;
        conn.execute("DELETE FROM app_ssl WHERE app = ?1;", [&secret_key])
            .map_err(StateStoreError::from)?;
        conn.execute("DELETE FROM app_backups WHERE app = ?1;", [&secret_key])
            .map_err(StateStoreError::from)?;
        conn.execute(
            "DELETE FROM apps WHERE name = ?1 AND environment = ?2;",
            rusqlite::params![name, environment],
        )
        .map_err(StateStoreError::from)?;
        Ok(())
    }

    pub fn load_apps(&self) -> Result<Vec<PersistedApp>, StateStoreError> {
        let conn = self.open_connection()?;

        let mut stmt = conn
            .prepare(
                "SELECT
                    name, environment, version, min_instances, max_instances, source_ip
                 FROM apps
                 ORDER BY name, environment;",
            )
            .map_err(StateStoreError::from)?;

        let mut apps = Vec::new();
        let mut rows = stmt.query([]).map_err(StateStoreError::from)?;

        while let Some(row) = rows.next().map_err(StateStoreError::from)? {
            let name: String = row.get(0).map_err(StateStoreError::from)?;
            let environment: String = row.get(1).map_err(StateStoreError::from)?;
            let version: String = row.get(2).map_err(StateStoreError::from)?;
            let min_instances: i64 = row.get(3).map_err(StateStoreError::from)?;
            let max_instances: i64 = row.get(4).map_err(StateStoreError::from)?;
            let source_ip: String = row.get(5).map_err(StateStoreError::from)?;

            let mut routes_stmt = conn
                .prepare(
                    "SELECT route FROM app_routes
                     WHERE name = ?1 AND environment = ?2
                     ORDER BY route;",
                )
                .map_err(StateStoreError::from)?;
            let routes: Vec<String> = routes_stmt
                .query_map(rusqlite::params![&name, &environment], |r| r.get(0))
                .map_err(StateStoreError::from)?
                .collect::<Result<Vec<String>, _>>()
                .map_err(StateStoreError::from)?;

            let config = AppConfig {
                name,
                environment,
                version,
                min_instances: to_u32(min_instances, "min_instances")?,
                max_instances: to_u32(max_instances, "max_instances")?,
                source_ip: source_ip_from_str(&source_ip)?,
                ..Default::default()
            };

            apps.push(PersistedApp { config, routes });
        }

        Ok(apps)
    }
}

fn upsert_app_on(
    conn: &rusqlite::Connection,
    config: &AppConfig,
    routes: &[String],
) -> Result<(), StateStoreError> {
    conn.execute(
        "INSERT INTO apps (
            name, environment, version, min_instances, max_instances, source_ip
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(name, environment) DO UPDATE SET
            version = excluded.version,
            min_instances = excluded.min_instances,
            max_instances = excluded.max_instances,
            source_ip = excluded.source_ip;",
        rusqlite::params![
            &config.name,
            &config.environment,
            &config.version,
            config.min_instances as i64,
            config.max_instances as i64,
            source_ip_to_str(config.source_ip),
        ],
    )
    .map_err(StateStoreError::from)?;

    conn.execute(
        "DELETE FROM app_routes WHERE name = ?1 AND environment = ?2;",
        rusqlite::params![&config.name, &config.environment],
    )
    .map_err(StateStoreError::from)?;

    for route in routes {
        conn.execute(
            "INSERT INTO app_routes (name, environment, route) VALUES (?1, ?2, ?3);",
            rusqlite::params![&config.name, &config.environment, route],
        )
        .map_err(StateStoreError::from)?;
    }

    Ok(())
}

fn to_u32(value: i64, field: &str) -> Result<u32, StateStoreError> {
    u32::try_from(value).map_err(|_| {
        StateStoreError::InvalidData(format!("field '{field}' out of range for u32: {value}"))
    })
}

fn source_ip_to_str(mode: tako_core::SourceIpMode) -> &'static str {
    match mode {
        tako_core::SourceIpMode::Auto => "auto",
        tako_core::SourceIpMode::Direct => "direct",
        tako_core::SourceIpMode::CloudflareProxy => "cloudflare-proxy",
        tako_core::SourceIpMode::TrustedProxy => "trusted-proxy",
    }
}

fn source_ip_from_str(value: &str) -> Result<tako_core::SourceIpMode, StateStoreError> {
    match value {
        "auto" => Ok(tako_core::SourceIpMode::Auto),
        "direct" => Ok(tako_core::SourceIpMode::Direct),
        "cloudflare-proxy" => Ok(tako_core::SourceIpMode::CloudflareProxy),
        "trusted-proxy" => Ok(tako_core::SourceIpMode::TrustedProxy),
        other => Err(StateStoreError::InvalidData(format!(
            "unsupported source_ip mode '{other}'"
        ))),
    }
}
