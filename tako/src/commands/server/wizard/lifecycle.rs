use crate::output;
use crate::vpn::VpnProvider;
use tracing::Instrument;

use super::connection::{
    WizardConnectionResult, check_tako_connection, configure_tako_server_with_service_user,
    install_tako_server_with_admin,
};
use super::ports::ServerPublicPorts;
use crate::ssh::SshConfig;

#[derive(Clone, Copy)]
pub(super) struct VerifyLabels {
    pub(super) progress: &'static str,
    pub(super) success: &'static str,
    pub(super) failure: &'static str,
}

impl VerifyLabels {
    pub(super) const INSTALL: Self = Self {
        progress: "Verifying install",
        success: "Install verified",
        failure: "Verification failed",
    };

    pub(super) const SERVER: Self = Self {
        progress: "Verifying server",
        success: "Server verified",
        failure: "Verification failed",
    };
}

pub(super) async fn install_start_and_verify(
    ssh_config: &SshConfig,
    admin_user: &str,
    public_ports: ServerPublicPorts,
    vpn: VpnProvider,
    verify_labels: VerifyLabels,
    scoped_timing: bool,
) -> Result<WizardConnectionResult, Box<dyn std::error::Error>> {
    install_tako_server(ssh_config, admin_user, public_ports, vpn, scoped_timing).await?;
    start_tako_server(ssh_config, public_ports, vpn).await?;
    verify_tako_server(ssh_config, vpn, verify_labels, scoped_timing)
        .await
        .map_err(Into::into)
}

pub(super) async fn start_and_verify(
    ssh_config: &SshConfig,
    public_ports: ServerPublicPorts,
    vpn: VpnProvider,
    verify_labels: VerifyLabels,
    scoped_timing: bool,
) -> Result<WizardConnectionResult, Box<dyn std::error::Error>> {
    start_tako_server(ssh_config, public_ports, vpn).await?;
    verify_tako_server(ssh_config, vpn, verify_labels, scoped_timing)
        .await
        .map_err(Into::into)
}

async fn install_tako_server(
    ssh_config: &SshConfig,
    admin_user: &str,
    public_ports: ServerPublicPorts,
    vpn: VpnProvider,
    scoped_timing: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let install = install_tako_server_with_admin(
        ssh_config,
        admin_user,
        Some(public_ports),
        crate::ssh::InstallServerMode::BootstrapOnly,
        vpn,
    );
    if scoped_timing {
        let install_scope = output::scope(&ssh_config.host);
        let _t = output::timed(&format!(
            "Install tako-server on {}:{}",
            ssh_config.host, ssh_config.port
        ));
        output::with_spinner_async_err(
            "Installing tako-server",
            "tako-server installed",
            "Install failed",
            install.instrument(install_scope),
        )
        .await?;
        drop(_t);
    } else {
        output::with_spinner_async_err(
            "Installing tako-server",
            "tako-server installed",
            "Install failed",
            install,
        )
        .await?;
    }

    Ok(())
}

pub(super) async fn start_tako_server(
    ssh_config: &SshConfig,
    public_ports: ServerPublicPorts,
    vpn: VpnProvider,
) -> Result<(), Box<dyn std::error::Error>> {
    let start_scope = output::scope(&ssh_config.host);
    let _t = output::timed(&format!(
        "Start tako-server on {}:{}",
        ssh_config.host, ssh_config.port
    ));
    output::with_spinner_async_err(
        "Starting tako-server",
        "tako-server started",
        "Start failed",
        configure_tako_server_with_service_user(ssh_config, Some(public_ports), vpn)
            .instrument(start_scope),
    )
    .await?;
    drop(_t);

    Ok(())
}

async fn verify_tako_server(
    ssh_config: &SshConfig,
    vpn: VpnProvider,
    labels: VerifyLabels,
    scoped_timing: bool,
) -> Result<WizardConnectionResult, String> {
    if scoped_timing {
        let verify_scope = output::scope(&ssh_config.host);
        let _t = output::timed(&format!(
            "Verify tako-server on {}:{}",
            ssh_config.host, ssh_config.port
        ));
        let result = output::with_spinner_async_err(
            labels.progress,
            labels.success,
            labels.failure,
            check_tako_connection(ssh_config, vpn).instrument(verify_scope),
        )
        .await;
        drop(_t);
        result
    } else {
        output::with_spinner_async_err(
            labels.progress,
            labels.success,
            labels.failure,
            check_tako_connection(ssh_config, vpn),
        )
        .await
    }
}
