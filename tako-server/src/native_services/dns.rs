use super::ServiceDirectory;
use hickory_proto::{
    op::{Message, MessageType, OpCode, ResponseCode},
    rr::{
        DNSClass, Name, RData, Record, RecordType,
        rdata::{A, AAAA, SOA},
    },
};
use parking_lot::RwLock;
use std::{
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tako_network::services::{ServiceHost, classify_host};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::watch,
    task::JoinSet,
};

const MAX_QUERY: usize = 4096;
const MAX_TCP_RESPONSE: usize = 65535;
const CONNECTIONS: usize = 64;
const TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Debug)]
pub enum DnsView {
    Native,
    Compose {
        gateway: Ipv4Addr,
        upstreams: Vec<IpAddr>,
    },
}

pub fn host_nameservers() -> io::Result<Vec<IpAddr>> {
    for path in ["/run/systemd/resolve/resolv.conf", "/etc/resolv.conf"] {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let mut addresses = Vec::new();
        for line in text.lines() {
            let mut words = line.split_whitespace();
            if words.next() != Some("nameserver") {
                continue;
            }
            if let Some(address) = words.next().and_then(|s| s.parse::<IpAddr>().ok()) {
                let address = address.to_canonical();
                if valid_upstream(address) && !addresses.contains(&address) {
                    addresses.push(address);
                }
            }
        }
        addresses.truncate(3);
        if !addresses.is_empty() {
            return Ok(addresses);
        }
    }
    Err(io::Error::other("no non-stub host DNS servers available"))
}

fn valid_upstream(ip: IpAddr) -> bool {
    let ip = ip.to_canonical();
    !ip.is_loopback()
        && !ip.is_unspecified()
        && !ip.is_multicast()
        && !matches!(ip, IpAddr::V4(v4) if v4.is_broadcast())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn response(query: &Message, code: ResponseCode) -> Message {
    let mut response = Message::response(query.metadata.id, query.metadata.op_code);
    response.metadata.response_code = code;
    response.metadata.recursion_desired = query.metadata.recursion_desired;
    response.queries = query.queries.clone();
    if let Some(mut edns) = query.edns.clone() {
        edns.set_max_payload(1232);
        response.edns = Some(edns);
    }
    response
}

fn authoritative(
    query: &Message,
    directory: &ServiceDirectory,
    view: &DnsView,
    now: u64,
) -> Option<Message> {
    if query.metadata.message_type != MessageType::Query
        || query.metadata.op_code != OpCode::Query
        || query.queries.len() != 1
        || !query.answers.is_empty()
        || !query.authorities.is_empty()
    {
        return Some(response(query, ResponseCode::FormErr));
    }
    let question = &query.queries[0];
    let host = question.name().to_ascii();
    let scope = classify_host(&host, directory.network());
    if scope == ServiceHost::Public {
        return None;
    }
    let zone = format!("{}.tako.internal.", directory.network());
    let hostname = format!("{}.", host.trim_end_matches('.').to_ascii_lowercase());
    if hostname != zone && !hostname.ends_with(&format!(".{zone}")) {
        return Some(response(query, ResponseCode::Refused));
    }
    let mut answer = response(query, ResponseCode::NoError);
    answer.metadata.authoritative = true;
    if question.query_class() != DNSClass::IN {
        answer.metadata.response_code = ResponseCode::Refused;
        return Some(answer);
    }
    let placements = match scope {
        ServiceHost::App(ref app) => directory.placements(app, now),
        _ => Vec::new(),
    };
    if placements.is_empty() && hostname != zone {
        answer.metadata.response_code = ResponseCode::NXDomain;
    }
    match (view, question.query_type()) {
        (DnsView::Native, RecordType::AAAA) => {
            for placement in &placements {
                answer.add_answer(Record::from_rdata(
                    question.name().clone(),
                    placement.ttl(now),
                    RData::AAAA(AAAA(placement.address)),
                ));
            }
        }
        (DnsView::Compose { gateway, .. }, RecordType::A) if !placements.is_empty() => {
            let ttl = placements.iter().map(|p| p.ttl(now)).min().unwrap_or(0);
            answer.add_answer(Record::from_rdata(
                question.name().clone(),
                ttl,
                RData::A(A(*gateway)),
            ));
        }
        _ => {}
    }
    if answer.answers.is_empty() {
        let zone_name = Name::from_ascii(&zone).expect("canonical network zone");
        let soa = SOA::new(
            zone_name.clone(),
            Name::from_ascii(format!("hostmaster.{zone}")).unwrap(),
            0,
            5,
            5,
            30,
            0,
        );
        answer.add_authority(Record::from_rdata(zone_name, 0, RData::SOA(soa)));
    }
    Some(answer)
}

fn encode(answer: &Message, udp: bool, limit: u16) -> io::Result<Vec<u8>> {
    let bytes = answer.to_vec().map_err(io::Error::other)?;
    if udp && bytes.len() > usize::from(limit.clamp(512, 1232)) {
        answer.truncate().to_vec().map_err(io::Error::other)
    } else {
        Ok(bytes)
    }
}

async fn forward(query: &Message, upstream: SocketAddr, tcp: bool) -> io::Result<Message> {
    let bytes = query.to_vec().map_err(io::Error::other)?;
    let reply = if tcp {
        let mut stream = TcpStream::connect(upstream).await?;
        stream.write_u16(bytes.len() as u16).await?;
        stream.write_all(&bytes).await?;
        let size = usize::from(stream.read_u16().await?);
        let mut reply = vec![0; size];
        stream.read_exact(&mut reply).await?;
        reply
    } else {
        let socket = UdpSocket::bind(if upstream.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        })
        .await?;
        socket.connect(upstream).await?;
        socket.send(&bytes).await?;
        let mut reply = vec![0; MAX_TCP_RESPONSE];
        let size = socket.recv(&mut reply).await?;
        reply.truncate(size);
        reply
    };
    let reply = Message::from_vec(&reply).map_err(io::Error::other)?;
    if reply.metadata.id != query.metadata.id
        || reply.metadata.message_type != MessageType::Response
        || reply.metadata.op_code != query.metadata.op_code
        || reply.queries != query.queries
    {
        return Err(io::Error::other("upstream DNS response mismatch"));
    }
    Ok(reply)
}

