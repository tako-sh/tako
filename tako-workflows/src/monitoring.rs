//! Claim latency for the server's Prometheus endpoint. Scraping never
//! performs database I/O: claims record their own timings.

use std::sync::{Arc, OnceLock, Weak};

use prometheus::core::{Collector, Desc};
use prometheus::proto::MetricFamily;
use prometheus::{Histogram, HistogramOpts, HistogramVec, Registry};

use crate::WorkflowManager;

const CLAIM_NAME: &str = "tako_workflow_claim_duration_seconds";
const CLAIM_HELP: &str = "Workflow claim call latency including storage wait, by outcome";

fn claim_vector() -> HistogramVec {
    HistogramVec::new(
        HistogramOpts::new(CLAIM_NAME, CLAIM_HELP).buckets(vec![
            0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0,
        ]),
        &["server", "app", "outcome"],
    )
    .unwrap()
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

/// A store's claim timings. Claims are timed once monitoring has observed
/// the store, so a server without metrics records nothing.
#[derive(Default)]
pub(crate) struct StoreMonitoring {
    pub(crate) claims: OnceLock<ClaimMetrics>,
}

impl StoreMonitoring {
    fn enable(&self, server: &str, app: &str) {
        self.claims.get_or_init(|| {
            let vector = claim_vector();
            let outcomes = ["claimed", "empty", "error"]
                .map(|outcome| vector.with_label_values(&[server, app, outcome]));
            ClaimMetrics { vector, outcomes }
        });
    }
}

struct WorkflowCollector {
    manager: Weak<WorkflowManager>,
    server: String,
    descriptors: Vec<Desc>,
}

impl WorkflowCollector {
    fn new(manager: Weak<WorkflowManager>, server: String) -> Self {
        Self {
            manager,
            server,
            descriptors: claim_vector().desc().into_iter().cloned().collect(),
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
        // Only active stores are exported, so stopped and replaced apps
        // disappear from the next scrape.
        let mut families = Vec::new();
        for (app, db) in manager.monitored_stores() {
            db.monitoring.enable(&self.server, &app);
            families.extend(db.monitoring.claims.get().unwrap().vector.collect());
        }
        families
    }
}

/// Register the workflow metrics. Call once when the server's metrics
/// listener is enabled. Nothing is exported once the manager is dropped.
pub fn register(
    manager: &Arc<WorkflowManager>,
    server: &str,
    registry: &Registry,
) -> Result<(), prometheus::Error> {
    registry.register(Box::new(WorkflowCollector::new(
        Arc::downgrade(manager),
        server.into(),
    )))
}

#[cfg(test)]
mod tests;
