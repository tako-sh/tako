//! Presence leases live only in this handle's memory: they are never written
//! to the replay database, so a backup or restore cannot capture or revive
//! membership.
use super::ChannelStore;
use crate::{ChannelError, PresenceMember};
use std::collections::BTreeMap;

pub(super) const LEASE_MS: i64 = 45_000;
const MAX_DATA_BYTES: usize = 16_384;
/// Expired leases are invisible at once; sweeping them is only memory upkeep.
const SWEEP_INTERVAL_MS: i64 = 1_000;

struct Lease {
    member: PresenceMember,
    expires_at: i64,
}

#[derive(Default)]
pub(super) struct Leases {
    /// Channel, then connection: snapshots list members by connection ID.
    channels: BTreeMap<String, BTreeMap<String, Lease>>,
    next_sweep: i64,
}

impl Leases {
    fn sweep(&mut self, now: i64) {
        if now < self.next_sweep {
            return;
        }
        self.next_sweep = now.saturating_add(SWEEP_INTERVAL_MS);
        self.channels.retain(|_, members| {
            members.retain(|_, lease| lease.expires_at > now);
            !members.is_empty()
        });
    }
}

impl ChannelStore {
    pub fn update_presence(
        &self,
        channel: &str,
        member: &PresenceMember,
        now: i64,
    ) -> Result<(), ChannelError> {
        self.ensure_writable()?;
        let data = serde_json::to_string(&member.data)
            .map_err(|error| ChannelError::BadRequest(error.to_string()))?;
        if data.len() > MAX_DATA_BYTES {
            return Err(ChannelError::BadRequest(
                "Presence data must be at most 16 KiB".into(),
            ));
        }
        let mut leases = self.presence.lock();
        leases.sweep(now);
        leases.channels.entry(channel.into()).or_default().insert(
            member.connection_id.clone(),
            Lease {
                member: member.clone(),
                expires_at: now.saturating_add(LEASE_MS),
            },
        );
        Ok(())
    }

    pub fn presence(&self, channel: &str, now: i64) -> Result<Vec<PresenceMember>, ChannelError> {
        Ok(self
            .presence
            .lock()
            .channels
            .get(channel)
            .into_iter()
            .flat_map(|members| members.values())
            .filter(|lease| lease.expires_at > now)
            .map(|lease| lease.member.clone())
            .collect())
    }

    pub fn remove_presence(&self, channel: &str, connection_id: &str) -> Result<(), ChannelError> {
        self.ensure_writable()?;
        let mut leases = self.presence.lock();
        if let Some(members) = leases.channels.get_mut(channel) {
            members.remove(connection_id);
            if members.is_empty() {
                leases.channels.remove(channel);
            }
        }
        Ok(())
    }
}
