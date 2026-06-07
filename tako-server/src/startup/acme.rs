use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::ServerState;
use crate::boot::certificate_renewal_task;
use crate::tls::{AcmeClient, AcmeConfig, CertManager, ChallengeTokens};
use tokio::runtime::Runtime;

pub(super) struct AcmeInitConfig {
    pub(super) standby: bool,
    pub(super) acme_staging: bool,
    pub(super) acme_email: Option<String>,
    pub(super) no_acme: bool,
    pub(super) account_dir: PathBuf,
    pub(super) cert_manager: Arc<CertManager>,
    pub(super) challenge_tokens: ChallengeTokens,
}

pub(super) fn init_acme_client(rt: &Runtime, config: AcmeInitConfig) -> Option<Arc<AcmeClient>> {
    if config.no_acme || config.standby {
        if config.standby {
            tracing::info!("ACME disabled (standby mode)");
        } else {
            tracing::info!("ACME disabled, using manual certificate management");
        }
        return None;
    }

    let client = Arc::new(AcmeClient::with_tokens(
        AcmeConfig {
            staging: config.acme_staging,
            email: config.acme_email,
            account_dir: config.account_dir,
            ..Default::default()
        },
        config.cert_manager,
        config.challenge_tokens,
    ));

    if let Err(e) = rt.block_on(client.init()) {
        tracing::error!("Failed to initialize ACME client: {}", e);
        tracing::warn!("Continuing without ACME - certificates must be managed manually");
        None
    } else {
        if config.acme_staging {
            tracing::warn!(
                "Using Let's Encrypt STAGING environment - certificates will NOT be trusted!"
            );
        } else {
            tracing::info!("ACME client initialized with Let's Encrypt production");
        }
        Some(client)
    }
}

pub(super) fn spawn_certificate_renewals(
    rt: &Runtime,
    state: Arc<ServerState>,
    renewal_interval_hours: u64,
) {
    if !state.runtime.no_acme && !state.runtime.standby {
        rt.spawn(certificate_renewal_task(
            state,
            Duration::from_secs(renewal_interval_hours * 3600),
        ));
    }
}
