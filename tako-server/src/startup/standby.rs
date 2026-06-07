use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::ServerState;
use crate::boot::{PrimaryStatus, certificate_renewal_task, probe_primary_socket};
use crate::socket::SocketServer;
use crate::tls::{AcmeClient, AcmeConfig, CertManager, ChallengeTokens};
use tokio::runtime::Runtime;

pub(super) struct StandbyPromotionConfig {
    pub(super) socket_path: String,
    pub(super) state: Arc<ServerState>,
    pub(super) cert_manager: Arc<CertManager>,
    pub(super) acme_staging: bool,
    pub(super) acme_email: Option<String>,
    pub(super) no_acme: bool,
    pub(super) renewal_interval_hours: u64,
    pub(super) data_dir: PathBuf,
    pub(super) challenge_tokens: ChallengeTokens,
}

pub(super) fn spawn_standby_monitor(rt: &Runtime, config: StandbyPromotionConfig) {
    rt.spawn(async move {
        const PROBE_INTERVAL: Duration = Duration::from_secs(5);
        const FAILURE_THRESHOLD: u32 = 3;
        let mut consecutive_failures: u32 = 0;
        let mut promoted = false;
        let our_pid = std::process::id();

        loop {
            tokio::time::sleep(PROBE_INTERVAL).await;

            match probe_primary_socket(&config.socket_path, our_pid).await {
                PrimaryStatus::Alive => {
                    if promoted {
                        tracing::info!("Primary server is back — standby shutting down");
                        #[cfg(unix)]
                        unsafe {
                            libc::kill(libc::getpid(), libc::SIGTERM);
                        }
                        break;
                    }
                    if consecutive_failures > 0 {
                        tracing::debug!("Primary socket is alive again");
                    }
                    consecutive_failures = 0;
                }
                PrimaryStatus::IsUs => {
                    consecutive_failures = 0;
                }
                PrimaryStatus::Down => {
                    consecutive_failures += 1;
                    tracing::warn!(
                        failures = consecutive_failures,
                        "Primary management socket not responding"
                    );

                    if consecutive_failures >= FAILURE_THRESHOLD && !promoted {
                        tracing::info!("Promoting standby to full mode");

                        let server = SocketServer::new(&config.socket_path);
                        match server.bind() {
                            Ok(listener) => {
                                let socket_state = config.state.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = SocketServer::serve(listener, move |cmd| {
                                        let state = socket_state.clone();
                                        async move { state.handle_command(cmd).await }
                                    })
                                    .await
                                    {
                                        tracing::error!("Socket server error after promotion: {e}");
                                    }
                                });
                                std::mem::forget(server);
                                tracing::info!("Management socket bound after promotion");
                            }
                            Err(e) => {
                                tracing::error!(
                                    "Failed to bind management socket on promotion: {e}"
                                );
                                consecutive_failures = 0;
                                continue;
                            }
                        }

                        if !config.no_acme {
                            let client = Arc::new(AcmeClient::with_tokens(
                                AcmeConfig {
                                    staging: config.acme_staging,
                                    email: config.acme_email.clone(),
                                    account_dir: config.data_dir.join("acme"),
                                    ..Default::default()
                                },
                                config.cert_manager.clone(),
                                config.challenge_tokens.clone(),
                            ));
                            match client.init().await {
                                Ok(()) => {
                                    tracing::info!("ACME initialized after promotion");
                                    config.state.set_acme_client(client).await;
                                    tokio::spawn(certificate_renewal_task(
                                        config.state.clone(),
                                        Duration::from_secs(config.renewal_interval_hours * 3600),
                                    ));
                                }
                                Err(e) => {
                                    tracing::error!("Failed to init ACME after promotion: {e}");
                                }
                            }
                        }

                        promoted = true;
                        consecutive_failures = 0;
                        tracing::info!("Promotion complete — standby now running as full server");
                    }
                }
            }
        }
    });
}
