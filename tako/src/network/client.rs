use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use tako_network::api::{RpcRequest, RpcResponse, SignedRequest};
use tako_network::{DeviceId, LocalDeviceKeys, NetworkPublicProfile, RegistrySnapshot, ServerInfo};

use super::{Profile, Result};
use crate::config::NetworkAlias;

pub(crate) fn hostname(network: &tako_network::NetworkId, server: &DeviceId) -> String {
    format!(
        "node-{}.{}.tako.internal",
        &server.to_string()[..32],
        network
    )
}

fn trusted_servers(profile: &Profile) -> Result<BTreeMap<DeviceId, ServerInfo>> {
    match profile {
        Profile::Created { .. } => {
            Err("Add the first server before using network management".into())
        }
        Profile::Pending { profile } => Ok(profile.trusted_servers.clone()),
        Profile::Active { profile } => super::current_server_pins(profile, super::now()?),
    }
}

pub(crate) fn call(
    profile: &Profile,
    server: &DeviceId,
    command: RpcRequest,
) -> Result<RpcResponse> {
    let keys = LocalDeviceKeys::load(&super::keys_path(&profile.request().request_id)?)?;
    profile.validate(&keys)?;
    let trusted = trusted_servers(profile)?;
    let info = trusted
        .get(server)
        .ok_or("Server is not trusted by this network profile")?;
    let request = SignedRequest::sign(
        &keys,
        profile.snapshot().network.id.clone(),
        server.clone(),
        profile.request().kind,
        super::now()?,
        command,
    )?;
    send(info, &request)
}

pub(crate) fn send(info: &ServerInfo, signed: &SignedRequest) -> Result<RpcResponse> {
    let (http, request) = prepare_request(info, signed)?;
    let response = http.execute(request)?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "Network request was rejected by {} ({status})",
            signed.server_id
        )
        .into());
    }
    super::read_bounded_json(response)
}

fn prepare_request(
    info: &ServerInfo,
    signed: &SignedRequest,
) -> Result<(reqwest::blocking::Client, reqwest::blocking::Request)> {
    info.validate()?;
    let certificate = reqwest::Certificate::from_pem(
        info.https_certificate_pem
            .as_ref()
            .ok_or("Server has no pinned HTTPS certificate")?
            .as_bytes(),
    )?;
    let host = hostname(&signed.network_id, &signed.server_id);
    let http = reqwest::blocking::Client::builder()
        .tls_certs_only([certificate])
        .resolve(
            &host,
            SocketAddr::new(info.wireguard_endpoint.ip(), info.https_port),
        )
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(20))
        .build()?;
    let path = signed.request.path();
    let request = http
        .post(format!("https://{host}:{}{path}", info.https_port))
        .json(signed)
        .build()?;
    Ok((http, request))
}

pub(crate) fn server(profile: &Profile) -> Result<DeviceId> {
    if let Profile::Pending { profile } = profile {
        return Ok(profile.issuer.clone());
    }
    trusted_servers(profile)?
        .into_keys()
        .next()
        .ok_or_else(|| "Network has no trusted servers".into())
}

