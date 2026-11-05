use crate::{
    DeviceId, MemberKind, RegistrySnapshot,
    api::{RuntimeError, RuntimeResult},
    corrosion::CorrosionStore,
    services::{MAX_LEASE_BYTES, ServiceLease},
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

const BATCH_SERVERS: usize = 32;
const MAX_SERVERS: usize = 256;
const MAX_BATCH_BYTES: usize = BATCH_SERVERS * (MAX_LEASE_BYTES * 6 + 1024);

impl CorrosionStore {
    pub fn publish_services(&self, lease: &ServiceLease) -> RuntimeResult<()> {
        let payload = serde_json::to_string(lease)?;
        ServiceLease::from_slice(payload.as_bytes())?;
        let bytes = self.post("/v1/transactions", &json!([[
            "INSERT INTO tako_service_leases (server_id, payload) VALUES (?, ?) ON CONFLICT(server_id) DO UPDATE SET payload=excluded.payload",
            [lease.server_id.to_string(), payload]
        ]]))?;
        let response: Value = serde_json::from_slice(&bytes)?;
        let results = response["results"]
            .as_array()
            .ok_or(RuntimeError::Corrosion("service write response"))?;
        if response.get("error").is_some()
            || results.len() != 1
            || results[0].get("error").is_some()
            || results[0]["rows_affected"].as_u64().is_none()
        {
            return Err(RuntimeError::Corrosion("service write failed"));
        }
        Ok(())
    }

    pub fn service_leases(
        &self,
        registry: &RegistrySnapshot,
        now: u64,
    ) -> RuntimeResult<Vec<ServiceLease>> {
        let members = registry.effective_members(now)?;
        let servers: Vec<_> = members
            .values()
            .filter(|m| matches!(m.kind, MemberKind::Server(_)) && m.permissions.server_admin)
            .collect();
        if servers.len() > MAX_SERVERS {
            return Err(RuntimeError::Capacity("service publishers"));
        }
        let mut leases = Vec::new();
        for batch in servers.chunks(BATCH_SERVERS) {
            let ids: BTreeSet<_> = batch.iter().map(|m| m.device_id.clone()).collect();
            let sql = format!(
                "SELECT server_id, CASE WHEN length(CAST(payload AS BLOB)) <= 65536 THEN payload ELSE NULL END AS payload FROM tako_service_leases WHERE server_id IN ({}) ORDER BY server_id LIMIT 33",
                vec!["?"; batch.len()].join(",")
            );
            let bytes = self.post("/v1/queries", &json!([sql, ids]))?;
            let rows = service_rows(&bytes)?;
            let mut candidates = BTreeMap::new();
            let mut seen = BTreeSet::new();
            for (id, payload) in rows {
                let Some(id) = id
                    .as_str()
                    .and_then(|id| id.parse::<DeviceId>().ok())
                    .filter(|id| ids.contains(id))
                else {
                    continue;
                };
                if !seen.insert(id.clone()) {
                    candidates.insert(id, None);
                    continue;
                }
                let lease = payload
                    .as_str()
                    .and_then(|payload| ServiceLease::from_slice(payload.as_bytes()).ok());
                let lease = lease.filter(|lease| {
                    lease.server_id == id
                        && lease
                            .verify_member(&registry.network.id, members[&id], now)
                            .is_ok()
                });
                candidates.insert(id, lease);
            }
            leases.extend(candidates.into_values().flatten());
        }
        Ok(leases)
    }
}

fn service_rows(bytes: &[u8]) -> RuntimeResult<Vec<(Value, Value)>> {
    if bytes.len() > MAX_BATCH_BYTES {
        return Err(RuntimeError::Capacity("service batch"));
    }
    let mut columns = false;
    let mut ended = false;
    let mut rows = Vec::new();
    for line in bytes
        .split(|b| *b == b'\n')
        .filter(|l| !l.iter().all(u8::is_ascii_whitespace))
    {
        if ended {
            return Err(RuntimeError::Corrosion("service data after eoq"));
        }
        let event: Value = serde_json::from_slice(line)?;
        let fields = event
            .as_object()
            .ok_or(RuntimeError::Corrosion("service query event"))?;
        if fields.len() != 1 {
            return Err(RuntimeError::Corrosion("service event fields"));
        }
        if let Some(names) = event.get("columns") {
            if columns || names != &json!(["server_id", "payload"]) {
                return Err(RuntimeError::Corrosion("service columns"));
            }
            columns = true;
        } else if let Some(row) = event.get("row").and_then(Value::as_array) {
            if !columns || row.len() != 2 || row[0].as_u64().is_none() {
                return Err(RuntimeError::Corrosion("service row framing"));
            }
            let values = row[1]
                .as_array()
                .filter(|v| v.len() == 2)
                .ok_or(RuntimeError::Corrosion("service row width"))?;
            rows.push((values[0].clone(), values[1].clone()));
            if rows.len() > BATCH_SERVERS {
                return Err(RuntimeError::Capacity("service batch rows"));
            }
        } else if event.get("eoq").is_some_and(Value::is_object) && columns {
            ended = true;
        } else {
            return Err(RuntimeError::Corrosion("incomplete service query"));
        }
    }
    if !ended {
        return Err(RuntimeError::Corrosion("incomplete service query"));
    }
    Ok(rows)
}

#[cfg(test)]
mod tests;
