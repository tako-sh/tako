use crate::output;
use tracing::Instrument;

use super::connection::{
    WizardConnectionResult, check_tako_connection, configure_tako_server_with_service_user,
    install_tako_server_with_admin,
};
use super::ports::ServerPublicPorts;

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
    host: &str,
    port: u16,
    admin_user: &str,
    public_ports: ServerPublicPorts,
    verify_labels: VerifyLabels,
    scoped_timing: bool,
) -> Result<WizardConnectionResult, Box<dyn std::error::Error>> {
    install_tako_server(host, port, admin_user, public_ports, scoped_timing).await?;
    start_tako_server(host, port, public_ports).await?;
    verify_tako_server(host, port, verify_labels, scoped_timing)
        .await
        .map_err(Into::into)
}

pub(super) async fn start_and_verify(
    host: &str,
    port: u16,
    public_ports: ServerPublicPorts,
    verify_labels: VerifyLabels,
    scoped_timing: bool,
) -> Result<WizardConnectionResult, Box<dyn std::error::Error>> {
    start_tako_server(host, port, public_ports).await?;
    verify_tako_server(host, port, verify_labels, scoped_timing)
        .await
        .map_err(Into::into)
}

async fn install_tako_server(
    host: &str,
    port: u16,
    admin_user: &str,
    public_ports: ServerPublicPorts,
    scoped_timing: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if scoped_timing {
        let install_scope = output::scope(host);
        let _t = output::timed(&format!("Install tako-server on {host}:{port}"));
        output::with_spinner_async_err(
            "Installing tako-server",
            "tako-server installed",
            "Install failed",
            install_tako_server_with_admin(
                host,
                port,
                admin_user,
                Some(public_ports),
                crate::ssh::InstallServerMode::BootstrapOnly,
            )
            .instrument(install_scope),
        )
        .await?;
        drop(_t);
    } else {
        output::with_spinner_async_err(
            "Installing tako-server",
            "tako-server installed",
            "Install failed",
            install_tako_server_with_admin(
                host,
                port,
                admin_user,
                Some(public_ports),
                crate::ssh::InstallServerMode::BootstrapOnly,
            ),
        )
        .await?;
    }

    Ok(())
}

pub(super) async fn start_tako_server(
    host: &str,
    port: u16,
    public_ports: ServerPublicPorts,
) -> Result<(), Box<dyn std::error::Error>> {
    let start_scope = output::scope(host);
    let _t = output::timed(&format!("Start tako-server on {host}:{port}"));
    output::with_spinner_async_err(
        "Starting tako-server",
        "tako-server started",
        "Start failed",
        configure_tako_server_with_service_user(host, port, Some(public_ports))
            .instrument(start_scope),
    )
    .await?;
    drop(_t);

    Ok(())
}

async fn verify_tako_server(
    host: &str,
    port: u16,
    labels: VerifyLabels,
    scoped_timing: bool,
) -> Result<WizardConnectionResult, String> {
    if scoped_timing {
        let verify_scope = output::scope(host);
        let _t = output::timed(&format!("Verify tako-server on {host}:{port}"));
        let result = output::with_spinner_async_err(
            labels.progress,
            labels.success,
            labels.failure,
            check_tako_connection(host, port).instrument(verify_scope),
        )
        .await;
        drop(_t);
        result
    } else {
        output::with_spinner_async_err(
            labels.progress,
            labels.success,
            labels.failure,
            check_tako_connection(host, port),
        )
        .await
    }
}
