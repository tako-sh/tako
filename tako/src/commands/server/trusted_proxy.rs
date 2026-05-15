use crate::output;
use crate::ssh::SshClient;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(super) struct TrustedProxyCliConfig {
    proxy_protocol: bool,
    trusted_cidrs: Vec<String>,
    client_ip_headers: Vec<String>,
}

impl TrustedProxyCliConfig {
    fn proxy_protocol(trusted_cidrs: Vec<String>) -> Self {
        Self {
            proxy_protocol: true,
            trusted_cidrs,
            client_ip_headers: Vec::new(),
        }
    }

    fn headers(trusted_cidrs: Vec<String>, client_ip_headers: Vec<String>) -> Self {
        Self {
            proxy_protocol: false,
            trusted_cidrs,
            client_ip_headers,
        }
    }
}

pub(super) async fn configure_trusted_proxy(
    name: &str,
    ssh: &SshClient,
    current_config: &super::remote_config::ServerConfigWithoutSecrets,
) -> Result<(), Box<dyn std::error::Error>> {
    match prompt_trusted_proxy_config_change(current_config.trusted_proxy())? {
        TrustedProxyConfigChange::Apply(config) => {
            apply_trusted_proxy_config(ssh, name, config.as_ref()).await?;
            output::success(&format!("Server {} configured", output::strong(name)));
        }
        TrustedProxyConfigChange::Unchanged => {
            output::success(&format!(
                "Source-IP handling unchanged on {}",
                output::strong(name)
            ));
        }
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TrustedProxyConfigChange {
    Apply(Option<TrustedProxyCliConfig>),
    Unchanged,
}

fn prompt_trusted_proxy_config_change(
    current: Option<&super::remote_config::ServerTrustedProxyConfig>,
) -> Result<TrustedProxyConfigChange, Box<dyn std::error::Error>> {
    let description = trusted_proxy_change_prompt_description(current);
    let should_change =
        output::confirm_with_description("Change source-IP handling?", Some(&description), false)?;
    if !should_change {
        return Ok(TrustedProxyConfigChange::Unchanged);
    }

    Ok(TrustedProxyConfigChange::Apply(
        prompt_trusted_proxy_config()?,
    ))
}

fn trusted_proxy_change_prompt_description(
    current: Option<&super::remote_config::ServerTrustedProxyConfig>,
) -> String {
    match current {
        None => "Currently using direct traffic.".to_string(),
        Some(config) if config.proxy_protocol => {
            "Currently using PROXY protocol from trusted proxy CIDRs.".to_string()
        }
        Some(config)
            if config
                .client_ip_headers
                .iter()
                .any(|header| header == "cf-connecting-ip") =>
        {
            "Currently using Cloudflare CF-Connecting-IP from trusted proxy CIDRs.".to_string()
        }
        Some(config)
            if config
                .client_ip_headers
                .iter()
                .any(|header| header == "x-forwarded-for") =>
        {
            "Currently using X-Forwarded-For from trusted proxy CIDRs.".to_string()
        }
        Some(_) => "Currently using trusted source-IP config.".to_string(),
    }
}

pub(super) fn prompt_trusted_proxy_config()
-> Result<Option<TrustedProxyCliConfig>, Box<dyn std::error::Error>> {
    let mode = output::select(
        "Source IP mode",
        Some(
            "Choose how Tako should find the real client IP. Use Direct traffic unless the server is only reachable through a trusted proxy.",
        ),
        source_ip_mode_options(),
    )?;

    match mode {
        "direct" => Ok(None),
        "proxy-protocol" => {
            let cidrs = prompt_trusted_cidrs(Some(&default_loopback_cidrs().join(", ")))?;
            Ok(Some(TrustedProxyCliConfig::proxy_protocol(cidrs)))
        }
        "cloudflare-header" => {
            output::muted("Enter Cloudflare proxy CIDRs from https://www.cloudflare.com/ips/.");
            let cidrs = prompt_trusted_cidrs(None)?;
            Ok(Some(TrustedProxyCliConfig::headers(
                cidrs,
                vec!["cf-connecting-ip".to_string()],
            )))
        }
        "x-forwarded-for" => {
            let cidrs = prompt_trusted_cidrs(Some(&default_loopback_cidrs().join(", ")))?;
            Ok(Some(TrustedProxyCliConfig::headers(
                cidrs,
                vec!["x-forwarded-for".to_string()],
            )))
        }
        _ => Err(output::operation_cancelled_error().into()),
    }
}

fn source_ip_mode_options() -> Vec<(String, &'static str)> {
    vec![
        ("Direct traffic".to_string(), "direct"),
        (
            "PROXY protocol from a TCP proxy such as NGINX".to_string(),
            "proxy-protocol",
        ),
        (
            "Cloudflare proxy mode using CF-Connecting-IP".to_string(),
            "cloudflare-header",
        ),
        (
            "X-Forwarded-For from a non-Cloudflare HTTP reverse proxy".to_string(),
            "x-forwarded-for",
        ),
    ]
}

fn prompt_trusted_cidrs(default: Option<&str>) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut field = output::TextField::new("Trusted proxy CIDRs")
        .with_hint("comma-separated, e.g. 127.0.0.1/32");
    if let Some(default) = default {
        field = field.with_default(default);
    }
    let value = field.prompt_validated(|value| parse_cidr_list(value).map(|_| ()))?;
    parse_cidr_list(&value).map_err(Into::into)
}

fn parse_cidr_list(value: &str) -> Result<Vec<String>, String> {
    let cidrs = value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if cidrs.is_empty() {
        return Err("Enter at least one trusted proxy CIDR.".to_string());
    }

    for cidr in &cidrs {
        cidr.parse::<ipnet::IpNet>()
            .map_err(|e| format!("Invalid CIDR '{cidr}': {e}"))?;
    }

    Ok(cidrs)
}

fn default_loopback_cidrs() -> Vec<String> {
    vec!["127.0.0.1/32".to_string(), "::1/128".to_string()]
}

async fn apply_trusted_proxy_config(
    ssh: &SshClient,
    name: &str,
    config: Option<&TrustedProxyCliConfig>,
) -> Result<(), Box<dyn std::error::Error>> {
    let msg = format!("Configuring source IP on {name}");
    output::with_spinner_async_err(
        &msg,
        &format!("Source IP configured on {name}"),
        &msg,
        apply_trusted_proxy_config_inner(ssh, config),
    )
    .await
}

async fn apply_trusted_proxy_config_inner(
    ssh: &SshClient,
    config: Option<&TrustedProxyCliConfig>,
) -> Result<(), Box<dyn std::error::Error>> {
    ssh.exec_checked(&trusted_proxy_config_command(config, true)?)
        .await?;

    Ok(())
}

pub(super) async fn apply_trusted_proxy_config_before_start(
    ssh: &SshClient,
    config: Option<&TrustedProxyCliConfig>,
) -> Result<(), Box<dyn std::error::Error>> {
    ssh.exec_checked(&trusted_proxy_config_command(config, false)?)
        .await?;
    Ok(())
}

fn trusted_proxy_config_command(
    config: Option<&TrustedProxyCliConfig>,
    restart: bool,
) -> Result<String, String> {
    let fragment = match config {
        Some(config) => Some(trusted_proxy_config_fragment(config)?),
        None => None,
    };
    let escaped_fragment = fragment
        .as_deref()
        .map(crate::shell::shell_single_quote)
        .unwrap_or_else(|| "''".to_string());
    let mode = if fragment.is_some() { "set" } else { "clear" };
    let restart_command = if restart {
        "if command -v systemctl >/dev/null 2>&1; then \
           systemctl restart tako-server; \
         elif command -v rc-service >/dev/null 2>&1; then \
           rc-service tako-server restart; \
         else \
           service tako-server restart; \
         fi"
    } else {
        ":"
    };

    Ok(SshClient::run_with_root_or_sudo(&format!(
        r#"CONFIG="{path}"; \
         MODE={mode}; \
         FRAGMENT={fragment}; \
         EXISTING="$(cat "$CONFIG" 2>/dev/null || echo '{{}}')"; \
         if ! command -v python3 >/dev/null 2>&1; then \
           echo "error: python3 required" >&2 && exit 1; \
         fi; \
         python3 -c "import json,sys; d=json.loads(sys.argv[1] or '{{}}'); mode=sys.argv[2]; frag=sys.argv[3]; d.pop('trusted_proxy', None) if mode == 'clear' else d.__setitem__('trusted_proxy', json.loads(frag)); json.dump(d, open(sys.argv[4], 'w'))" "$EXISTING" "$MODE" "$FRAGMENT" "$CONFIG.tmp" && \
         mv "$CONFIG.tmp" "$CONFIG" && chmod 0644 "$CONFIG" && chown tako:tako "$CONFIG" && \
         {restart_command}"#,
        path = super::remote_config::SERVER_CONFIG_JSON,
        mode = crate::shell::shell_single_quote(mode),
        fragment = escaped_fragment,
        restart_command = restart_command,
    )))
}

fn trusted_proxy_config_fragment(config: &TrustedProxyCliConfig) -> Result<String, String> {
    serde_json::to_string(config).map_err(|e| format!("Failed to encode trusted proxy config: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_fragment_for_proxy_protocol_uses_loopback_default() {
        let config = TrustedProxyCliConfig::proxy_protocol(default_loopback_cidrs());
        let fragment = trusted_proxy_config_fragment(&config).unwrap();

        assert!(fragment.contains(r#""proxy_protocol":true"#));
        assert!(fragment.contains(r#""127.0.0.1/32""#));
        assert!(fragment.contains(r#""::1/128""#));
        assert!(fragment.contains(r#""client_ip_headers":[]"#));
    }

    #[test]
    fn config_fragment_for_cloudflare_headers_uses_cf_connecting_ip() {
        let config = TrustedProxyCliConfig::headers(
            vec!["203.0.113.0/24".to_string()],
            vec!["cf-connecting-ip".to_string()],
        );
        let fragment = trusted_proxy_config_fragment(&config).unwrap();

        assert!(fragment.contains(r#""proxy_protocol":false"#));
        assert!(fragment.contains(r#""client_ip_headers":["cf-connecting-ip"]"#));
    }

    #[test]
    fn source_ip_mode_prompt_uses_compact_labels_for_common_modes() {
        let options = source_ip_mode_options();

        assert_eq!(options.len(), 4);
        assert_eq!(options[0].0, "Direct traffic");
        assert_eq!(options[0].1, "direct");
        assert_eq!(
            options[1].0,
            "PROXY protocol from a TCP proxy such as NGINX",
        );
        assert_eq!(options[1].1, "proxy-protocol");
        assert_eq!(options[2].0, "Cloudflare proxy mode using CF-Connecting-IP",);
        assert_eq!(options[2].1, "cloudflare-header");
        assert_eq!(
            options[3].0,
            "X-Forwarded-For from a non-Cloudflare HTTP reverse proxy",
        );
        assert_eq!(options[3].1, "x-forwarded-for");
    }

    #[test]
    fn cidr_list_parser_trims_commas_and_spaces() {
        assert_eq!(
            parse_cidr_list(" 127.0.0.1/32, ::1/128  ").unwrap(),
            vec!["127.0.0.1/32".to_string(), "::1/128".to_string()]
        );
    }

    #[test]
    fn trusted_proxy_merge_command_can_skip_service_restart_before_first_start() {
        let config = TrustedProxyCliConfig::proxy_protocol(default_loopback_cidrs());
        let command = trusted_proxy_config_command(Some(&config), false).unwrap();

        assert!(command.contains("trusted_proxy"));
        assert!(!command.contains("systemctl restart tako-server"));
        assert!(!command.contains("rc-service tako-server restart"));
    }

    #[test]
    fn trusted_proxy_change_prompt_describes_direct_current_state() {
        assert_eq!(
            trusted_proxy_change_prompt_description(None),
            "Currently using direct traffic.",
        );
    }

    #[test]
    fn trusted_proxy_change_prompt_describes_proxy_protocol_current_state() {
        let current = super::super::remote_config::ServerTrustedProxyConfig {
            proxy_protocol: true,
            trusted_cidrs: default_loopback_cidrs(),
            client_ip_headers: Vec::new(),
        };

        assert_eq!(
            trusted_proxy_change_prompt_description(Some(&current)),
            "Currently using PROXY protocol from trusted proxy CIDRs.",
        );
    }
}
