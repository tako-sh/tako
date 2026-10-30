use std::net::IpAddr;
use std::time::Duration;

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum NetworkProvider {
    #[default]
    Tailscale,
    Netbird,
}

impl NetworkProvider {
    pub const ALL: [Self; 2] = [Self::Tailscale, Self::Netbird];

    pub fn id(self) -> &'static str {
        match self {
            Self::Tailscale => "tailscale",
            Self::Netbird => "netbird",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Tailscale => "Tailscale",
            Self::Netbird => "NetBird",
        }
    }

    pub fn status_command(self) -> &'static str {
        match self {
            Self::Tailscale => "tailscale status --json",
            Self::Netbird => "netbird status --json",
        }
    }

    pub fn unavailable_message(self) -> String {
        format!(
            "Remote management requires {}. Connect this machine and the server to the same private network, allow management traffic in its access policies, then run `tako servers add` with the server's VPN hostname or IP.",
            self.label()
        )
    }
}

#[derive(Debug)]
pub(crate) struct NetworkStatus {
    pub provider: NetworkProvider,
    pub addresses: Vec<IpAddr>,
}

fn parse_ip(value: &str) -> Option<IpAddr> {
    value
        .split('/')
        .next()?
        .parse()
        .ok()
        .filter(is_private_address)
}

fn is_private_address(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            ip.is_private() || (ip.octets()[0] == 100 && (64..=127).contains(&ip.octets()[1]))
        }
        IpAddr::V6(ip) => ip.is_unique_local(),
    }
}

// Server checks must use its own addresses, never the addresses of its peers.
pub(crate) fn assigned_addresses(
    provider: NetworkProvider,
    value: &serde_json::Value,
) -> Option<Vec<IpAddr>> {
    let addresses = match provider {
        NetworkProvider::Tailscale => {
            if value["BackendState"].as_str()? != "Running" {
                return None;
            }
            value["TailscaleIPs"]
                .as_array()?
                .iter()
                .filter_map(|ip| ip.as_str().and_then(parse_ip))
                .collect::<Vec<_>>()
        }
        NetworkProvider::Netbird => {
            if value["management"]["connected"].as_bool() != Some(true)
                || value["daemonStatus"]
                    .as_str()
                    .is_some_and(|state| state != "Connected")
            {
                return None;
            }
            ["netbirdIp", "netbirdIpv6"]
                .into_iter()
                .filter_map(|key| value[key].as_str().and_then(parse_ip))
                .collect()
        }
    };
    (!addresses.is_empty()).then_some(addresses)
}

// Read assigned addresses and peer addresses from the agent, never infer a
// provider from a shared CGNAT range or from the presence of its executable.
fn parse_status(provider: NetworkProvider, value: &serde_json::Value) -> Option<NetworkStatus> {
    let mut addresses = assigned_addresses(provider, value)?;
    match provider {
        NetworkProvider::Tailscale => {
            if let Some(peers) = value["Peer"].as_object() {
                for peer in peers.values() {
                    if let Some(ips) = peer["TailscaleIPs"].as_array() {
                        addresses
                            .extend(ips.iter().filter_map(|ip| ip.as_str().and_then(parse_ip)));
                    }
                }
            }
        }
        NetworkProvider::Netbird => {
            if let Some(peers) = value["peers"]["details"].as_array() {
                for peer in peers {
                    addresses.extend(peer["netbirdIp"].as_str().and_then(parse_ip));
                    addresses.extend(peer["netbirdIpv6"].as_str().and_then(parse_ip));
                }
            }
        }
    }
    Some(NetworkStatus {
        provider,
        addresses,
    })
}

pub(crate) async fn detect() -> Vec<NetworkStatus> {
    let (tailscale, netbird) = tokio::join!(
        read_status(NetworkProvider::Tailscale),
        read_status(NetworkProvider::Netbird),
    );
    [tailscale, netbird].into_iter().flatten().collect()
}

