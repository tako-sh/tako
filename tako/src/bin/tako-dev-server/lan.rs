use std::collections::HashMap;
use std::process::{Child, Command, Stdio};

/// Manages mDNS publisher processes for LAN mode.
///
/// Each registered app gets a `.local` mDNS entry so phones/tablets can
/// find it by hostname on the local Wi-Fi network.
pub(crate) struct MdnsPublisher {
    /// Maps app hostnames → spawned publisher process.
    publishers: HashMap<String, Child>,
    lan_ip: String,
}

impl MdnsPublisher {
    pub(crate) fn new(lan_ip: String) -> Self {
        Self {
            publishers: HashMap::new(),
            lan_ip,
        }
    }

    /// Publish a `.local` mDNS entry for the given hostname.
    ///
    /// Returns silently without publishing if the hostname is a wildcard
    /// (e.g. `*.app.test`), since mDNS (Bonjour/Avahi) cannot advertise
    /// wildcard names — each subdomain must be a concrete record.
    pub(crate) fn publish(&mut self, hostname: &str) {
        let Some(local_hostname) = to_mdns_hostname(hostname) else {
            return;
        };
        if self.publishers.contains_key(&local_hostname) {
            return;
        }
        if let Some(child) = spawn_mdns_publisher(&local_hostname, &self.lan_ip) {
            tracing::debug!(hostname = %local_hostname, ip = %self.lan_ip, "mDNS published");
            self.publishers.insert(local_hostname, child);
        }
    }

    /// Unpublish a specific hostname (kills its publisher process).
    pub(crate) fn unpublish(&mut self, hostname: &str) {
        let Some(local_hostname) = to_mdns_hostname(hostname) else {
            return;
        };
        if let Some(mut child) = self.publishers.remove(&local_hostname) {
            let _ = child.kill();
            let _ = child.wait();
            tracing::debug!(hostname = %local_hostname, "mDNS unpublished");
        }
    }

    /// Stop all publisher processes and clear state.
    pub(crate) fn cleanup_all(&mut self) {
        for (hostname, mut child) in self.publishers.drain() {
            let _ = child.kill();
            let _ = child.wait();
            tracing::debug!(hostname = %hostname, "mDNS unpublished");
        }
    }
}

impl Drop for MdnsPublisher {
    fn drop(&mut self) {
        self.cleanup_all();
    }
}

/// Convert a `.test` or `.tako.test` hostname to a `.local` hostname for mDNS.
///
/// Returns `None` for wildcard hostnames (`*.foo.test`) because mDNS cannot
/// advertise wildcards — each concrete subdomain needs its own record.
/// Returns `None` for external hostnames because Tako does not own their LAN
/// DNS shape.
pub(crate) fn to_mdns_hostname(hostname: &str) -> Option<String> {
    if hostname.starts_with("*.") {
        return None;
    }
    // Check the more specific suffix first — `.tako.test` ends with `.test`,
    // so stripping `.test` would leave a trailing `.tako`.
    let base = hostname
        .strip_suffix(".tako.test")
        .or_else(|| hostname.strip_suffix(".test"))?;
    Some(format!("{base}.local"))
}

/// Spawn a platform-appropriate mDNS publisher process.
fn spawn_mdns_publisher(local_hostname: &str, ip: &str) -> Option<Child> {
    #[cfg(target_os = "macos")]
    {
        // dns-sd -P <name> _http._tcp local 443 <hostname> <ip>
        let name = local_hostname
            .strip_suffix(".local")
            .unwrap_or(local_hostname);
        Command::new("dns-sd")
            .args(["-P", name, "_http._tcp", "local", "443", local_hostname, ip])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()
    }
    #[cfg(target_os = "linux")]
    {
        Command::new("avahi-publish-address")
            .args(["-R", local_hostname, ip])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (local_hostname, ip);
        None
    }
}

/// Detect the LAN IP by UDP-connecting to a public DNS address.
/// The socket is never used to send traffic — it just reveals the local IP.
pub(crate) fn detect_lan_ip() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("1.1.1.1:53").ok()?;
    let local_addr = socket.local_addr().ok()?;
    let ip = local_addr.ip();
    if ip.is_loopback() || ip.is_unspecified() {
        return None;
    }
    Some(ip.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_mdns_hostname_strips_tako_test_suffix() {
        assert_eq!(
            to_mdns_hostname("myapp.tako.test").as_deref(),
            Some("myapp.local")
        );
    }

    #[test]
    fn to_mdns_hostname_strips_test_suffix() {
        assert_eq!(
            to_mdns_hostname("myapp.test").as_deref(),
            Some("myapp.local")
        );
    }

    #[test]
    fn to_mdns_hostname_rejects_bare_name() {
        assert_eq!(to_mdns_hostname("myapp"), None);
    }

    #[test]
    fn to_mdns_hostname_rejects_external_hostname() {
        assert_eq!(to_mdns_hostname("tunnel.example.com"), None);
    }

    #[test]
    fn to_mdns_hostname_rejects_wildcards() {
        // mDNS cannot advertise wildcard hostnames, so the publisher must
        // refuse to emit them rather than spawning `dns-sd -P '*.myapp'`.
        assert_eq!(to_mdns_hostname("*.myapp.test"), None);
        assert_eq!(to_mdns_hostname("*.myapp.tako.test"), None);
    }

    #[test]
    fn detect_lan_ip_returns_some_on_connected_machine() {
        // This test will pass on machines with a network connection
        // and fail (return None) on isolated CI environments — both are acceptable.
        let ip = detect_lan_ip();
        if let Some(ref ip) = ip {
            assert!(!ip.starts_with("127."));
            assert!(!ip.is_empty());
        }
    }
}
