use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use tako_network::{AppEnv, services::MAX_SERVICES};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalServices {
    pub observed_at: u64,
    pub apps: BTreeSet<AppEnv>,
}

impl LocalServices {
    pub fn validate(&self, now: u64) -> Result<(), &'static str> {
        if self.observed_at > now || now - self.observed_at >= 15 || self.apps.len() > MAX_SERVICES
        {
            return Err("stale or oversized local service export");
        }
        Ok(())
    }
}
