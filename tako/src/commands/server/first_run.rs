use crate::output;
use crate::ssh::{SshClient, SshConfig};

pub(super) struct FirstRunServerSettings {
    dns: super::dns::DnsConfigChange,
    source_ip: Option<Option<super::trusted_proxy::TrustedProxyCliConfig>>,
}

impl Default for FirstRunServerSettings {
    fn default() -> Self {
        Self {
            dns: super::dns::DnsConfigChange::Unchanged,
            source_ip: None,
        }
    }
}

impl FirstRunServerSettings {
    fn is_unchanged(&self) -> bool {
        matches!(self.dns, super::dns::DnsConfigChange::Unchanged) && self.source_ip.is_none()
    }
}

pub(super) fn prompt_first_run_settings()
-> Result<FirstRunServerSettings, Box<dyn std::error::Error>> {
    if !output::is_interactive() {
        return Ok(FirstRunServerSettings::default());
    }

    output::section("Server settings");
    let mut settings = FirstRunServerSettings::default();

    for step in first_run_setting_steps() {
        match step {
            FirstRunSettingStep::SourceIp => {
                settings.source_ip = super::trusted_proxy::prompt_trusted_proxy_config()?.map(Some);
            }
            FirstRunSettingStep::DnsWildcardCertificates => {
                settings.dns = super::dns::prompt_dns_setup(None)?;
            }
        }
    }

    Ok(settings)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FirstRunSettingStep {
    SourceIp,
    DnsWildcardCertificates,
}

const FIRST_RUN_SETTING_STEPS: &[FirstRunSettingStep] = &[
    FirstRunSettingStep::SourceIp,
    FirstRunSettingStep::DnsWildcardCertificates,
];

fn first_run_setting_steps() -> &'static [FirstRunSettingStep] {
    FIRST_RUN_SETTING_STEPS
}

pub(super) async fn apply_first_run_settings_before_start(
    host: &str,
    port: u16,
    label: &str,
    settings: &FirstRunServerSettings,
) -> Result<(), Box<dyn std::error::Error>> {
    if settings.is_unchanged() {
        return Ok(());
    }

    let message = format!("Applying server settings on {label}");
    output::with_spinner_async_err(
        &message,
        &format!("Server settings applied on {label}"),
        &message,
        apply_first_run_settings_before_start_inner(host, port, label, settings),
    )
    .await
}

async fn apply_first_run_settings_before_start_inner(
    host: &str,
    port: u16,
    label: &str,
    settings: &FirstRunServerSettings,
) -> Result<(), Box<dyn std::error::Error>> {
    let ssh_config = SshConfig::from_server(host, port);
    let mut ssh = SshClient::new(ssh_config);
    ssh.connect()
        .await
        .map_err(|e| -> Box<dyn std::error::Error> {
            format!("Failed to connect to {label}: {e}").into()
        })?;

    let result = async {
        super::dns::apply_dns_config_before_start(&ssh, label, &settings.dns).await?;
        if let Some(source_ip) = &settings.source_ip {
            super::trusted_proxy::apply_trusted_proxy_config_before_start(&ssh, source_ip.as_ref())
                .await?;
        }
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;

    let disconnect = ssh.disconnect().await;
    match (result, disconnect) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(error)) => Err(error.into()),
        (Err(error), _) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_run_settings_start_with_source_ip_then_dns_wildcards() {
        assert_eq!(
            first_run_setting_steps(),
            &[
                FirstRunSettingStep::SourceIp,
                FirstRunSettingStep::DnsWildcardCertificates,
            ],
        );
    }
}