async fn read_status(provider: NetworkProvider) -> Option<NetworkStatus> {
    let mut command = tokio::process::Command::new(provider.id());
    command.args(["status", "--json"]).kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(3), command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_status(provider, &serde_json::from_slice(&output.stdout).ok()?)
}

pub(crate) async fn host_addresses(host: &str) -> Result<Vec<IpAddr>, String> {
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![ip]);
    }
    tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::lookup_host((host, 9844)),
    )
    .await
    .map_err(|_| "VPN hostname resolution timed out".to_string())?
    .map(|addresses| addresses.map(|address| address.ip()).collect())
    .map_err(|error| format!("Could not resolve VPN hostname: {error}"))
}

pub(crate) async fn verify_host(host: &str, provider: NetworkProvider) -> Result<(), String> {
    let status = read_status(provider)
        .await
        .ok_or_else(|| provider.unavailable_message())?;
    let addresses = host_addresses(host).await?;
    if addresses_belong_to(&addresses, &status.addresses) {
        Ok(())
    } else {
        Err(provider.unavailable_message())
    }
}

pub(crate) fn addresses_belong_to(destination: &[IpAddr], assigned: &[IpAddr]) -> bool {
    !destination.is_empty()
        && destination
            .iter()
            .all(|ip| is_private_address(ip) && assigned.contains(ip))
}

pub(crate) async fn management_address(host: &str) -> Result<IpAddr, String> {
    let addresses = host_addresses(host).await?;
    if addresses.is_empty() || addresses.iter().any(|ip| !is_private_address(ip)) {
        return Err("Use the server's private VPN hostname or IP.".to_string());
    }
    // Prefer IPv4 consistently for dual-stack names. The installer verifies
    // that the selected VPN agent actually owns this address on the server.
    Ok(addresses
        .iter()
        .copied()
        .find(IpAddr::is_ipv4)
        .unwrap_or(addresses[0]))
}

pub(crate) fn ordered_providers(detected: &[NetworkStatus]) -> Vec<NetworkProvider> {
    let mut providers = NetworkProvider::ALL.to_vec();
    providers.sort_by_key(|provider| !detected.iter().any(|status| status.provider == *provider));
    providers
}

