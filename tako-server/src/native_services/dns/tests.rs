use super::*;
use crate::native_services::tests::{fixture, lease};
use hickory_proto::op::Query;

fn query(host: &str, kind: RecordType) -> Message {
    let mut query = Message::query();
    query.add_query(Query::query(Name::from_ascii(host).unwrap(), kind));
    query
}

#[test]
fn native_dns_only_answers_current_private_placements() {
    let (registry, keys, mut directory) = fixture();
    let lease = lease(&registry, &keys, &["api/prod"], 10);
    directory.replace(&registry, &[lease], 10).unwrap();
    let host = format!("api.prod.{}.tako.internal", registry.network.id);
    let packet = query(&host, RecordType::AAAA).to_vec().unwrap();
    let request = Message::from_vec(&packet).unwrap();
    let reply = authoritative(&request, &directory, &DnsView::Native, 10).unwrap();
    let reply = Message::from_vec(&reply.to_vec().unwrap()).unwrap();
    assert!(reply.metadata.authoritative);
    assert!(!reply.metadata.recursion_available);
    assert_eq!(reply.answers.len(), 1);
    assert!(reply.answers[0].ttl <= 5);
    assert_eq!(
        reply.answers[0].data,
        RData::AAAA(AAAA(
            registry.network.id.address(&keys.wg_public_key().unwrap())
        ))
    );
    let a = authoritative(
        &query(&host, RecordType::A),
        &directory,
        &DnsView::Native,
        10,
    )
    .unwrap();
    assert!(a.answers.is_empty());
    assert_eq!(a.metadata.response_code, ResponseCode::NoError);
    let expired = authoritative(&request, &directory, &DnsView::Native, 40).unwrap();
    assert_eq!(expired.metadata.response_code, ResponseCode::NXDomain);
    assert!(expired.answers.is_empty());
    assert!(
        authoritative(
            &query("example.com", RecordType::A),
            &directory,
            &DnsView::Native,
            10
        )
        .is_none()
    );
    let other = authoritative(
        &query("api.prod.other.tako.internal", RecordType::A),
        &directory,
        &DnsView::Native,
        10,
    )
    .unwrap();
    assert_eq!(other.metadata.response_code, ResponseCode::Refused);
}

#[test]
fn compose_dns_returns_only_gateway_a_and_never_external_fallback_for_private_names() {
    let (registry, keys, mut directory) = fixture();
    directory
        .replace(&registry, &[lease(&registry, &keys, &["api/prod"], 10)], 10)
        .unwrap();
    let view = DnsView::Compose {
        gateway: "10.70.80.1".parse().unwrap(),
        upstreams: vec![],
    };
    let host = format!("api.prod.{}.tako.internal", registry.network.id);
    let response = authoritative(&query(&host, RecordType::A), &directory, &view, 10).unwrap();
    assert_eq!(
        response.answers[0].data,
        RData::A(A("10.70.80.1".parse().unwrap()))
    );
    let response = authoritative(&query(&host, RecordType::AAAA), &directory, &view, 10).unwrap();
    assert_eq!(response.metadata.response_code, ResponseCode::NoError);
    assert!(response.answers.is_empty());
    let missing = format!("missing.prod.{}.tako.internal", registry.network.id);
    assert_eq!(
        authoritative(&query(&missing, RecordType::A), &directory, &view, 10)
            .unwrap()
            .metadata
            .response_code,
        ResponseCode::NXDomain
    );
    assert!(authoritative(&query("example.com", RecordType::A), &directory, &view, 10).is_none());
}

#[test]
fn malformed_queries_and_oversized_udp_answers_are_bounded() {
    let (_, _, directory) = fixture();
    let mut request = query("example.com", RecordType::A);
    request.add_query(Query::query(
        Name::from_ascii("other.com").unwrap(),
        RecordType::AAAA,
    ));
    assert_eq!(
        authoritative(&request, &directory, &DnsView::Native, 10)
            .unwrap()
            .metadata
            .response_code,
        ResponseCode::FormErr
    );
    let mut response = response(
        &query("example.com", RecordType::AAAA),
        ResponseCode::NoError,
    );
    for n in 1..100u16 {
        response.add_answer(Record::from_rdata(
            Name::from_ascii("example.com").unwrap(),
            5,
            RData::AAAA(AAAA(std::net::Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, n))),
        ));
    }
    let bytes = encode(&response, true, 512).unwrap();
    assert!(bytes.len() <= 512);
    assert!(Message::from_vec(&bytes).unwrap().metadata.truncation);
    assert!(
        !Message::from_vec(&encode(&response, false, 512).unwrap())
            .unwrap()
            .metadata
            .truncation
    );
    assert!(valid_upstream("169.254.169.254".parse().unwrap()));
    for address in [
        "127.0.0.53",
        "::1",
        "::ffff:127.0.0.53",
        "0.0.0.0",
        "224.0.0.1",
    ] {
        assert!(!valid_upstream(address.parse().unwrap()));
    }
}

