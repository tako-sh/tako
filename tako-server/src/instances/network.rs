use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamEndpoint {
    addr: SocketAddr,
    bind_host: String,
}

impl UpstreamEndpoint {
    pub fn loopback(port: u16) -> Self {
        Self {
            addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)),
            bind_host: "127.0.0.1".to_string(),
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    #[cfg(test)]
    pub fn bind_host(&self) -> &str {
        &self.bind_host
    }
}

pub struct PreparedInstanceNetwork {
    endpoint: UpstreamEndpoint,
}

impl PreparedInstanceNetwork {
    pub fn host_loopback(port: u16) -> Self {
        Self {
            endpoint: UpstreamEndpoint::loopback(port),
        }
    }

    pub fn endpoint(&self) -> &UpstreamEndpoint {
        &self.endpoint
    }

    #[cfg(test)]
    pub fn bind_host(&self) -> &str {
        self.endpoint.bind_host()
    }

    pub fn cleanup(self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_loopback_endpoint_uses_localhost_bind_host() {
        let upstream = PreparedInstanceNetwork::host_loopback(47_831);
        assert_eq!(
            upstream.endpoint().addr(),
            "127.0.0.1:47831".parse().unwrap()
        );
        assert_eq!(upstream.bind_host(), "127.0.0.1");
    }
}
