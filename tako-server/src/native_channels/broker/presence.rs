use super::*;
use crate::native_channels::presence::{self, Publication};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tako_channels::PresenceMember;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
impl Broker {
    pub(super) fn expire_presence(&mut self) {
        let now = Instant::now();
        for app in self.apps.values_mut() {
            app.presence.expire(now);
        }
    }
    fn presence_allowance(&self, app: &AppEnv) -> usize {
        let other_bytes: usize = self
            .apps
            .iter()
            .filter(|(id, _)| *id != app)
            .map(|(_, app)| app.presence.bytes())
            .sum();
        presence::MAX_TOTAL_BYTES.saturating_sub(other_bytes)
    }
    pub(super) async fn update_presence(
        &mut self,
        app: &AppEnv,
        channel: String,
        member: PresenceMember,
    ) -> io::Result<()> {
        if let Some(application) = self.apps.get_mut(app) {
            application.presence.expire(Instant::now());
        }
        let member = presence::local_member(&self.local, &channel, &member)?;
        let allowance = self.presence_allowance(app);
        let application = self
            .apps
            .get_mut(app)
            .ok_or_else(|| error("unknown presence app"))?;
        let bytes = application
            .presence
            .check(&self.local, &channel, &member, allowance)?;
        let now = now_ms();
        application.presence.update(
            self.local.clone(),
            channel.clone(),
            member.clone(),
            Instant::now() + Duration::from_millis(presence::LEASE_MS as u64),
            now.saturating_add(presence::LEASE_MS),
            bytes,
        );
        let sequence = application.presence.next_sequence()?;
        let publication = Publication::Update {
            channel,
            member,
            expires_at_ms: now.saturating_add(presence::LEASE_MS),
            sequence,
        };
        if !self.fanout(&Frame::Presence {
            app: app.clone(),
            publication,
        }) {
            self.fence().await?;
        }
        Ok(())
    }
    pub(super) async fn presence(
        &mut self,
        app: &AppEnv,
        channel: String,
    ) -> io::Result<Vec<PresenceMember>> {
        presence::validate_channel(&channel)?;
        if let Some(application) = self.apps.get_mut(app) {
            application.presence.expire(Instant::now());
        }
        let application = self
            .apps
            .get_mut(app)
            .ok_or_else(|| error("unknown presence app"))?;
        for (peer, sender) in self.peers.lock().iter() {
            if !application.presence_queried.contains(peer)
                && sender
                    .send(&Frame::Presence {
                        app: app.clone(),
                        publication: Publication::Sync,
                    })
                    .is_ok()
            {
                application.presence_queried.insert(peer.clone());
            }
        }
        let members = application.presence.snapshot(&channel, &self.local);
        // Do not truncate a snapshot and falsely report that members left.
        if serde_json::to_vec(&members)
            .map_err(|_| error("invalid presence snapshot"))?
            .len()
            > wire::MAX_FRAME - 1024
        {
            return Err(error("presence snapshot exceeds frame limit"));
        }
        Ok(members)
    }
    pub(super) async fn remove_presence(
        &mut self,
        app: &AppEnv,
        channel: String,
        connection_id: String,
    ) -> io::Result<()> {
        presence::validate_channel(&channel)?;
        let connection_id = presence::local_id(&self.local, &connection_id)?;
        let application = self
            .apps
            .get_mut(app)
            .ok_or_else(|| error("unknown presence app"))?;
        application
            .presence
            .leave(&self.local, &channel, &connection_id);
        let sequence = application.presence.next_sequence()?;
        if !self.fanout(&Frame::Presence {
            app: app.clone(),
            publication: Publication::Leave {
                channel,
                connection_id,
                sequence,
            },
        }) {
            self.fence().await?;
        }
        Ok(())
    }
    pub(super) fn remote_presence(
        &mut self,
        peer: &DeviceId,
        app: &AppEnv,
        publication: Publication,
    ) -> io::Result<()> {
        if let Some(application) = self.apps.get_mut(app) {
            application.presence.expire(Instant::now());
        }
        let allowance = self.presence_allowance(app);
        let Some(application) = self.apps.get_mut(app) else {
            return Ok(());
        };
        let publication = match publication {
            Publication::Sync => {
                let snapshot = application.presence.local_snapshot(&self.local);
                if let Some(sender) = self.peers.lock().get(peer) {
                    sender.send(&Frame::Presence {
                        app: app.clone(),
                        publication: snapshot,
                    })?;
                }
                return Ok(());
            }
            Publication::Snapshot { sequence, leases } => {
                return application.presence.replace_remote(
                    peer,
                    sequence,
                    leases,
                    now_ms(),
                    allowance,
                );
            }
            publication => publication,
        };
        let sequence = publication.sequence();
        if !application.presence.fresh(peer, sequence) {
            return Ok(());
        }
        match publication {
            Publication::Update {
                channel,
                member,
                expires_at_ms,
                ..
            } => {
                let bytes = application
                    .presence
                    .check(peer, &channel, &member, allowance)?;
                if let Some(remaining) = presence::remaining(expires_at_ms, now_ms()) {
                    application.presence.update(
                        peer.clone(),
                        channel,
                        member,
                        Instant::now() + remaining,
                        expires_at_ms,
                        bytes,
                    );
                } else {
                    application
                        .presence
                        .leave(peer, &channel, &member.connection_id);
                }
            }
            Publication::Sync | Publication::Snapshot { .. } => {
                unreachable!("presence control dispatched above")
            }
            Publication::Leave {
                channel,
                connection_id,
                ..
            } => {
                presence::validate_channel(&channel)?;
                presence::validate_owned_id(peer, &connection_id)?;
                application.presence.leave(peer, &channel, &connection_id);
            }
        }
        application.presence.received(peer.clone(), sequence);
        Ok(())
    }
}