pub(crate) fn refresh(alias: &NetworkAlias) -> Result<Profile> {
    let mut profile = super::load_profile(alias)?;
    if let Profile::Pending { profile: pending } = &profile {
        let issuer = pending.issuer.clone();
        let pending_id = pending.pending_id.clone();
        let response = call(
            &profile,
            &issuer,
            RpcRequest::Begin {
                pending_id: pending_id.clone(),
            },
        )?;
        let active = match response {
            RpcResponse::Active(active) => active,
            RpcResponse::Challenge {
                challenge_id,
                challenge,
            } => {
                let keys = LocalDeviceKeys::load(&super::keys_path(&pending.request.request_id)?)?;
                let proof = keys.answer_challenge(&challenge, &pending.request, super::now()?)?;
                match call(
                    &profile,
                    &issuer,
                    RpcRequest::Complete {
                        pending_id,
                        challenge_id,
                        proof,
                    },
                )? {
                    RpcResponse::Active(active) => active,
                    _ => return Err("Unexpected network activation response".into()),
                }
            }
            _ => return Err("Unexpected network enrollment challenge response".into()),
        };
        let keys = LocalDeviceKeys::load(&super::keys_path(&profile.request().request_id)?)?;
        profile = accept_activation(&profile, active, &keys)?;
        super::save_profile(&alias.name, &profile)?;
        profile = super::load_profile(alias)?;
    }
    let servers = trusted_servers(&profile)?;
    let mut last_error = None;
    for server in servers.keys() {
        match call(&profile, server, RpcRequest::Snapshot) {
            Ok(RpcResponse::Snapshot(snapshot)) => {
                let refreshed = active_from_snapshot(&profile, snapshot)?;
                super::save_profile(&alias.name, &refreshed)?;
                return super::load_profile(alias);
            }
            Ok(_) => return Err("Unexpected network snapshot response".into()),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| "No network server is reachable".into()))
}

fn accept_activation(
    profile: &Profile,
    active: NetworkPublicProfile,
    keys: &LocalDeviceKeys,
) -> Result<Profile> {
    if active.request != *profile.request() || active.snapshot.network != profile.snapshot().network
    {
        return Err("Activation response does not match the pending enrollment".into());
    }
    let active = Profile::Active { profile: active };
    active.validate_for_storage(keys)?;
    active_from_snapshot(profile, active.snapshot().clone())
}

