use serde::{Deserialize, Serialize};
use tako_network::{
    DeviceId, EnrollmentRequest, LocalDeviceKeys, Member, MemberKind, NetworkDescriptor,
    RegistrySnapshot, Revocation, RevocationId, ServerInfo,
    api::{RpcRequest, RpcResponse, SignedRequest},
};

use super::{Profile, Result};
use crate::config::LocalNetwork;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    alias: LocalNetwork,
    network: NetworkDescriptor,
    request: EnrollmentRequest,
    issuer: DeviceId,
    pin: ServerInfo,
    expected: Revocation,
    recorded: Option<Revocation>,
}

impl Journal {
    fn validate(
        &self,
        alias: &LocalNetwork,
        profile: &Profile,
        keys: &LocalDeviceKeys,
    ) -> Result<()> {
        alias.validate()?;
        self.request.verify()?;
        let snapshot = profile.snapshot();
        snapshot.validate()?;
        let own = self.request.device_id_for(&self.network.id)?;
        if self.alias != *alias
            || self.network != snapshot.network
            || self.network.id.to_string() != alias.id
            || own.to_string() != alias.device
            || own != self.expected.by
            || profile.request() != &self.request
            || self.request.wg_public_key != keys.wg_public_key()?
            || self.request.ssh_public_key != keys.ssh_public_key()?
            || !snapshot.members.iter().any(|member| {
                member.wg_verified && member.device_id == own && member.request == self.request
            })
        {
            return Err("Saved revocation does not match this network and device identity".into());
        }
        let Profile::Active { profile } = profile else {
            return Err("Saved revocation requires its original network profile".into());
        };
        // Historical bindings authorize only this exact retry, even after local revocation.
        super::current_server_pins(&profile.snapshot, &profile.trusted_servers, super::now()?)?;
        self.pin.validate()?;
        if self.pin.https_certificate_pem.is_none()
            || !snapshot.members.iter().any(|member| {
                member.wg_verified
                    && member.device_id == self.issuer
                    && matches!(&member.kind, MemberKind::Server(pin) if pin == &self.pin)
            })
        {
            return Err(
                "Saved revocation issuer pin does not match its recorded membership".into(),
            );
        }
        let target = target(snapshot, &self.expected.target.to_string(), false, 0)?;
        if target.request.wg_public_key.fingerprint() != self.expected.wg_fingerprint
            || target.request.ssh_public_key.fingerprint()? != self.expected.ssh_fingerprint
        {
            return Err("Saved revocation target keys changed".into());
        }
        if let Some(recorded) = &self.recorded {
            self.validate_response(recorded)?;
        }
        for observed in &snapshot.revocations {
            if observed.id == self.expected.id {
                self.validate_response(observed)?;
                if self
                    .recorded
                    .as_ref()
                    .is_some_and(|recorded| recorded != observed)
                {
                    return Err("Conflicting recorded revocation".into());
                }
            }
        }
        Ok(())
    }

    fn validate_response(&self, response: &Revocation) -> Result<()> {
        if response.id != self.expected.id
            || response.target != self.expected.target
            || response.by != self.expected.by
            || response.wg_fingerprint != self.expected.wg_fingerprint
            || response.ssh_fingerprint != self.expected.ssh_fingerprint
        {
            return Err("Revocation response does not match the saved attempt".into());
        }
        Ok(())
    }

    fn signed(&self, keys: &LocalDeviceKeys, now: u64) -> Result<SignedRequest> {
        Ok(SignedRequest::sign(
            keys,
            self.network.id.clone(),
            self.issuer.clone(),
            self.request.kind,
            now,
            RpcRequest::Revoke {
                id: self.expected.id.clone(),
                target: self.expected.target.clone(),
            },
        )?)
    }
}

fn target<'a>(
    snapshot: &'a RegistrySnapshot,
    device: &str,
    effective_only: bool,
    now: u64,
) -> Result<&'a Member> {
    let members = if effective_only {
        snapshot.effective_members(now)?
    } else {
        snapshot.validate()?;
        snapshot
            .members
            .iter()
            .filter(|member| member.wg_verified)
            .map(|member| (member.device_id.clone(), member))
            .collect()
    };
    let mut matches = members.into_values().filter(|member| {
        member.device_id.to_string() == device || member.request.name.as_str() == device
    });
    match (matches.next(), matches.next()) {
        (Some(member), None) => Ok(member),
        _ => Err("Device must uniquely match a name or device ID".into()),
    }
}

/// Local projection only; does not create a journal or contact a server.
pub(crate) fn project(alias: &LocalNetwork, device: &str) -> Result<Revocation> {
    let path = alias.profile_path()?;
    if let Some(journal) = read_journal(&path.with_file_name("revocation.json"))? {
        let profile: Profile = super::read_json(&path)?;
        let keys = LocalDeviceKeys::load(&super::keys_path(&journal.request.request_id)?)?;
        journal.validate(alias, &profile, &keys)?;
        if target(profile.snapshot(), device, false, super::now()?)?.device_id
            == journal.expected.target
        {
            return Ok(journal.recorded.unwrap_or(journal.expected));
        }
        if journal.recorded.is_none() {
            return Err("Another revocation is still pending for this identity".into());
        }
    }
    let profile = super::load_profile(alias)?;
    let now = super::now()?;
    let member = target(profile.snapshot(), device, true, now)?;
    Ok(profile.snapshot().plan_revocation(
        RevocationId::random()?,
        &profile
            .request()
            .device_id_for(&profile.snapshot().network.id)?,
        &member.device_id,
        now,
    )?)
}

