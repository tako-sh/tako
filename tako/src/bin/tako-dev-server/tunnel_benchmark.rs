//! Manual transport benchmark. See scripts/benchmark-tunnel.md.
use super::*;
use openssl::{
    pkey::PKey,
    ssl::{SslAcceptor, SslMethod},
    x509::X509,
};
use std::io::{Read, Write};
use tokio_tungstenite::tungstenite::protocol::Role;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual benchmark; run with controlled network conditions"]
async fn concurrent_transport() {
    let concurrency: usize = std::env::var("TUNNEL_BENCH_CONCURRENCY")
        .unwrap_or_else(|_| "8".into())
        .parse()
        .unwrap();
    assert!((1..=24).contains(&concurrency));
    let body_size: usize = std::env::var("TUNNEL_BENCH_BODY_BYTES")
        .unwrap_or_else(|_| "2097152".into())
        .parse()
        .unwrap();
    let write_size: usize = std::env::var("TUNNEL_BENCH_CHUNK_BYTES")
        .unwrap_or_else(|_| "8192".into())
        .parse()
        .unwrap();
    assert!(body_size > 0 && write_size > 0);
    let ca = tako::dev::LocalCA::generate().unwrap();
    let cert = ca.generate_leaf_cert("app.test").unwrap();
    let mut acceptor = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
    acceptor
        .set_certificate(&X509::from_pem(cert.cert_pem.as_bytes()).unwrap())
        .unwrap();
    acceptor
        .set_private_key(&PKey::private_key_from_pem(cert.key_pem.as_bytes()).unwrap())
        .unwrap();
    let acceptor = Arc::new(acceptor.build());
    let local = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = local.local_addr().unwrap().to_string();
    let client = local_proxy_client_with_root("app.test", &addr, ca.ca_cert_pem()).unwrap();
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            let (stream, _) = local.accept().await.unwrap();
            let stream = stream.into_std().unwrap();
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(30)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(30)))
                .unwrap();
            let acceptor = Arc::clone(&acceptor);
            connections.spawn_blocking(move || {
                let mut stream = acceptor.accept(stream).unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                let size = if request.starts_with(b"GET /bulk ") {
                    body_size
                } else {
                    32
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {size}\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
                // TLS fragments are intentionally smaller than a tunnel chunk.
                for data in vec![42; size].chunks(write_size) {
                    stream.write_all(data).unwrap();
                }
                stream.flush().unwrap();
            });
        }
    });
    // Only traffic to/from 127.0.0.2 is impaired by the benchmark's netem filter.
    let listener = tokio::net::TcpListener::bind("127.0.0.2:0").await.unwrap();
    let (socket, accepted) = tokio::join!(
        TcpStream::connect(listener.local_addr().unwrap()),
        listener.accept()
    );
    let socket = socket.unwrap();
    socket.set_nodelay(true).unwrap();
    let (peer, _) = accepted.unwrap();
    peer.set_nodelay(true).unwrap();
    let socket = WebSocketStream::from_raw_socket(socket, Role::Client, None).await;
    let relay = WebSocketStream::from_raw_socket(peer, Role::Server, None).await;
    let snapshot = TunnelSnapshot {
        config_path: "/tmp/tunnel-benchmark.toml".into(),
        app_name: "app".into(),
        local_host: "app.test".into(),
        listen_addr: addr,
    };
    let state = super::tests::tunnel_state_with_app(&snapshot.config_path, 0, false);
    tasks.spawn(async move {
        run_connection(
            socket,
            snapshot.clone(),
            state,
            snapshot.config_path,
            client,
        )
        .await
        .unwrap();
    });
    let (mut writer, mut reader) = relay.split();
    let (outbound, mut outgoing) = mpsc::unbounded_channel();
    tasks.spawn(async move {
        while let Some(message) = outgoing.recv().await {
            writer
                .send(Message::Binary(
                    rmp_serde::to_vec_named(&message).unwrap().into(),
                ))
                .await
                .unwrap();
        }
    });
    let mut starts = BTreeMap::new();
    let mut received = BTreeMap::<String, usize>::new();
    let started = std::time::Instant::now();
    for i in 0..concurrency {
        let id = format!("bulk-{i}");
        starts.insert(id.clone(), std::time::Instant::now());
        outbound
            .send(ServerMessage::Request(TunnelRequestMessage {
                id,
                method: "GET".into(),
                path: "/bulk".into(),
                headers: vec![],
                body: vec![],
            }))
            .unwrap();
    }
    let mut probes_sent = 0;
    let mut probes_done = 0;
    let mut bulk_done = 0;
    let mut bulk_ms = 0.0;
    let mut latencies = Vec::new();
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    while probes_done < 20 || bulk_done < concurrency {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => panic!("benchmark timed out"),
            _ = tick.tick(), if probes_sent < 20 && probes_sent - probes_done < 4 => {
                let id = format!("probe-{probes_sent}"); probes_sent += 1;
                starts.insert(id.clone(), std::time::Instant::now());
                outbound.send(ServerMessage::Request(TunnelRequestMessage { id, method: "GET".into(), path: "/probe".into(), headers: vec![], body: vec![] })).unwrap();
            }
            message = reader.next() => {
                let Message::Binary(data) = message.unwrap().unwrap() else { continue; };
                match rmp_serde::from_slice::<ClientMessage>(&data).unwrap() {
                    ClientMessage::Ping { nonce } => { outbound.send(ServerMessage::Pong { nonce }).unwrap(); }
                    ClientMessage::Response(head) => { assert_eq!(head.status, 200); }
                    ClientMessage::ResponseChunk(chunk) => {
                        let count = received.entry(chunk.id.clone()).or_default();
                        if *count == 0 && chunk.id.starts_with("probe-") { latencies.push(starts[&chunk.id].elapsed().as_secs_f64() * 1000.0); }
                        *count += chunk.data.len();
                        outbound.send(ServerMessage::ResponseCredit { bytes: response_chunk_cost(chunk.data.len()), id: chunk.id }).unwrap();
                    }
                    ClientMessage::ResponseEnd { id } => {
                        if id.starts_with("bulk-") { assert_eq!(received[&id], body_size); bulk_done += 1; bulk_ms = started.elapsed().as_secs_f64() * 1000.0; }
                        else { assert_eq!(received[&id], 32); probes_done += 1; }
                    }
                    other => panic!("unexpected benchmark message: {other:?}"),
                }
            }
        }
    }
    latencies.sort_by(f64::total_cmp);
    println!(
        "TUNNEL_BENCH {}",
        serde_json::json!({"concurrency":concurrency,"bytes_per_bulk":body_size,"local_write_bytes":write_size,"probe_count":latencies.len(),"probe_first_byte_p50_ms":latencies[9],"probe_first_byte_p95_ms":latencies[18],"bulk_complete_ms":bulk_ms,"bulk_mib_per_s":concurrency as f64 * body_size as f64 / 1048576.0 / (bulk_ms / 1000.0)})
    );
}