fn active_from_snapshot(profile: &Profile, mut snapshot: RegistrySnapshot) -> Result<Profile> {
    super::merge_snapshot(&mut snapshot, profile.snapshot())?;
    let servers = super::server_members_from_snapshot(&snapshot)?;
    Ok(Profile::Active {
        profile: NetworkPublicProfile {
            request: profile.request().clone(),
            trusted_servers: servers,
            snapshot,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::{Profile, now, tests::fixture};

    #[test]
    fn refresh_merges_revocations_before_rebuilding_trust() {
        let fixture = fixture();
        let stale = fixture.profile.snapshot().clone();
        let mut previous = fixture.profile.clone();
        let Profile::Active { profile } = &mut previous else {
            panic!()
        };
        let revocation = profile
            .snapshot
            .plan_revocation(
                tako_network::RevocationId::random().unwrap(),
                &profile.snapshot.network.initial_admin,
                &fixture.server,
                now().unwrap(),
            )
            .unwrap();
        profile.snapshot.revocations.push(revocation.clone());
        profile.trusted_servers.clear();
        let refreshed = active_from_snapshot(&previous, stale).unwrap();
        assert_eq!(refreshed.snapshot().revocations, vec![revocation]);
        assert!(trusted_servers(&refreshed).unwrap().is_empty());
        assert!(refreshed.validate(&fixture.keys).is_err());
        let merged = active_from_snapshot(&refreshed, refreshed.snapshot().clone()).unwrap();
        assert_eq!(merged.snapshot(), refreshed.snapshot());
    }

    #[test]
    fn snapshots_reject_conflicting_records_and_network_identity() {
        let fixture = fixture();
        let mut snapshot = fixture.profile.snapshot().clone();
        snapshot.members[0].request.name = "tampered".parse().unwrap();
        assert!(active_from_snapshot(&fixture.profile, snapshot).is_err());
        let mut snapshot = fixture.profile.snapshot().clone();
        snapshot.network.initial_admin = fixture.server;
        assert!(active_from_snapshot(&fixture.profile, snapshot).is_err());
    }

    #[test]
    fn activation_binds_the_exact_request_and_keeps_known_denials() {
        let fixture = fixture();
        let Profile::Active {
            profile: mut active,
        } = fixture.profile.clone()
        else {
            panic!()
        };
        let mut pending = Profile::Pending {
            profile: tako_network::PendingApprovalProfile {
                request: active.request.clone(),
                pending_id: tako_network::RequestId::random().unwrap(),
                issuer: fixture.server.clone(),
                expires_at: now().unwrap() + 60,
                trusted_servers: active.trusted_servers.clone(),
                snapshot: active.snapshot.clone(),
            },
        };
        accept_activation(&pending, active.clone(), &fixture.keys).unwrap();
        active.request = fixture
            .keys
            .enrollment_request(
                Some(active.snapshot.network.id.clone()),
                tako_network::DeviceKind::Laptop,
                "changed".parse().unwrap(),
            )
            .unwrap();
        assert!(accept_activation(&pending, active, &fixture.keys).is_err());
        let Profile::Active { profile: active } = fixture.profile.clone() else {
            panic!()
        };
        let Profile::Pending { profile } = &mut pending else {
            panic!()
        };
        let revocation = profile
            .snapshot
            .plan_revocation(
                tako_network::RevocationId::random().unwrap(),
                &profile.snapshot.network.initial_admin,
                &fixture.server,
                now().unwrap(),
            )
            .unwrap();
        profile.snapshot.revocations.push(revocation.clone());
        let merged = accept_activation(&pending, active, &fixture.keys).unwrap();
        assert_eq!(merged.snapshot().revocations, vec![revocation]);
        assert!(merged.validate(&fixture.keys).is_err());
    }

    #[test]
    fn transport_uses_the_signed_network_server_and_command_audience() {
        let fixture = fixture();
        let info = trusted_servers(&fixture.profile)
            .unwrap()
            .remove(&fixture.server)
            .unwrap();
        for command in [
            RpcRequest::Snapshot,
            RpcRequest::Begin {
                pending_id: tako_network::RequestId::random().unwrap(),
            },
        ] {
            let signed = SignedRequest::sign(
                &fixture.keys,
                fixture.profile.snapshot().network.id.clone(),
                fixture.server.clone(),
                fixture.profile.request().kind,
                now().unwrap(),
                command,
            )
            .unwrap();
            let (_, request) = prepare_request(&info, &signed).unwrap();
            assert_eq!(request.method(), reqwest::Method::POST);
            assert_eq!(request.url().scheme(), "https");
            assert_eq!(
                request.url().host_str(),
                Some(hostname(&signed.network_id, &signed.server_id).as_str())
            );
            assert_eq!(request.url().port(), Some(info.https_port));
            assert_eq!(request.url().path(), signed.request.path());
            let body: SignedRequest =
                serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
            assert_eq!(body, signed);
        }
    }

    #[test]
    fn transport_rejects_missing_and_mismatched_pins_before_connecting() {
        let fixture = fixture();
        let mut info = trusted_servers(&fixture.profile)
            .unwrap()
            .remove(&fixture.server)
            .unwrap();
        let signed = SignedRequest::sign(
            &fixture.keys,
            fixture.profile.snapshot().network.id.clone(),
            fixture.server,
            fixture.profile.request().kind,
            now().unwrap(),
            RpcRequest::Snapshot,
        )
        .unwrap();
        info.https_spki_sha256 = Some(tako_network::KeyFingerprint::of_bytes(b"wrong"));
        assert!(
            send(&info, &signed)
                .unwrap_err()
                .to_string()
                .contains("SPKI")
        );
        info.https_spki_sha256 = None;
        info.https_certificate_pem = None;
        assert!(
            send(&info, &signed)
                .unwrap_err()
                .to_string()
                .contains("pinned HTTPS")
        );
        assert!(
            hostname(&signed.network_id, &signed.server_id)
                .contains(&signed.network_id.to_string())
        );
        assert!(
            hostname(&signed.network_id, &signed.server_id)
                .contains(&signed.server_id.to_string()[..32])
        );
    }
}
