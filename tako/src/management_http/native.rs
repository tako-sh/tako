use std::net::SocketAddr;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use reqwest::header::{HeaderMap, HeaderValue};
use sha2::{Digest, Sha256};
use tako_core::Command;
use tako_network::api::{ManagementAction, RpcRequest, SignedRequest};
use tako_network::{AppEnv, DeviceKind, LocalDeviceKeys, NetworkId, Permissions, ServerInfo};

use super::{MANAGEMENT_CONNECT_TIMEOUT, ManagementError, NativeTarget};
use crate::network;

pub(super) const AUTHORIZATION: &str = "x-tako-network-authorization";
pub(super) const MAX_LOG_BYTES: usize = 1024 * 1024;

pub(super) struct Identity {
    target: NativeTarget,
    network_id: NetworkId,
    kind: DeviceKind,
    keys: LocalDeviceKeys,
    pub(super) permissions: Permissions,
    info: ServerInfo,
}

fn error(error: impl std::fmt::Display) -> ManagementError {
    ManagementError::Message(error.to_string())
}

impl Identity {
    pub(super) fn load(target: &NativeTarget) -> Result<Self, ManagementError> {
        let profile = network::load_profile(&target.network).map_err(error)?;
        let network_id = profile.snapshot().network.id.clone();
        let keys = LocalDeviceKeys::load(
            &network::keys_path(&profile.request().request_id).map_err(error)?,
        )
        .map_err(error)?;
        let members = profile
            .snapshot()
            .effective_members(network::now().map_err(error)?)
            .map_err(error)?;
        let own = profile
            .request()
            .device_id_for(&network_id)
            .map_err(error)?;
        let permissions = members
            .get(&own)
            .ok_or_else(|| error("Local device is not an active network member"))?
            .permissions
            .clone();
        let info = match members.get(&target.server_id).map(|member| &member.kind) {
            Some(tako_network::MemberKind::Server(info)) => info.clone(),
            _ => return Err(error("Server is not a trusted active network member")),
        };
        Ok(Self {
            target: target.clone(),
            network_id,
            kind: profile.request().kind,
            keys,
            permissions,
            info,
        })
    }

    pub(super) fn client(&self, timeout: Duration) -> Result<reqwest::Client, ManagementError> {
        pinned_client(&self.info, &self.hostname(), timeout)
    }

    fn hostname(&self) -> String {
        network::client::hostname(&self.network_id, &self.target.server_id)
    }

    pub(super) fn url(&self, endpoint: &str) -> String {
        format!(
            "https://{}:{}/v1/management/{endpoint}",
            self.hostname(),
            self.info.https_port
        )
    }

    pub(super) fn headers(&self, action: ManagementAction) -> Result<HeaderMap, ManagementError> {
        action.validate().map_err(error)?;
        let signed = SignedRequest::sign(
            &self.keys,
            self.network_id.clone(),
            self.target.server_id.clone(),
            self.kind,
            network::now().map_err(error)?,
            RpcRequest::Authorize { action },
        )
        .map_err(error)?;
        let encoded = STANDARD.encode(serde_json::to_vec(&signed).map_err(error)?);
        if encoded.len() > 16 * 1024 {
            return Err(error("Native management authorization exceeds 16 KiB"));
        }
        let mut headers = HeaderMap::new();
        let mut authorization = HeaderValue::from_str(&encoded).map_err(error)?;
        authorization.set_sensitive(true);
        headers.insert(AUTHORIZATION, authorization);
        Ok(headers)
    }
}

fn pinned_client(
    info: &ServerInfo,
    hostname: &str,
    timeout: Duration,
) -> Result<reqwest::Client, ManagementError> {
    info.validate().map_err(error)?;
    let certificate = reqwest::Certificate::from_pem(
        info.https_certificate_pem
            .as_ref()
            .ok_or_else(|| error("Server has no pinned HTTPS certificate"))?
            .as_bytes(),
    )
    .map_err(error)?;
    reqwest::Client::builder()
        .tls_certs_only([certificate])
        .resolve(
            hostname,
            SocketAddr::new(info.wireguard_endpoint.ip(), info.https_port),
        )
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(MANAGEMENT_CONNECT_TIMEOUT)
        .timeout(timeout)
        .build()
        .map_err(error)
}

pub(super) fn scope(app: &str) -> Result<AppEnv, ManagementError> {
    if tako_core::split_deployment_app_id(app).is_none() {
        return Err(error(
            "Native management requires an exact app/environment identity",
        ));
    }
    app.parse().map_err(error)
}

pub(super) fn rpc_action(
    command: &Command,
    body: &[u8],
    preflight_scope: Option<&AppEnv>,
) -> Result<ManagementAction, ManagementError> {
    let app = match command {
        Command::PrepareRelease { app, .. }
        | Command::PrepareReleaseUpload { app, .. }
        | Command::PrepareDeploy { app, .. }
        | Command::CleanupPreparedDeploy { app, .. }
        | Command::CleanupRelease { app, .. }
        | Command::FinalizeRelease { app, .. }
        | Command::RunRelease { app, .. }
        | Command::Deploy { app, .. }
        | Command::BackupNow { app, .. }
        | Command::ListBackups { app }
        | Command::BackupStatus { app }
        | Command::BackupDownloadUrl { app, .. }
        | Command::RestoreBackup { app, .. }
        | Command::Scale { app, .. }
        | Command::Stop { app }
        | Command::Delete { app }
        | Command::Status { app }
        | Command::ListReleases { app }
        | Command::Rollback { app, .. }
        | Command::UpdateSecrets { app, .. }
        | Command::GetSecretsHash { app }
        | Command::EnqueueRun { app, .. }
        | Command::Signal { app, .. }
        | Command::ListWorkflowRuns { app, .. }
        | Command::GetWorkflowRun { app, .. }
        | Command::RetryWorkflowRun { app, .. }
        | Command::CancelWorkflowRun { app, .. } => Some(app),
        Command::CheckDeploySpace { .. } => {
            return Ok(ManagementAction::Rpc {
                scope: preflight_scope.cloned(),
                sha256: hex::encode(Sha256::digest(body)),
            });
        }
        Command::Hello { .. }
        | Command::ServerInfo
        | Command::List
        | Command::Routes
        | Command::DrainServer { .. }
        | Command::ResumeServer
        | Command::EnterUpgrading { .. }
        | Command::ExitUpgrading { .. }
        | Command::InjectChallengeToken { .. } => None,
        Command::RegisterSchedules { .. }
        | Command::ClaimRun { .. }
        | Command::HeartbeatRun { .. }
        | Command::SaveStep { .. }
        | Command::GetRunResult { .. }
        | Command::CompleteRun { .. }
        | Command::CancelRun { .. }
        | Command::FailRun { .. }
        | Command::DeferRun { .. }
        | Command::WaitForEvent { .. }
        | Command::NetworkCall { .. }
        | Command::ChannelPublish { .. }
        | Command::ChannelClose { .. } => {
            return Err(error(
                "Internal worker commands are unavailable over native management",
            ));
        }
    };
    Ok(ManagementAction::Rpc {
        scope: app.map(|app| scope(app)).transpose()?,
        sha256: hex::encode(Sha256::digest(body)),
    })
}

#[cfg(test)]
mod tests;