async fn answer(
    bytes: &[u8],
    directory: &RwLock<ServiceDirectory>,
    view: &DnsView,
    udp: bool,
) -> io::Result<Vec<u8>> {
    let query = Message::from_vec(bytes).map_err(io::Error::other)?;
    let limit = query.max_payload();
    let local = authoritative(&query, &directory.read(), view, now());
    if let Some(answer) = local {
        return encode(&answer, udp, limit);
    }
    if let DnsView::Compose { upstreams, .. } = view {
        for &upstream in upstreams {
            if directory
                .read()
                .view
                .as_ref()
                .is_some_and(|v| v.peers.contains(&upstream))
            {
                continue;
            }
            let result = tokio::time::timeout(
                Duration::from_millis(800),
                forward(&query, SocketAddr::new(upstream, 53), !udp),
            )
            .await;
            if let Ok(Ok(reply)) = result {
                return encode(&reply, udp, limit);
            }
        }
        return encode(&response(&query, ResponseCode::ServFail), udp, limit);
    }
    encode(&response(&query, ResponseCode::Refused), udp, limit)
}

pub async fn serve_dns(
    directory: Arc<RwLock<ServiceDirectory>>,
    bind: SocketAddr,
    view: DnsView,
    mut shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    if bind.ip().is_unspecified() {
        return Err(io::Error::other("DNS requires an exact private address"));
    }
    if let DnsView::Compose { gateway, upstreams } = &view {
        if bind.ip() != IpAddr::V4(*gateway)
            || upstreams.is_empty()
            || upstreams.len() > 3
            || upstreams.iter().any(|ip| {
                !valid_upstream(*ip)
                    || ip.to_canonical() == bind.ip().to_canonical()
                    || directory
                        .read()
                        .view
                        .as_ref()
                        .is_some_and(|v| v.peers.contains(ip))
            })
        {
            return Err(io::Error::other("invalid Compose DNS gateway or upstream"));
        }
    }
    let tcp = TcpListener::bind(bind).await?;
    let bind = tcp.local_addr()?;
    let udp = Arc::new(UdpSocket::bind(bind).await?);
    let mut jobs = JoinSet::new();
    let mut buffer = vec![0; MAX_QUERY + 1];
    loop {
        if *shutdown.borrow() {
            break;
        }
        tokio::select! {
            changed = shutdown.changed() => { if changed.is_err() { break; } }
            _ = jobs.join_next(), if !jobs.is_empty() => {}
            result = udp.recv_from(&mut buffer) => {
                let (length, peer) = result?;
                if length > MAX_QUERY || jobs.len() >= CONNECTIONS
                    || !directory.read().admits_dns(bind.ip(), peer.ip(), now()) { continue; }
                let query = buffer[..length].to_vec();
                let (directory, view, udp) = (directory.clone(), view.clone(), udp.clone());
                jobs.spawn(async move {
                    if let Ok(Ok(reply)) = tokio::time::timeout(TIMEOUT, answer(&query, &directory, &view, true)).await {
                        if directory.read().admits_dns(bind.ip(), peer.ip(), now()) { let _ = udp.send_to(&reply, peer).await; }
                    }
                });
            }
            result = tcp.accept(), if jobs.len() < CONNECTIONS => {
                let (mut stream, peer) = result?;
                if !directory.read().admits_dns(bind.ip(), peer.ip(), now()) { continue; }
                let (directory, view) = (directory.clone(), view.clone());
                jobs.spawn(async move {
                    let _ = tokio::time::timeout(TIMEOUT, async {
                        let length = usize::from(stream.read_u16().await?);
                        if length > MAX_QUERY { return Err(io::Error::other("DNS query too large")); }
                        let mut query = vec![0; length];
                        stream.read_exact(&mut query).await?;
                        let reply = answer(&query, &directory, &view, false).await?;
                        if !directory.read().admits_dns(bind.ip(), peer.ip(), now()) { return Ok(()); }
                        stream.write_u16(reply.len() as u16).await?;
                        stream.write_all(&reply).await
                    }).await;
                });
            }
        }
    }
    jobs.abort_all();
    while jobs.join_next().await.is_some() {}
    Ok(())
}

#[cfg(test)]
mod tests;
