//! Cached Postgres health and claim latency for the server's Prometheus endpoint.
//! Scraping never performs database I/O. Sampling uses a separate read-only connection.

use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::{Mutex, RwLock};
use postgres::{Client, Config, NoTls};
use prometheus::core::{Collector, Desc};
use prometheus::proto::MetricFamily;
use prometheus::{GaugeVec, Histogram, HistogramOpts, HistogramVec, Opts, Registry};

use crate::WorkflowManager;

const SAMPLE_INTERVAL: Duration = Duration::from_secs(30);
const CLAIM_NAME: &str = "tako_workflow_claim_duration_seconds";
const CLAIM_HELP: &str = "Workflow claim call latency including storage wait, by outcome";
const HEALTH_METRICS: &[(&str, &str)] = &[
    (
        "health_sample_success",
        "Whether the latest Postgres health sample succeeded",
    ),
    (
        "health_last_success_timestamp_seconds",
        "Unix timestamp of the last successful health sample",
    ),
    (
        "runnable_jobs",
        "Due pending jobs for this app; excludes future and parked jobs",
    ),
    (
        "oldest_runnable_age_seconds",
        "Seconds since the oldest pending job became due for this app",
    ),
    (
        "pg_runs_dead_tuples",
        "Estimated dead tuples in the shared runs table",
    ),
    (
        "pg_runs_live_tuples",
        "Estimated live tuples in the shared runs table",
    ),
    (
        "pg_runs_table_bytes",
        "Shared runs table bytes including TOAST and auxiliary forks",
    ),
    (
        "pg_runs_index_bytes",
        "Total index bytes for the shared runs table",
    ),
    (
        "pg_runs_autovacuum_count",
        "Autovacuum count for the shared runs table since statistics reset",
    ),
    (
        "pg_oldest_transaction_seconds",
        "Oldest visible transaction age in this database, excluding the sampler",
    ),
    (
        "pg_oldest_backend_xmin_age_transactions",
        "Oldest visible backend xmin age in transactions in this database",
    ),
    (
        "pg_activity_visibility_complete",
        "Whether this role can see other roles' activity statistics",
    ),
];

fn claim_vector() -> HistogramVec {
    HistogramVec::new(
        HistogramOpts::new(CLAIM_NAME, CLAIM_HELP).buckets(vec![
            0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0,
        ]),
        &["server", "app", "outcome"],
    )
    .unwrap()
}

fn health_vectors() -> Vec<GaugeVec> {
    HEALTH_METRICS
        .iter()
        .map(|(name, help)| {
            GaugeVec::new(
                Opts::new(format!("tako_workflow_{name}"), *help),
                &["server", "app"],
            )
            .unwrap()
        })
        .collect()
}

pub(crate) struct ClaimMetrics {
    vector: HistogramVec,
    outcomes: [Histogram; 3],
}

impl ClaimMetrics {
    pub(crate) fn observe(&self, outcome: usize, seconds: f64) {
        self.outcomes[outcome].observe(seconds);
    }
}

#[derive(Default)]
struct CachedHealth {
    success: bool,
    timestamp: f64,
    values: Option<Vec<f64>>,
}

#[derive(Default)]
pub(crate) struct StoreMonitoring {
    pub(crate) claims: OnceLock<ClaimMetrics>,
    reader: Option<Mutex<HealthReader>>,
    health: RwLock<CachedHealth>,
}

impl StoreMonitoring {
    pub(crate) fn postgres(url: &str, schema: &str, app: &str) -> Result<Self, postgres::Error> {
        Ok(Self {
            reader: Some(Mutex::new(HealthReader::new(url, schema, app)?)),
            ..Self::default()
        })
    }

    fn enable(&self, server: &str, app: &str) {
        self.claims.get_or_init(|| {
            let vector = claim_vector();
            let outcomes = ["claimed", "empty", "error"]
                .map(|outcome| vector.with_label_values(&[server, app, outcome]));
            ClaimMetrics { vector, outcomes }
        });
    }

    fn sample(&self) -> Result<(), postgres::Error> {
        let Some(reader) = &self.reader else {
            return Ok(());
        };
        let result = reader.lock().sample();
        let mut health = self.health.write();
        match result {
            Ok(values) => {
                health.success = true;
                health.timestamp = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs_f64();
                health.values = Some(values);
                Ok(())
            }
            Err(error) => {
                // Retain last good data with explicit freshness/failure signals.
                health.success = false;
                Err(error)
            }
        }
    }
}

struct HealthReader {
    config: Config,
    client: Option<Client>,
    schema: String,
    app: String,
}

impl HealthReader {
    fn new(url: &str, schema: &str, app: &str) -> Result<Self, postgres::Error> {
        let mut config: Config = url.parse()?;
        config.connect_timeout(Duration::from_secs(2));
        config.application_name("tako-workflow-monitor");
        Ok(Self {
            config,
            client: None,
            schema: schema.into(),
            app: app.into(),
        })
    }