pub(crate) async fn select_provider(
    explicit: Option<NetworkProvider>,
    saved: Option<NetworkProvider>,
) -> Result<NetworkProvider, Box<dyn std::error::Error>> {
    if let (Some(explicit), Some(saved)) = (explicit, saved)
        && explicit != saved
    {
        return Err("Changing a server private network requires configuring the server on the new VPN as administrator, then removing and re-adding its inventory entry.".into());
    }
    if let Some(provider) = explicit.or(saved) {
        return Ok(provider);
    }
    if crate::output::is_dry_run() {
        return Ok(NetworkProvider::default());
    }
    let detected = detect().await;
    if crate::output::is_interactive() {
        let providers = ordered_providers(&detected);
        let options = providers
            .iter()
            .map(|provider| (provider.label().to_string(), *provider))
            .collect();
        let hints = providers
            .iter()
            .map(|provider| {
                if detected.iter().any(|status| status.provider == *provider) {
                    "detected"
                } else {
                    ""
                }
            })
            .collect::<Vec<_>>();
        let mut wizard = crate::output::Wizard::new().with_fields(&[("Private network", false)]);
        return Ok(wizard.select(
            "Private network",
            "Which private network connects you to this server?",
            options,
            &hints,
            0,
        )?);
    }
    match detected.as_slice() {
        [status] => Ok(status.provider),
        [] => Ok(NetworkProvider::default()),
        _ => Err("Both Tailscale and NetBird are connected. Select one with --network tailscale|netbird.".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn netbird_detection_requires_connection_and_reads_cidr_and_peers() {
        let mut value = json!({"management":{"connected":true},"netbirdIp":"100.80.0.1/16", "peers":{"details":[{"netbirdIp":"100.80.0.2","netbirdIpv6":"fd00::2"}]}});
        let status = parse_status(NetworkProvider::Netbird, &value).unwrap();
        assert!(status.addresses.contains(&"100.80.0.2".parse().unwrap()));
        assert!(status.addresses.contains(&"fd00::2".parse().unwrap()));
        value["management"]["connected"] = json!(false);
        assert!(parse_status(NetworkProvider::Netbird, &value).is_none());
    }

    #[test]
    fn tailscale_detection_requires_running_agent() {
        let mut value = json!({"BackendState":"Running","TailscaleIPs":["100.64.0.1"],"Peer":{"key":{"TailscaleIPs":["100.64.0.2"]}}});
        assert_eq!(
            parse_status(NetworkProvider::Tailscale, &value)
                .unwrap()
                .addresses
                .len(),
            2
        );
        value["BackendState"] = json!("NeedsLogin");
        assert!(parse_status(NetworkProvider::Tailscale, &value).is_none());
    }

    #[test]
    fn detected_provider_is_first_and_both_remain_available() {
        assert_eq!(
            ordered_providers(&[NetworkStatus {
                provider: NetworkProvider::Netbird,
                addresses: vec![]
            }]),
            vec![NetworkProvider::Netbird, NetworkProvider::Tailscale]
        );
        assert_eq!(ordered_providers(&[]), NetworkProvider::ALL);
    }

    #[test]
    fn server_ownership_excludes_peers_and_invalid_addresses() {
        let value = json!({"management":{"connected":true}, "daemonStatus":"Connected",
            "netbirdIp":"10.9.0.1/16", "netbirdIpv6":"fd00::1/64",
            "peers":{"details":[{"netbirdIp":"10.9.0.2"}]}});
        let assigned = assigned_addresses(NetworkProvider::Netbird, &value).unwrap();
        assert!(addresses_belong_to(
            &["fd00::1".parse().unwrap()],
            &assigned
        ));
        assert!(!addresses_belong_to(
            &["10.9.0.2".parse().unwrap()],
            &assigned
        ));
        assert!(!addresses_belong_to(&[], &assigned));
        // DNS must not mix an accepted VPN address with another destination.
        assert!(!addresses_belong_to(
            &["10.9.0.1".parse().unwrap(), "8.8.8.8".parse().unwrap()],
            &assigned
        ));
        for ip in [
            "0.0.0.0",
            "127.0.0.1",
            "8.8.8.8",
            "::",
            "::1",
            "fc::1",
            "fd00::::1",
        ] {
            assert!(parse_ip(ip).is_none(), "{ip}");
        }
    }

    #[test]
    fn stale_or_malformed_agent_status_is_not_detection() {
        for value in [
            json!({}),
            json!({"management":{"connected":true},"netbirdIp":"garbage"}),
            json!({"management":{"connected":true},"daemonStatus":"Idle","netbirdIp":"100.80.0.1"}),
        ] {
            assert!(parse_status(NetworkProvider::Netbird, &value).is_none());
        }
    }

    #[tokio::test]
    async fn explicit_and_saved_choices_do_not_depend_on_local_detection() {
        assert_eq!(
            select_provider(Some(NetworkProvider::Netbird), None)
                .await
                .unwrap(),
            NetworkProvider::Netbird
        );
        assert_eq!(
            select_provider(None, Some(NetworkProvider::Netbird))
                .await
                .unwrap(),
            NetworkProvider::Netbird
        );
        assert_eq!(
            management_address("[fd00::1]").await.unwrap(),
            "fd00::1".parse::<IpAddr>().unwrap()
        );
        assert!(management_address("127.0.0.1").await.is_err());
        assert!(
            select_provider(
                Some(NetworkProvider::Netbird),
                Some(NetworkProvider::Tailscale)
            )
            .await
            .is_err()
        );
    }
}
