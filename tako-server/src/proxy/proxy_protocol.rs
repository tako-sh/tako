use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use tokio::io::{AsyncRead, AsyncReadExt};

const V1_PREFIX: &[u8] = b"PROXY ";
const V1_MAX_LEN: usize = 108;
const V2_SIGNATURE: &[u8; 12] = b"\r\n\r\n\0\r\nQUIT\n";
const V2_VERSION: u8 = 0x20;
const V2_COMMAND_LOCAL: u8 = 0x00;
const V2_COMMAND_PROXY: u8 = 0x01;
const V2_FAMILY_UNSPEC: u8 = 0x00;
const V2_FAMILY_INET: u8 = 0x10;
const V2_FAMILY_INET6: u8 = 0x20;
const V2_TRANSPORT_STREAM: u8 = 0x01;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProxyProtocolResult {
    pub(super) source_addr: Option<SocketAddr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProxyProtocolError {
    message: String,
}

impl ProxyProtocolError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ProxyProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ProxyProtocolError {}

pub(super) async fn read_proxy_protocol_header<R>(
    reader: &mut R,
) -> Result<ProxyProtocolResult, ProxyProtocolError>
where
    R: AsyncRead + Unpin,
{
    let mut prefix = [0_u8; 12];
    reader
        .read_exact(&mut prefix)
        .await
        .map_err(|e| ProxyProtocolError::new(format!("missing PROXY header: {e}")))?;

    if &prefix == V2_SIGNATURE {
        return read_proxy_v2_header(reader).await;
    }

    if !prefix.starts_with(V1_PREFIX) {
        return Err(ProxyProtocolError::new("missing PROXY header"));
    }

    read_proxy_v1_header(reader, prefix.to_vec()).await
}

async fn read_proxy_v1_header<R>(
    reader: &mut R,
    mut line: Vec<u8>,
) -> Result<ProxyProtocolResult, ProxyProtocolError>
where
    R: AsyncRead + Unpin,
{
    while !line.ends_with(b"\r\n") {
        if line.len() >= V1_MAX_LEN {
            return Err(ProxyProtocolError::new("PROXY v1 header is too long"));
        }
        let mut byte = [0_u8; 1];
        reader
            .read_exact(&mut byte)
            .await
            .map_err(|e| ProxyProtocolError::new(format!("incomplete PROXY v1 header: {e}")))?;
        line.push(byte[0]);
    }

    let line = std::str::from_utf8(&line)
        .map_err(|_| ProxyProtocolError::new("PROXY v1 header is not UTF-8"))?
        .trim_end_matches("\r\n");
    parse_proxy_v1_line(line)
}

fn parse_proxy_v1_line(line: &str) -> Result<ProxyProtocolResult, ProxyProtocolError> {
    let mut parts = line.split_whitespace();
    match parts.next() {
        Some("PROXY") => {}
        _ => return Err(ProxyProtocolError::new("missing PROXY v1 prefix")),
    }

    let Some(protocol) = parts.next() else {
        return Err(ProxyProtocolError::new("missing PROXY v1 protocol"));
    };
    if protocol == "UNKNOWN" {
        return Ok(ProxyProtocolResult { source_addr: None });
    }

    let source_ip = parts
        .next()
        .ok_or_else(|| ProxyProtocolError::new("missing PROXY v1 source address"))?;
    let _destination_ip = parts
        .next()
        .ok_or_else(|| ProxyProtocolError::new("missing PROXY v1 destination address"))?;
    let source_port = parts
        .next()
        .ok_or_else(|| ProxyProtocolError::new("missing PROXY v1 source port"))?;
    let _destination_port = parts
        .next()
        .ok_or_else(|| ProxyProtocolError::new("missing PROXY v1 destination port"))?;

    if parts.next().is_some() {
        return Err(ProxyProtocolError::new("too many PROXY v1 fields"));
    }

    let source_ip: IpAddr = source_ip
        .parse()
        .map_err(|_| ProxyProtocolError::new("invalid PROXY v1 source address"))?;
    match (protocol, source_ip) {
        ("TCP4", IpAddr::V4(_)) | ("TCP6", IpAddr::V6(_)) => {}
        ("TCP4" | "TCP6", _) => {
            return Err(ProxyProtocolError::new(
                "PROXY v1 address family does not match source address",
            ));
        }
        _ => return Err(ProxyProtocolError::new("unsupported PROXY v1 protocol")),
    }

    let source_port = source_port
        .parse::<u16>()
        .map_err(|_| ProxyProtocolError::new("invalid PROXY v1 source port"))?;

    Ok(ProxyProtocolResult {
        source_addr: Some(SocketAddr::new(source_ip, source_port)),
    })
}

async fn read_proxy_v2_header<R>(reader: &mut R) -> Result<ProxyProtocolResult, ProxyProtocolError>
where
    R: AsyncRead + Unpin,
{
    let mut header = [0_u8; 4];
    reader
        .read_exact(&mut header)
        .await
        .map_err(|e| ProxyProtocolError::new(format!("incomplete PROXY v2 header: {e}")))?;

    let version_command = header[0];
    if version_command & 0xf0 != V2_VERSION {
        return Err(ProxyProtocolError::new("unsupported PROXY v2 version"));
    }
    let command = version_command & 0x0f;
    let family_transport = header[1];
    let payload_len = u16::from_be_bytes([header[2], header[3]]) as usize;

    let mut payload = vec![0_u8; payload_len];
    reader
        .read_exact(&mut payload)
        .await
        .map_err(|e| ProxyProtocolError::new(format!("incomplete PROXY v2 payload: {e}")))?;

    match command {
        V2_COMMAND_LOCAL => Ok(ProxyProtocolResult { source_addr: None }),
        V2_COMMAND_PROXY => parse_proxy_v2_payload(family_transport, &payload),
        _ => Err(ProxyProtocolError::new("unsupported PROXY v2 command")),
    }
}

fn parse_proxy_v2_payload(
    family_transport: u8,
    payload: &[u8],
) -> Result<ProxyProtocolResult, ProxyProtocolError> {
    let family = family_transport & 0xf0;
    let transport = family_transport & 0x0f;
    if family == V2_FAMILY_UNSPEC {
        return Ok(ProxyProtocolResult { source_addr: None });
    }
    if transport != V2_TRANSPORT_STREAM {
        return Err(ProxyProtocolError::new(
            "unsupported PROXY v2 transport protocol",
        ));
    }

    match family {
        V2_FAMILY_INET => {
            if payload.len() < 12 {
                return Err(ProxyProtocolError::new("short PROXY v2 IPv4 payload"));
            }
            let source_ip = Ipv4Addr::new(payload[0], payload[1], payload[2], payload[3]);
            let source_port = u16::from_be_bytes([payload[8], payload[9]]);
            Ok(ProxyProtocolResult {
                source_addr: Some(SocketAddr::new(IpAddr::V4(source_ip), source_port)),
            })
        }
        V2_FAMILY_INET6 => {
            if payload.len() < 36 {
                return Err(ProxyProtocolError::new("short PROXY v2 IPv6 payload"));
            }
            let mut source_ip = [0_u8; 16];
            source_ip.copy_from_slice(&payload[..16]);
            let source_port = u16::from_be_bytes([payload[32], payload[33]]);
            Ok(ProxyProtocolResult {
                source_addr: Some(SocketAddr::new(
                    IpAddr::V6(Ipv6Addr::from(source_ip)),
                    source_port,
                )),
            })
        }
        _ => Err(ProxyProtocolError::new(
            "unsupported PROXY v2 address family",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    use tokio::io::AsyncWriteExt;

    async fn parse_header(bytes: &[u8]) -> ProxyProtocolResult {
        let (mut client, mut server) = tokio::io::duplex(256);
        client.write_all(bytes).await.unwrap();
        drop(client);
        read_proxy_protocol_header(&mut server).await.unwrap()
    }

    #[tokio::test]
    async fn proxy_v1_tcp4_returns_source_address() {
        let result =
            parse_header(b"PROXY TCP4 203.0.113.10 192.0.2.20 51121 443\r\nGET / HTTP/1.1\r\n")
                .await;

        assert_eq!(
            result.source_addr,
            Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10)),
                51121,
            ))
        );
    }

    #[tokio::test]
    async fn proxy_v1_unknown_keeps_socket_peer() {
        let result = parse_header(b"PROXY UNKNOWN\r\nGET / HTTP/1.1\r\n").await;

        assert_eq!(result.source_addr, None);
    }

    #[tokio::test]
    async fn proxy_v2_tcp6_returns_source_address() {
        let mut header = Vec::from(b"\r\n\r\n\0\r\nQUIT\n");
        header.extend_from_slice(&[0x21, 0x21, 0x00, 0x24]);
        header.extend_from_slice(&Ipv6Addr::LOCALHOST.octets());
        header.extend_from_slice(&Ipv6Addr::UNSPECIFIED.octets());
        header.extend_from_slice(&51121_u16.to_be_bytes());
        header.extend_from_slice(&443_u16.to_be_bytes());

        let result = parse_header(&header).await;

        assert_eq!(
            result.source_addr,
            Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 51121))
        );
    }

    #[tokio::test]
    async fn direct_connection_without_proxy_header_is_rejected() {
        let (mut client, mut server) = tokio::io::duplex(64);
        client.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();
        drop(client);

        let error = read_proxy_protocol_header(&mut server).await.unwrap_err();

        assert!(error.to_string().contains("missing PROXY header"));
    }
}