fn read_journal(path: &std::path::Path) -> Result<Option<Journal>> {
    match super::private_file(path, false) {
        Ok(file) => Ok(Some(super::read_bounded_json(file)?)),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn run(alias: &LocalNetwork, device: &str) -> Result<Revocation> {
    run_with(
        alias,
        device,
        super::client::refresh,
        super::client::send,
        super::now,
    )
}

fn run_with(
    alias: &LocalNetwork,
    device: &str,
    mut refresh: impl FnMut(&LocalNetwork) -> Result<Profile>,
    mut send: impl FnMut(&ServerInfo, &SignedRequest) -> Result<RpcResponse>,
    mut now: impl FnMut() -> Result<u64>,
) -> Result<Revocation> {
    let profile_path = alias.profile_path()?;
    super::private_directory(profile_path.parent().unwrap())?;
    let lock = super::private_file(&profile_path.with_file_name("revocation.lock"), true)?;
    lock.lock()?;
    let path = profile_path.with_file_name("revocation.json");
    let saved = read_journal(&path)?;
    if let Some(journal) = saved.as_ref() {
        let profile: Profile = super::read_json(&profile_path)?;
        let keys = LocalDeviceKeys::load(&super::keys_path(&journal.request.request_id)?)?;
        journal.validate(alias, &profile, &keys)?;
        let requested = target(profile.snapshot(), device, false, now()?)?;
        if requested.device_id == journal.expected.target {
            return publish(alias, &path, journal.clone(), &keys, &mut send, now()?);
        }
        let Some(recorded) = &journal.recorded else {
            return Err(format!(
                "Revocation {} is still pending for {}; retry that device before another revocation",
                journal.expected.id, journal.expected.target
            ).into());
        };
        super::record_revocation(alias, recorded.clone())?;
    }

    let profile = refresh(alias)?;
    let keys = LocalDeviceKeys::load(&super::keys_path(&profile.request().request_id)?)?;
    profile.validate(&keys)?;
    let snapshot = profile.snapshot();
    let planned_at = now()?;
    let member = target(snapshot, device, true, planned_at)?;
    let expected = snapshot.plan_revocation(
        RevocationId::random()?,
        &profile.request().device_id_for(&snapshot.network.id)?,
        &member.device_id,
        planned_at,
    )?;
    let Profile::Active { profile: active } = &profile else {
        return Err("Revocation requires an active network profile".into());
    };
    let mut candidates =
        super::current_server_pins(&active.snapshot, &active.trusted_servers, planned_at)?;
    let mut prospective = snapshot.clone();
    prospective.revocations.push(expected.clone());
    let survivors = prospective.effective_members(planned_at)?;
    if candidates.keys().any(|id| survivors.contains_key(id)) {
        candidates.retain(|id, _| survivors.contains_key(id));
    }
    let mut last_error = None;
    let mut issuer = None;
    for (server, pin) in candidates {
        let probe = SignedRequest::sign(
            &keys,
            snapshot.network.id.clone(),
            server.clone(),
            profile.request().kind,
            now()?,
            RpcRequest::Snapshot,
        )?;
        match send(&pin, &probe) {
            Ok(RpcResponse::Snapshot(observed)) if observed.network == snapshot.network => {
                observed.validate()?;
                issuer = Some((server, pin));
                break;
            }
            Ok(_) => return Err("Unexpected network snapshot response".into()),
            Err(error) => last_error = Some(error),
        }
    }
    let (issuer, pin) = issuer.ok_or_else(|| {
        last_error.unwrap_or_else(|| "No network server is reachable for revocation".into())
    })?;
    let journal = Journal {
        alias: alias.clone(),
        network: snapshot.network.clone(),
        request: profile.request().clone(),
        issuer,
        pin,
        expected,
        recorded: None,
    };
    journal.validate(alias, &profile, &keys)?;
    super::write_json(&path, &journal, saved.is_some())?;
    publish(alias, &path, journal, &keys, &mut send, now()?)
}

fn publish(
    alias: &LocalNetwork,
    path: &std::path::Path,
    mut journal: Journal,
    keys: &LocalDeviceKeys,
    send: &mut impl FnMut(&ServerInfo, &SignedRequest) -> Result<RpcResponse>,
    now: u64,
) -> Result<Revocation> {
    let result = (|| -> Result<Revocation> {
        if journal.recorded.is_none() {
            let signed = journal.signed(keys, now)?;
            let RpcResponse::Revoked(recorded) = send(&journal.pin, &signed)? else {
                return Err("Unexpected revocation response".into());
            };
            journal.validate_response(&recorded)?;
            journal.recorded = Some(recorded);
            super::write_json(path, &journal, true)?;
        }
        let recorded = journal.recorded.as_ref().unwrap();
        super::record_revocation(alias, recorded.clone())?;
        Ok(recorded.clone())
    })();
    result.map_err(|error| {
        format!(
        "Revocation {} remains pending with issuer {}; retry device {} on network '{}': {error}",
        journal.expected.id, journal.issuer, journal.expected.target, alias.name
    ).into()
    })
}

#[cfg(test)]
#[path = "revocation/tests.rs"]
mod tests;
