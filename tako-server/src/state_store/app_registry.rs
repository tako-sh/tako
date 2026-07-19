use crate::instances::AppConfig;

use super::{PersistedApp, SqliteStateStore, StateStoreError, block_on};

impl SqliteStateStore {
    pub fn upsert_app(&self, config: &AppConfig, routes: &[String]) -> Result<(), StateStoreError> {
        let conn = self.lock_conn()?;
        block_on(async {
            let tx = conn.unchecked_transaction().await?;
            let result = upsert_app_on(&tx, config, routes).await;
            tako_sqlite::commit_or_rollback(tx, result).await
        })
    }

    pub fn delete_app(&self, name: &str, environment: &str) -> Result<(), StateStoreError> {
        let conn = self.lock_conn()?;
        // Delete secrets for this app to prevent leaking to a future app with the same name.
        let secret_key = format!("{name}/{environment}");
        block_on(async {
            for table in [
                "app_secrets",
                "app_runtime_credentials",
                "app_storages",
                "app_ssl",
                "app_backups",
            ] {
                conn.execute(
                    &format!("DELETE FROM {table} WHERE app = ?1;"),
                    (secret_key.as_str(),),
                )
                .await?;
            }
            conn.execute(
                "DELETE FROM apps WHERE name = ?1 AND environment = ?2;",
                (name, environment),
            )
            .await?;
            Ok(())
        })
    }

    pub fn load_apps(&self) -> Result<Vec<PersistedApp>, StateStoreError> {
        let conn = self.lock_conn()?;
        block_on(async {
            let mut rows = conn
                .query(
                    "SELECT
                        name, environment, version, min_instances, max_instances, source_ip
                     FROM apps
                     ORDER BY name, environment;",
                    (),
                )
                .await?;

            let mut app_rows: Vec<(String, String, String, i64, i64, String)> = Vec::new();
            while let Some(row) = rows.next().await? {
                app_rows.push((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ));
            }
            drop(rows);

            let mut apps = Vec::new();
            for (name, environment, version, min_instances, max_instances, source_ip) in app_rows {
                let mut route_rows = conn
                    .query(
                        "SELECT route FROM app_routes
                         WHERE name = ?1 AND environment = ?2
                         ORDER BY route;",
                        (name.as_str(), environment.as_str()),
                    )
                    .await?;
                let mut routes = Vec::new();
                while let Some(row) = route_rows.next().await? {
                    routes.push(row.get::<String>(0)?);
                }

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
        })
    }
}

async fn upsert_app_on(
    conn: &turso::Connection,
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
        (
            config.name.as_str(),
            config.environment.as_str(),
            config.version.as_str(),
            config.min_instances as i64,
            config.max_instances as i64,
            source_ip_to_str(config.source_ip),
        ),
    )
    .await?;

    conn.execute(
        "DELETE FROM app_routes WHERE name = ?1 AND environment = ?2;",
        (config.name.as_str(), config.environment.as_str()),
    )
    .await?;

    for route in routes {
        conn.execute(
            "INSERT INTO app_routes (name, environment, route) VALUES (?1, ?2, ?3);",
            (
                config.name.as_str(),
                config.environment.as_str(),
                route.as_str(),
            ),
        )
        .await?;
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
