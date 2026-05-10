//! Local authoritative DNS for `*.test` and `*.tako.test` development hosts.
//!
//! Answers queries directly from the current app host table.

use std::net::{Ipv4Addr, SocketAddr};

use hickory_proto::{
    op::{Message, ResponseCode},
    rr::{DNSClass, Name, RData, Record, RecordType, rdata::A},
};

use tokio::net::UdpSocket;

const DNS_TTL_SECS: u32 = 30;
const DEV_SUFFIX: &str = ".test";
const SCOPED_DEV_SUFFIX: &str = ".tako.test";

#[derive(Debug, Clone)]
struct ParsedDnsQuery {
    request: Message,
    query_name: Name,
    qname: String,
    qtype: RecordType,
    qclass: DNSClass,
}

#[derive(Debug, Clone)]
pub struct LocalDns {
    listen_addr: SocketAddr,
}

impl LocalDns {
    pub fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    pub fn port(&self) -> u16 {
        self.listen_addr.port()
    }
}

fn is_dev_host(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    h == "test" || h.ends_with(DEV_SUFFIX) || h == "tako.test" || h.ends_with(SCOPED_DEV_SUFFIX)
}

fn parse_dns_query(packet: &[u8]) -> Option<ParsedDnsQuery> {
    let request = Message::from_vec(packet).ok()?;
    let query = request.queries.first()?.clone();
    let qname = query
        .name()
        .to_ascii()
        .trim_end_matches('.')
        .to_ascii_lowercase();

    Some(ParsedDnsQuery {
        request,
        query_name: query.name().clone(),
        qname,
        qtype: query.query_type(),
        qclass: query.query_class(),
    })
}

fn response_with_record(
    response: &mut Message,
    name: Name,
    record_type: RecordType,
    loopback_ip: Ipv4Addr,
) {
    if record_type == RecordType::A {
        response.add_answer(Record::from_rdata(
            name,
            DNS_TTL_SECS,
            RData::A(A(loopback_ip)),
        ));
    }
}

fn build_dns_response(packet: &[u8], loopback_ip: Ipv4Addr) -> Option<Vec<u8>> {
    let q = parse_dns_query(packet)?;
    let mut response = Message::response(q.request.metadata.id, q.request.metadata.op_code);
    let in_zone = is_dev_host(&q.qname);

    response.metadata.recursion_desired = q.request.metadata.recursion_desired;
    response.metadata.authoritative = true;
    if let Some(query) = q.request.queries.first() {
        response.add_query(query.clone());
    }

    if !in_zone {
        response.metadata.response_code = ResponseCode::Refused;
        return response.to_vec().ok();
    }

    if q.qclass == DNSClass::IN {
        match q.qtype {
            RecordType::A => {
                response_with_record(&mut response, q.query_name, q.qtype, loopback_ip);
            }
            RecordType::ANY => {
                response_with_record(&mut response, q.query_name, RecordType::A, loopback_ip);
            }
            _ => {}
        }
    }

    response.to_vec().ok()
}

pub async fn start(
    listen_addr: &str,
    loopback_ip: Ipv4Addr,
) -> Result<LocalDns, Box<dyn std::error::Error>> {
    let socket = UdpSocket::bind(listen_addr).await?;
    let bound = socket.local_addr()?;

    tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        loop {
            let (len, peer) = match socket.recv_from(&mut buf).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "local DNS recv failed");
                    continue;
                }
            };

            let packet = &buf[..len];
            let Some(resp) = build_dns_response(packet, loopback_ip) else {
                continue;
            };

            if let Err(e) = socket.send_to(&resp, peer).await {
                tracing::warn!(error = %e, "local DNS send failed");
            }
        }
    });

    Ok(LocalDns { listen_addr: bound })
}

#[cfg(test)]
mod tests {
    use super::*;
    const DNS_CLASS_IN: u16 = 1;
    const DNS_TYPE_A: u16 = 1;
    const DNS_TYPE_AAAA: u16 = 28;

    fn build_query_with_flags(host: &str, qtype: u16, flags: u16) -> Vec<u8> {
        let mut q = Vec::new();
        q.extend_from_slice(&0x1234u16.to_be_bytes()); // id
        q.extend_from_slice(&flags.to_be_bytes());
        q.extend_from_slice(&1u16.to_be_bytes()); // qd
        q.extend_from_slice(&0u16.to_be_bytes()); // an
        q.extend_from_slice(&0u16.to_be_bytes()); // ns
        q.extend_from_slice(&0u16.to_be_bytes()); // ar
        for label in host.split('.') {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&DNS_CLASS_IN.to_be_bytes());
        q
    }