#[tokio::test]
async fn serves_real_udp_and_tcp_queries_and_stops_on_shutdown() {
    let (registry, keys, mut directory) = fixture();
    let timestamp = now();
    directory
        .replace(
            &registry,
            &[lease(&registry, &keys, &["api/prod"], timestamp)],
            timestamp,
        )
        .unwrap();
    let view = directory.view.as_mut().unwrap();
    view.address = std::net::Ipv6Addr::LOCALHOST;
    view.peers.insert(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST));
    let reservation = TcpListener::bind("[::1]:0").await.unwrap();
    let bind = reservation.local_addr().unwrap();
    drop(reservation);
    let (shutdown, shutdown_rx) = watch::channel(false);
    let directory = Arc::new(RwLock::new(directory));
    let task = tokio::spawn(serve_dns(directory, bind, DnsView::Native, shutdown_rx));
    let mut tcp = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(stream) = TcpStream::connect(bind).await {
                break stream;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let host = format!("api.prod.{}.tako.internal", registry.network.id);
    let request = query(&host, RecordType::AAAA).to_vec().unwrap();
    tcp.write_u16(request.len() as u16).await.unwrap();
    tcp.write_all(&request).await.unwrap();
    let length = tcp.read_u16().await.unwrap();
    let mut reply = vec![0; length as usize];
    tcp.read_exact(&mut reply).await.unwrap();
    assert_eq!(Message::from_vec(&reply).unwrap().answers.len(), 1);
    let udp = UdpSocket::bind("[::1]:0").await.unwrap();
    udp.connect(bind).await.unwrap();
    udp.send(&request).await.unwrap();
    let mut reply = [0; 4096];
    let length = tokio::time::timeout(Duration::from_secs(2), udp.recv(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        Message::from_vec(&reply[..length]).unwrap().answers.len(),
        1
    );
    let public = query("example.com", RecordType::A).to_vec().unwrap();
    udp.send(&public).await.unwrap();
    let length = tokio::time::timeout(Duration::from_secs(2), udp.recv(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        Message::from_vec(&reply[..length])
            .unwrap()
            .metadata
            .response_code,
        ResponseCode::Refused
    );
    shutdown.send_replace(true);
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn forwards_public_udp_and_tcp_and_rejects_mismatched_replies() {
    let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let upstream = udp.local_addr().unwrap();
    // Forwarded queries arrive from the wire, where names are fully qualified;
    // the reply comparison in `forward` holds only for such a query.
    let query = Message::from_vec(&query("example.com", RecordType::A).to_vec().unwrap()).unwrap();
    let worker = tokio::spawn(async move {
        for tamper in [false, true] {
            let mut buffer = [0; 4096];
            let (size, peer) = udp.recv_from(&mut buffer).await.unwrap();
            let request = Message::from_vec(&buffer[..size]).unwrap();
            let mut reply = response(&request, ResponseCode::NoError);
            reply.metadata.recursion_available = true;
            if tamper {
                reply.metadata.id = reply.metadata.id.wrapping_add(1);
            }
            reply.add_answer(Record::from_rdata(
                request.queries[0].name().clone(),
                60,
                RData::A(A("192.0.2.42".parse().unwrap())),
            ));
            udp.send_to(&reply.to_vec().unwrap(), peer).await.unwrap();
        }
    });
    let reply = tokio::time::timeout(TIMEOUT, forward(&query, upstream, false))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        reply.answers[0].data,
        RData::A(A("192.0.2.42".parse().unwrap()))
    );
    assert!(
        tokio::time::timeout(TIMEOUT, forward(&query, upstream, false))
            .await
            .unwrap()
            .is_err()
    );
    worker.await.unwrap();
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = tcp.local_addr().unwrap();
    let worker = tokio::spawn(async move {
        let (mut stream, _) = tcp.accept().await.unwrap();
        let length = stream.read_u16().await.unwrap();
        let mut bytes = vec![0; length as usize];
        stream.read_exact(&mut bytes).await.unwrap();
        let request = Message::from_vec(&bytes).unwrap();
        let reply = response(&request, ResponseCode::NXDomain).to_vec().unwrap();
        stream.write_u16(reply.len() as u16).await.unwrap();
        stream.write_all(&reply).await.unwrap();
    });
    let reply = tokio::time::timeout(TIMEOUT, forward(&query, upstream, true))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reply.metadata.response_code, ResponseCode::NXDomain);
    worker.await.unwrap();
}
