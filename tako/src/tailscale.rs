use std::net::IpAddr;

const TAILSCALE_REQUIRED_MESSAGE: &str = "Remote management requires Tailscale so Tako can keep server control traffic private by default.";

pub(crate) fn required_message() -> &'static str {
    TAILSCALE_REQUIRED_MESSAGE
}

pub(crate) fn is_tailscale_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [first, second, _, _] = ip.octets();
            first == 100 && (64..=127).contains(&second)
        }
        IpAddr::V6(ip) => {
            let segments = ip.segments();
            segments[0] == 0xfd7a && segments[1] == 0x115c && segments[2] == 0xa1e0
        }
    }
}

pub(crate) async fn ensure_tailscale_host(host: &str) -> Result<(), String> {
    let trimmed = host.trim();
    let literal = trimmed
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(trimmed);

    if let Ok(ip) = literal.parse::<IpAddr>() {
        return if is_tailscale_ip(ip) {
            Ok(())
        } else {
            Err(TAILSCALE_REQUIRED_MESSAGE.to_string())
        };
    }

    let addrs = tokio::net::lookup_host((trimmed, crate::management_http::MANAGEMENT_PORT))
        .await
        .map_err(|_| TAILSCALE_REQUIRED_MESSAGE.to_string())?;

    if addrs.map(|addr| addr.ip()).any(is_tailscale_ip) {
        Ok(())
    } else {
        Err(TAILSCALE_REQUIRED_MESSAGE.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn tailscale_ipv4_range_is_100_64_0_0_to_100_127_255_255() {
        assert!(is_tailscale_ip(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))));
        assert!(is_tailscale_ip(IpAddr::V4(Ipv4Addr::new(
            100, 127, 255, 255
        ))));
        assert!(!is_tailscale_ip(IpAddr::V4(Ipv4Addr::new(100, 128, 0, 1))));
        assert!(!is_tailscale_ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
    }

    #[test]
    fn tailscale_ipv6_ula_prefix_is_accepted() {
        assert!(is_tailscale_ip(IpAddr::V6(
            "fd7a:115c:a1e0::1".parse::<Ipv6Addr>().unwrap()
        )));
        assert!(!is_tailscale_ip(IpAddr::V6(
            "fd00::1".parse::<Ipv6Addr>().unwrap()
        )));
    }
}
