//! Ephemeral presence leases, keyed by the authenticated originating server.
use super::error;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io,
    time::{Duration, Instant},
};
use tako_channels::PresenceMember;
use tako_network::DeviceId;

pub(super) const LEASE_MS: i64 = 45_000;
pub(super) const MAX_TOTAL_BYTES: usize = 8 * 1024 * 1024;
const MAX_APP_BYTES: usize = 1024 * 1024;
const MAX_APP_MEMBERS: usize = 256;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Publication {
    Update {
        channel: String,
        member: PresenceMember,
        expires_at_ms: i64,
        sequence: u64,
    },
    Leave {
        channel: String,
        connection_id: String,
        sequence: u64,
    },
}
impl Publication {
    pub fn sequence(&self) -> u64 {
        match self {
            Self::Update { sequence, .. } | Self::Leave { sequence, .. } => *sequence,
        }
    }
}
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    origin: DeviceId,
    channel: String,
    connection_id: String,
}
struct Lease {
    member: PresenceMember,
    expires: Instant,
    bytes: usize,
}
#[derive(Default)]
pub(super) struct Leases {
    members: BTreeMap<Key, Lease>,
    received: BTreeMap<DeviceId, u64>,
    sequence: u64,
    bytes: usize,
}
impl Leases {
    pub fn bytes(&self) -> usize {
        self.bytes
    }
    pub fn next_sequence(&mut self) -> io::Result<u64> {
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| error("presence sequence exhausted"))?;
        Ok(self.sequence)
    }
    pub fn fresh(&self, origin: &DeviceId, sequence: u64) -> bool {
        sequence != 0 && sequence > self.received.get(origin).copied().unwrap_or(0)
    }
    pub fn received(&mut self, origin: DeviceId, sequence: u64) {
        self.received.insert(origin, sequence);
    }
    pub fn check(
        &self,
        origin: &DeviceId,
        channel: &str,
        member: &PresenceMember,
        allowance: usize,
    ) -> io::Result<usize> {
        validate_member(channel, member)?;
        validate_owned_id(origin, &member.connection_id)?;
        let bytes = serde_json::to_vec(member)
            .map_err(|_| error("invalid presence member"))?
            .len()
            + channel.len();
        let key = Key {
            origin: origin.clone(),
            channel: channel.into(),
            connection_id: member.connection_id.clone(),
        };
        let old = self.members.get(&key).map_or(0, |lease| lease.bytes);
        if (old == 0 && self.members.len() >= MAX_APP_MEMBERS)
            || self.bytes.saturating_sub(old).saturating_add(bytes) > allowance.min(MAX_APP_BYTES)
        {
            return Err(error("presence lease capacity exceeded"));
        }
        Ok(bytes)
    }
    pub fn update(
        &mut self,
        origin: DeviceId,
        channel: String,
        member: PresenceMember,
        expires: Instant,
        bytes: usize,
    ) {
        let key = Key {
            origin,
            channel,
            connection_id: member.connection_id.clone(),
        };
        if let Some(old) = self.members.insert(
            key,
            Lease {
                member,
                expires,
                bytes,
            },
        ) {
            self.bytes -= old.bytes;
        }
        self.bytes += bytes;
    }
    pub fn leave(&mut self, origin: &DeviceId, channel: &str, connection_id: &str) {
        let key = Key {
            origin: origin.clone(),
            channel: channel.into(),
            connection_id: connection_id.into(),
        };
        if let Some(old) = self.members.remove(&key) {
            self.bytes -= old.bytes;
        }
    }
    pub fn expire(&mut self, now: Instant) {
        self.members.retain(|_, lease| {
            if lease.expires <= now {
                self.bytes -= lease.bytes;
                false
            } else {
                true
            }
        });
    }
    pub fn clear_remote(&mut self, local: &DeviceId) {
        self.members.retain(|key, lease| {
            if &key.origin != local {
                self.bytes -= lease.bytes;
                false
            } else {
                true
            }
        });
        self.received.clear();
    }
    pub fn snapshot(&self, channel: &str, local: &DeviceId) -> Vec<PresenceMember> {
        self.members
            .iter()
            .filter(|(key, _)| key.channel == channel)
            .map(|(_, lease)| project_member(local, lease.member.clone()))
            .collect()
    }
}
pub(super) fn local_member(
    local: &DeviceId,
    channel: &str,
    member: &PresenceMember,
) -> io::Result<PresenceMember> {
    validate_member(channel, member)?;
    validate_local_id(&member.connection_id)?;
    let mut member = member.clone();
    member.connection_id = format!("{local}:{}", member.connection_id);
    Ok(member)
}
pub(super) fn local_id(local: &DeviceId, connection_id: &str) -> io::Result<String> {
    validate_local_id(connection_id)?;
    Ok(format!("{local}:{connection_id}"))
}
pub(super) fn validate_channel(channel: &str) -> io::Result<()> {
    if channel.is_empty() || channel.len() > 1024 {
        return Err(error("invalid presence scope"));
    }
    Ok(())
}
fn validate_local_id(id: &str) -> io::Result<()> {
    if id.len() != 32
        || !id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(error("invalid presence connection identity"));
    }
    Ok(())
}
pub(super) fn validate_owned_id(origin: &DeviceId, id: &str) -> io::Result<()> {
    let raw = id
        .strip_prefix(&format!("{origin}:"))
        .ok_or_else(|| error("foreign presence connection identity"))?;
    validate_local_id(raw)
}
fn validate_member(channel: &str, member: &PresenceMember) -> io::Result<()> {
    validate_channel(channel)?;
    if member.subject.as_ref().is_some_and(|s| s.len() > 1024)
        || serde_json::to_vec(&member.data)
            .map_err(|_| error("invalid presence data"))?
            .len()
            > 16_384
    {
        return Err(error("presence metadata exceeds limit"));
    }
    Ok(())
}
pub(super) fn project_member(local: &DeviceId, mut member: PresenceMember) -> PresenceMember {
    // PresenceSession reports its original random ID to its own WebSocket. Foreign
    // IDs remain qualified so a peer cannot alias a local connection in snapshots.
    if let Some(id) = member.connection_id.strip_prefix(&format!("{local}:")) {
        member.connection_id = id.into();
    }
    member
}
pub(super) fn remaining(expires_at_ms: i64, now_ms: i64) -> Option<Duration> {
    let remaining = expires_at_ms.saturating_sub(now_ms);
    (remaining > 0).then(|| Duration::from_millis(remaining.min(LEASE_MS) as u64))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn origin() -> DeviceId {
        "00".repeat(32).parse().unwrap()
    }
    fn member() -> PresenceMember {
        PresenceMember {
            connection_id: "ab".repeat(16),
            subject: Some("verified".into()),
            data: serde_json::json!({"typing":true}),
        }
    }
    #[test]
    fn lease_expires_at_original_deadline_and_snapshot_never_renews_it() {
        let origin = origin();
        let member = local_member(&origin, "room", &member()).unwrap();
        let mut leases = Leases::default();
        let start = Instant::now();
        let bytes = leases
            .check(&origin, "room", &member, MAX_TOTAL_BYTES)
            .unwrap();
        leases.update(
            origin.clone(),
            "room".into(),
            member,
            start + Duration::from_secs(45),
            bytes,
        );
        for second in [1, 20, 44] {
            leases.expire(start + Duration::from_secs(second));
            assert_eq!(leases.snapshot("room", &origin).len(), 1);
        }
        leases.expire(start + Duration::from_secs(45));
        assert!(leases.snapshot("room", &origin).is_empty());
        assert_eq!(leases.bytes(), 0);
        assert_eq!(remaining(45_000, 40_000), Some(Duration::from_secs(5)));
        assert_eq!(remaining(45_000, 45_000), None);
        assert_eq!(remaining(i64::MAX, 1), Some(Duration::from_secs(45)));
    }
    #[test]
    fn connection_ownership_and_capacity_are_checked_before_mutation() {
        let origin = origin();
        let member = local_member(&origin, "room", &member()).unwrap();
        let other: DeviceId = "11".repeat(32).parse().unwrap();
        let leases = Leases::default();
        assert!(
            leases
                .check(&other, "room", &member, MAX_TOTAL_BYTES)
                .is_err()
        );
        assert!(leases.check(&origin, "room", &member, 1).is_err());
        assert!(local_id(&origin, &member.connection_id).is_err());
        let mut huge = member.clone();
        huge.data = serde_json::json!("x".repeat(16_384));
        assert!(
            leases
                .check(&origin, "room", &huge, MAX_TOTAL_BYTES)
                .is_err()
        );
        assert_eq!(leases.bytes(), 0);
    }
    #[test]
    fn duplicate_sequences_and_peer_loss_cannot_renew_or_alias_local_members() {
        let origin = origin();
        let remote: DeviceId = "11".repeat(32).parse().unwrap();
        let mut leases = Leases::default();
        for server in [&origin, &remote] {
            let member = local_member(server, "room", &member()).unwrap();
            let bytes = leases
                .check(server, "room", &member, MAX_TOTAL_BYTES)
                .unwrap();
            leases.update(
                server.clone(),
                "room".into(),
                member,
                Instant::now() + Duration::from_secs(45),
                bytes,
            );
        }
        let rows = leases.snapshot("room", &origin);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|m| m.connection_id == "ab".repeat(16)));
        assert!(
            rows.iter()
                .any(|m| m.connection_id.starts_with(&format!("{remote}:")))
        );
        leases.received(remote.clone(), 3);
        assert!(!leases.fresh(&remote, 3));
        assert!(!leases.fresh(&remote, 2));
        leases.clear_remote(&origin);
        assert_eq!(leases.snapshot("room", &origin).len(), 1);
        assert!(leases.fresh(&remote, 1));
    }
}