    fn build_query(host: &str, qtype: u16) -> Vec<u8> {
        build_query_with_flags(host, qtype, 0x0100)
    }

    fn rcode(resp: &[u8]) -> u8 {
        (u16::from_be_bytes([resp[2], resp[3]]) & 0x000F) as u8
    }

    fn opcode(flags: u16) -> u16 {
        (flags >> 11) & 0xF
    }

    fn resp_flags(resp: &[u8]) -> u16 {
        u16::from_be_bytes([resp[2], resp[3]])
    }

    fn ancount(resp: &[u8]) -> u16 {
        u16::from_be_bytes([resp[6], resp[7]])
    }

    #[test]
    fn parses_dns_question_name() {
        let q = build_query("app.test", DNS_TYPE_A);
        let parsed = parse_dns_query(&q).expect("query should parse");
        assert_eq!(parsed.qname, "app.test");
        assert_eq!(parsed.qtype, RecordType::A);
    }

    #[test]
    fn returns_a_record_for_known_host() {
        let q = build_query("app.test", DNS_TYPE_A);
        let resp = build_dns_response(&q, Ipv4Addr::new(127, 77, 0, 1)).expect("response");
        assert_eq!(rcode(&resp), 0);
        assert_eq!(ancount(&resp), 1);
        assert!(resp.ends_with(&[127, 77, 0, 1]));
    }

    #[test]
    fn returns_empty_answer_for_aaaa_known_host() {
        let q = build_query("app.test", DNS_TYPE_AAAA);
        let resp = build_dns_response(&q, Ipv4Addr::new(127, 77, 0, 1)).expect("response");
        assert_eq!(rcode(&resp), 0);
        assert_eq!(ancount(&resp), 0);
    }

    #[test]
    fn returns_only_a_record_for_any_query() {
        let q = build_query("app.test", 255);
        let resp = build_dns_response(&q, Ipv4Addr::new(127, 77, 0, 1)).expect("response");
        assert_eq!(rcode(&resp), 0);
        assert_eq!(ancount(&resp), 1);
        assert!(resp.ends_with(&[127, 77, 0, 1]));
    }

    #[test]
    fn resolves_any_tako_subdomain() {
        let q = build_query("anything.tako.test", DNS_TYPE_A);
        let resp = build_dns_response(&q, Ipv4Addr::new(127, 77, 0, 1)).expect("response");
        assert_eq!(rcode(&resp), 0);
        assert_eq!(ancount(&resp), 1);
        assert!(resp.ends_with(&[127, 77, 0, 1]));
    }

    #[test]
    fn resolves_bare_tako() {
        let q = build_query("tako.test", DNS_TYPE_A);
        let resp = build_dns_response(&q, Ipv4Addr::new(127, 77, 0, 1)).expect("response");
        assert_eq!(rcode(&resp), 0);
        assert_eq!(ancount(&resp), 1);
        assert!(resp.ends_with(&[127, 77, 0, 1]));
    }

    #[test]
    fn resolves_short_test_domain() {
        let q = build_query("app.test", DNS_TYPE_A);
        let resp = build_dns_response(&q, Ipv4Addr::new(127, 77, 0, 1)).expect("response");
        assert_eq!(rcode(&resp), 0);
        assert_eq!(ancount(&resp), 1);
        assert!(resp.ends_with(&[127, 77, 0, 1]));
    }

    #[test]
    fn resolves_bare_test_tld() {
        let q = build_query("test", DNS_TYPE_A);
        let resp = build_dns_response(&q, Ipv4Addr::new(127, 77, 0, 1)).expect("response");
        assert_eq!(rcode(&resp), 0);
        assert_eq!(ancount(&resp), 1);
    }

    #[test]
    fn resolves_nested_short_test_subdomain() {
        let q = build_query("api.my-app.test", DNS_TYPE_A);
        let resp = build_dns_response(&q, Ipv4Addr::new(127, 77, 0, 1)).expect("response");
        assert_eq!(rcode(&resp), 0);
        assert_eq!(ancount(&resp), 1);
    }

    #[test]
    fn returns_refused_for_outside_zone() {
        let q = build_query("example.com", DNS_TYPE_A);
        let resp = build_dns_response(&q, Ipv4Addr::new(127, 77, 0, 1)).expect("response");
        assert_eq!(rcode(&resp), 5); // REFUSED
    }

    #[test]
    fn echoes_opcode_in_response_flags() {
        // STATUS opcode with RD bit set.
        let q = build_query_with_flags("app.test", DNS_TYPE_A, (2 << 11) | 0x0100);
        let resp = build_dns_response(&q, Ipv4Addr::new(127, 77, 0, 1)).expect("response");
        assert_eq!(opcode(resp_flags(&resp)), 2);
    }
}