    fn sample(&mut self) -> Result<Vec<f64>, postgres::Error> {
        let result = self.query();
        if result.is_err() {
            // Reconnect on the next sample, including after timeouts or restarts.
            self.client = None;
        }
        result
    }

    fn query(&mut self) -> Result<Vec<f64>, postgres::Error> {
        if self.client.is_none() {
            let mut client = self.config.connect(NoTls)?;
            client.batch_execute("SET default_transaction_read_only=on; SET statement_timeout='2s'; SET lock_timeout='500ms'")?;
            self.client = Some(client);
        }
        let client = self.client.as_mut().unwrap();
        // Do not leave a monitoring snapshot open between samples.
        let now = crate::enqueue::now_ms();
        let queue = client.query_one(&format!(
            "SELECT count(*)::float8, COALESCE(GREATEST(($2::bigint::float8-min(run_at)::float8)/1000,0),0)
             FROM {}.runs WHERE app_id=$1 AND status='pending' AND run_at <= $2",
            self.schema,
        ), &[&self.app, &now])?;
        let table = client.query_one(
            "SELECT n_dead_tup::float8, n_live_tup::float8, pg_table_size(relid)::float8,
                    pg_indexes_size(relid)::float8, autovacuum_count::float8
             FROM pg_stat_user_tables WHERE schemaname=$1 AND relname='runs'",
            &[&self.schema],
        )?;
        let activity = client.query_one(
            "SELECT COALESCE(max(EXTRACT(EPOCH FROM clock_timestamp()-xact_start))::float8,0),
                    COALESCE(max(age(backend_xmin))::float8,0),
                    pg_has_role(current_user,'pg_read_all_stats','USAGE')
             FROM pg_stat_activity WHERE datname=current_database() AND pid<>pg_backend_pid()",
            &[],
        )?;
        Ok(vec![
            queue.get(0),
            queue.get(1),
            table.get(0),
            table.get(1),
            table.get(2),
            table.get(3),
            table.get(4),
            activity.get(0),
            activity.get(1),
            if activity.get::<_, bool>(2) { 1.0 } else { 0.0 },
        ])
    }
}

struct WorkflowCollector {
    manager: Weak<WorkflowManager>,
    server: String,
    descriptors: Vec<Desc>,
}

impl WorkflowCollector {
    fn new(manager: Weak<WorkflowManager>, server: String) -> Self {
        let mut descriptors: Vec<Desc> = health_vectors()
            .iter()
            .flat_map(|g| g.desc().into_iter().cloned().collect::<Vec<_>>())
            .collect();
        descriptors.extend(claim_vector().desc().into_iter().cloned());
        Self {
            manager,
            server,
            descriptors,
        }
    }
}

impl Collector for WorkflowCollector {
    fn desc(&self) -> Vec<&Desc> {
        self.descriptors.iter().collect()
    }

    fn collect(&self) -> Vec<MetricFamily> {
        let Some(manager) = self.manager.upgrade() else {
            return Vec::new();
        };
        // Fresh vectors remove deleted/replaced apps and avoid cross-scrape mutation.
        let gauges = health_vectors();
        let mut families = Vec::new();
        for (app, db) in manager.monitored_stores() {
            db.monitoring.enable(&self.server, &app);
            families.extend(db.monitoring.claims.get().unwrap().vector.collect());
            if db.monitoring.reader.is_some() {
                let health = db.monitoring.health.read();
                gauges[0]
                    .with_label_values(&[&self.server, &app])
                    .set(if health.success { 1.0 } else { 0.0 });
                gauges[1]
                    .with_label_values(&[&self.server, &app])
                    .set(health.timestamp);
                if let Some(values) = &health.values {
                    for (gauge, value) in gauges[2..].iter().zip(values) {
                        gauge.with_label_values(&[&self.server, &app]).set(*value);
                    }
                }
            }
        }
        for gauge in gauges {
            families.extend(gauge.collect());
        }
        families
    }
}

/// Register cached metrics and start a 30-second sampler. Call once when the
/// server's metrics listener is enabled. The task ends when the manager is dropped.
pub fn register(
    manager: &Arc<WorkflowManager>,
    server: &str,
    registry: &Registry,
    runtime: &tokio::runtime::Handle,
) -> Result<(), prometheus::Error> {
    let weak = Arc::downgrade(manager);
    registry.register(Box::new(WorkflowCollector::new(
        weak.clone(),
        server.into(),
    )))?;
    let server = server.to_string();
    runtime.spawn(async move {
        loop {
            let Some(manager) = weak.upgrade() else {
                break;
            };
            let stores = manager.monitored_stores();
            drop(manager);
            for (app, db) in stores {
                db.monitoring.enable(&server, &app);
                if db.monitoring.reader.is_some() {
                    match crate::blocking::run(move || db.monitoring.sample()).await {
                        Ok(Ok(())) => {}
                        _ => tracing::warn!(%app, "Workflow Postgres health sample failed"),
                    }
                }
            }
            tokio::time::sleep(SAMPLE_INTERVAL).await;
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests;
