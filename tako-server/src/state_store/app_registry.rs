use crate::instances::AppConfig;
use rusqlite::OptionalExtension;

use super::{PersistedApp, PersistedRelease, SqliteStateStore, StateStoreError};

pub(crate) fn load_persisted_releases_read_only(
    state_path: &std::path::Path,
) -> Result<Vec<PersistedRelease>, StateStoreError> {
    if !state_path.exists() {
        return Ok(Vec::new());
    }
    let connection = rusqlite::Connection::open_with_flags(
        state_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let mut statement = connection.prepare(
        "SELECT name, environment, version
         FROM apps
         ORDER BY name, environment;",
    )?;
    let releases = statement
        .query_map([], |row| {
            let name = row.get::<_, String>(0)?;
            let environment = row.get::<_, String>(1)?;
            Ok(PersistedRelease {
                app_id: tako_core::deployment_app_id(&name, &environment),
                version: row.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(releases)
}

impl SqliteStateStore {
    pub fn upsert_app(&self, config: &AppConfig, routes: &[String]) -> Result<(), StateStoreError> {
        let conn = self.lock_conn()?;
        let tx = conn.unchecked_transaction()?;
        upsert_app_on(&tx, config, routes)?;
        tx.commit()?;
        Ok(())
    }

    pub fn delete_app(&self, name: &str, environment: &str) -> Result<(), StateStoreError> {
        let conn = self.lock_conn()?;
        // Delete secrets for this app to prevent leaking to a future app with the same name.
        let secret_key = format!("{name}/{environment}");
        for table in [
            "app_secrets",
            "app_runtime_credentials",
            "app_schedule_revisions",
            "app_storages",
            "app_ssl",
            "app_backups",
            "app_compose_pending",
            "app_compose_scales",
            "app_lifecycle",
            "app_release_routing",
        ] {
            conn.execute(
                &format!("DELETE FROM {table} WHERE app = ?1;"),
                (secret_key.as_str(),),
            )?;
        }
        conn.execute(
            "DELETE FROM apps WHERE name = ?1 AND environment = ?2;",
            (name, environment),
        )?;
        Ok(())
    }

    pub fn load_apps(&self) -> Result<Vec<PersistedApp>, StateStoreError> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT
                name, environment, version, min_instances, max_instances, source_ip
             FROM apps
             ORDER BY name, environment;",
        )?;
        let app_rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);

        let mut apps = Vec::new();
        for (name, environment, version, min_instances, max_instances, source_ip) in app_rows {
            let mut route_stmt = conn.prepare(
                "SELECT route FROM app_routes
                 WHERE name = ?1 AND environment = ?2
                 ORDER BY route;",
            )?;
            let routes = route_stmt
                .query_map((name.as_str(), environment.as_str()), |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?;

            let mut scale_stmt = conn.prepare(
                "SELECT service, replicas FROM app_compose_scales WHERE app = ?1 ORDER BY service;",
            )?;
            let compose_replicas = scale_stmt
                .query_map(
                    [if environment.is_empty() {
                        name.clone()
                    } else {
                        tako_core::deployment_app_id(&name, &environment)
                    }],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?)),
                )?
                .collect::<Result<_, _>>()?;
            let app_id = if environment.is_empty() {
                name.clone()
            } else {
                tako_core::deployment_app_id(&name, &environment)
            };
            let lifecycle = conn
                .query_row(
                    "SELECT stopped, compose_stopped FROM app_lifecycle WHERE app = ?1;",
                    [&app_id],
                    |row| Ok((row.get::<_, bool>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()?;
            let (stopped, compose_stopped) = match lifecycle {
                Some((stopped, services)) => (
                    stopped,
                    serde_json::from_str(&services).map_err(|e| {
                        StateStoreError::InvalidData(format!(
                            "Invalid stopped apps for {app_id}: {e}"
                        ))
                    })?,
                ),
                None => Default::default(),
            };
            let config = AppConfig {
                compose_replicas,
                stopped,
                compose_stopped,
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

pub(super) fn upsert_app_on(
    conn: &rusqlite::Connection,
    config: &AppConfig,
    routes: &[String],
) -> Result<(), StateStoreError> {
    let app = config.deployment_id();
    // Capture the pre-upgrade native release before overwriting the registry.
    // Existing release history is immutable until that version is deployed again.
    let previous = conn.query_row(
        "SELECT version, source_ip FROM apps WHERE name = ?1 AND environment = ?2
         AND NOT EXISTS (SELECT 1 FROM app_release_routing WHERE app = ?3 AND version = apps.version)",
        (&config.name, &config.environment, &app),
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    ).optional()?;
    if let Some((version, source_ip)) = previous {
        let mut statement = conn.prepare(
            "SELECT route FROM app_routes WHERE name = ?1 AND environment = ?2 ORDER BY route",
        )?;
        let routes = statement
            .query_map((&config.name, &config.environment), |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        super::release_routing::save_on(
            conn,
            &app,
            &version,
            &super::ReleaseRouting::new(
                routes,
                source_ip_from_str(&source_ip)?,
                None,
                &config.environment,
            ),
        )?;
    }
    super::release_routing::save_on(
        conn,
        &app,
        &config.version,
        &super::ReleaseRouting::new(
            routes.to_vec(),
            config.source_ip,
            config.compose.as_ref(),
            &config.environment,
        ),
    )?;
    conn.execute(
        "INSERT INTO apps (
            name, environment, version, min_instances, max_instances, source_ip
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(name, environment) DO UPDATE SET
            version = excluded.version,
            min_instances = excluded.min_instances,
            max_instances = excluded.max_instances,
            source_ip = excluded.source_ip;",
        (
            config.name.as_str(),
            config.environment.as_str(),
            config.version.as_str(),
            config.min_instances as i64,
            config.max_instances as i64,
            source_ip_to_str(config.source_ip),
        ),
    )?;

    conn.execute(
        "DELETE FROM app_routes WHERE name = ?1 AND environment = ?2;",
        (config.name.as_str(), config.environment.as_str()),
    )?;

    let app = config.deployment_id();
    conn.execute(
        "INSERT INTO app_lifecycle (app, stopped, compose_stopped) VALUES (?1, ?2, ?3)
         ON CONFLICT(app) DO UPDATE SET stopped = excluded.stopped, compose_stopped = excluded.compose_stopped;",
        (&app, config.stopped, serde_json::to_string(&config.compose_stopped)
            .map_err(|e| StateStoreError::InvalidData(e.to_string()))?),
    )?;
    conn.execute("DELETE FROM app_compose_scales WHERE app = ?1;", [&app])?;
    for (service, replicas) in &config.compose_replicas {
        conn.execute(
            "INSERT INTO app_compose_scales (app, service, replicas) VALUES (?1, ?2, ?3);",
            (&app, service, replicas),
        )?;
    }

    for route in routes {
        conn.execute(
            "INSERT INTO app_routes (name, environment, route) VALUES (?1, ?2, ?3);",
            (
                config.name.as_str(),
                config.environment.as_str(),
                route.as_str(),
            ),
        )?;
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
