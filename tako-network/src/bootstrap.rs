//! Public messages carried over authenticated operator SSH, never a listener.
use crate::{
    DeviceId, DeviceName, EnrollmentRequest, NetworkId, RegistrySnapshot, ServerInfo, WgChallenge,
    WgProof,
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

pub const MAX_BOOTSTRAP_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum BootstrapRequest {
    Prepare {
        network: NetworkId,
        name: DeviceName,
        wireguard_endpoint: SocketAddr,
        ssh_port: u16,
        https_port: u16,
    },
    Answer {
        network: NetworkId,
        challenge: WgChallenge,
    },
    Install {
        snapshot: RegistrySnapshot,
    },
    Sign {
        network: NetworkId,
        server: DeviceId,
        request: crate::api::RpcRequest,
    },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum BootstrapResponse {
    Prepared {
        request: EnrollmentRequest,
        server: ServerInfo,
    },
    Proof {
        proof: WgProof,
    },
    Installed {
        network: NetworkId,
        device: DeviceId,
    },
    Signed {
        request: crate::api::SignedRequest,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_wire_format_rejects_unknown_fields() {
        let request = BootstrapRequest::Prepare {
            network: NetworkId::random().unwrap(),
            name: "server".parse().unwrap(),
            wireguard_endpoint: "192.0.2.1:51820".parse().unwrap(),
            ssh_port: 22,
            https_port: 443,
        };
        let mut json = serde_json::to_value(request).unwrap();
        assert_eq!(json["operation"], "prepare");
        assert!(serde_json::from_value::<BootstrapRequest>(json.clone()).is_ok());
        json["keys_path"] = "/tmp/client-chosen-key".into();
        assert!(serde_json::from_value::<BootstrapRequest>(json).is_err());
    }

    #[test]
    fn installed_response_has_only_public_identifiers() {
        let json = serde_json::to_value(BootstrapResponse::Installed {
            network: NetworkId::random().unwrap(),
            device: "00".repeat(32).parse().unwrap(),
        })
        .unwrap();
        assert_eq!(json["result"], "installed");
        assert_eq!(json.as_object().unwrap().len(), 3);
        assert!(serde_json::from_value::<BootstrapResponse>(json).is_ok());
    }
}
