use super::*;
use super::{
    broker::{Broker, Command},
    ipc::{Notice, Reply, Request, Response},
    peer::{Frame, Hello},
    wire::{Budget, MAX_FRAME},
};
use std::net::IpAddr;
use tako_channels::{
    ChannelError, ChannelEvent, ChannelEventId, ChannelMessage, ChannelPublication,
    ChannelPublishPayload, ChannelStore,
};
use tako_network::{
    AppEnv, DeviceKind, LocalDeviceKeys, Member, MemberKind, NetworkId, Permissions, ServerInfo,
};
use tokio::{io::AsyncWriteExt, sync::oneshot};

fn remote_id() -> DeviceId {
    "11".repeat(32).parse().unwrap()
}
fn local_id() -> DeviceId {
    "00".repeat(32).parse().unwrap()
}
fn app() -> AppEnv {
    "chat/production".parse().unwrap()
}
fn payload(n: usize) -> ChannelPublishPayload {
    ChannelPublishPayload {
        r#type: "update".into(),
        data: serde_json::json!(n),
    }
}
fn member(network: &NetworkId, keys: &LocalDeviceKeys) -> Member {
    let request = keys
        .enrollment_request(
            Some(network.clone()),
            DeviceKind::Server,
            "server".parse().unwrap(),
        )
        .unwrap();
    Member {
        network_id: network.clone(),
        device_id: request.device_id.clone().unwrap(),
        kind: MemberKind::Server(ServerInfo {
            wireguard_endpoint: "127.0.0.1:51820".parse().unwrap(),
            ssh_host_key: keys.ssh_public_key().unwrap(),
            ssh_port: 22,
            https_port: 443,
            https_certificate_pem: None,
            corrosion_ca_certificate_pem: None,
            https_spki_sha256: None,
        }),
        request,
        permissions: Permissions {
            network_admin: true,
            server_admin: true,
            ..Default::default()
        },
        approved_by: None,
        independent_admin: true,
        wg_verified: true,
        created_at: 1,
        expires_at: None,
    }
}
fn identities() -> (NetworkId, LocalDeviceKeys, LocalDeviceKeys, Member, Member) {
    let net = NetworkId::random().unwrap();
    let a = LocalDeviceKeys::generate().unwrap();
    let b = LocalDeviceKeys::generate().unwrap();
    let ma = member(&net, &a);
    let mb = member(&net, &b);
    if ma.device_id < mb.device_id {
        (net, a, b, ma, mb)
    } else {
        (net, b, a, mb, ma)
    }
}
async fn handshake_case(
    wrong_key: bool,
    wrong_network: bool,
    wrong_ip: bool,
    laptop: bool,
) -> (bool, bool) {
    let (net, a, b, ma, mut mb) = identities();
    if laptop {
        mb.kind = MemberKind::Laptop;
    }
    let rogue = LocalDeviceKeys::generate().unwrap();
    let other_net = NetworkId::random().unwrap();
    let ra = BTreeMap::from([(mb.device_id.clone(), mb.clone())]);
    let rb = BTreeMap::from([(ma.device_id.clone(), ma.clone())]);
    let (mut x, mut y) = tokio::io::duplex(32 * 1024);
    let ip_a = IpAddr::V6(net.address(&ma.request.wg_public_key));
    let ip_b = if wrong_ip {
        "::1".parse().unwrap()
    } else {
        IpAddr::V6(net.address(&mb.request.wg_public_key))
    };
    let (left, right) = tokio::join!(
        peer::authenticate(
            &mut x,
            &net,
            &ma.device_id,
            &a,
            &ra,
            ip_b,
            Some(&mb.device_id)
        ),
        peer::authenticate(
            &mut y,
            if wrong_network { &other_net } else { &net },
            &mb.device_id,
            if wrong_key { &rogue } else { &b },
            &rb,
            ip_a,
            None
        ),
    );
    (left.is_ok(), right.is_ok())
}
#[tokio::test]
async fn mutually_authenticates_real_duplex() {
    assert_eq!(
        handshake_case(false, false, false, false).await,
        (true, true)
    );
}
#[tokio::test]
async fn denies_wrong_key_network_ip_and_server_role() {
    for flags in [
        (true, false, false, false),
        (false, true, false, false),
        (false, false, true, false),
        (false, false, false, true),
    ] {
        assert!(!handshake_case(flags.0, flags.1, flags.2, flags.3).await.0);
    }
}
#[tokio::test]
async fn captured_signature_cannot_answer_fresh_connection_challenge() {
    let (net, a, b, ma, mb) = identities();
    let roster = BTreeMap::from([(mb.device_id.clone(), mb.clone())]);
    let mut captured = None;
    for replay in [false, true] {
        let (mut honest, mut attacker) = tokio::io::duplex(32 * 1024);
        let ip = IpAddr::V6(net.address(&mb.request.wg_public_key));
        let attempt = peer::authenticate(
            &mut honest,
            &net,
            &ma.device_id,
            &a,
            &roster,
            ip,
            Some(&mb.device_id),
        );
        let malicious = async {
            let budget = Budget::new(8192);
            let hello: Hello = wire::read(&mut attacker, &budget).await.unwrap().value;
            let remote = Hello {
                network: net.clone(),
                server: mb.device_id.clone(),
                challenge: [7; 32],
            };
            wire::write(&mut attacker, &remote).await.unwrap();
            let _: serde_json::Value = wire::read(&mut attacker, &budget).await.unwrap().value;
            let signature = if replay {
                captured.clone().unwrap()
            } else {
                b.sign(
                    peer::NAMESPACE,
                    &peer::transcript(&hello, &remote, "acceptor").unwrap(),
                )
                .unwrap()
                .to_pem(ssh_key::LineEnding::LF)
                .unwrap()
            };
            wire::write(&mut attacker, &serde_json::json!({"signature":signature}))
                .await
                .unwrap();
            signature
        };
        let (result, signature) = tokio::join!(attempt, malicious);
        assert_eq!(result.is_ok(), !replay);
        captured = Some(signature);
    }
}
#[test]
fn signatures_bind_both_identities_challenges_network_and_direction() {
    let (net, a, _, ma, mb) = identities();
    let hello = Hello {
        network: net.clone(),
        server: ma.device_id.clone(),
        challenge: [1; 32],
    };
    let other = Hello {
        network: net,
        server: mb.device_id,
        challenge: [2; 32],
    };
    let bytes = peer::transcript(&hello, &other, "dialer").unwrap();
    let signature = a
        .sign(peer::NAMESPACE, &bytes)
        .unwrap()
        .to_pem(ssh_key::LineEnding::LF)
        .unwrap();
    let key = a.ssh_public_key().unwrap();
    peer::verify(&key, &bytes, &signature).unwrap();
    let mut changed = hello.clone();
    changed.challenge[0] ^= 1;
    assert!(
        peer::verify(
            &key,
            &peer::transcript(&changed, &other, "dialer").unwrap(),
            &signature
        )
        .is_err()
    );
    changed = hello.clone();
    changed.network = NetworkId::random().unwrap();
    assert!(
        peer::verify(
            &key,
            &peer::transcript(&changed, &other, "dialer").unwrap(),
            &signature
        )
        .is_err()
    );
    assert!(
        peer::verify(
            &key,
            &peer::transcript(&hello, &other, "acceptor").unwrap(),
            &signature
        )
        .is_err()
    );
    assert!(
        peer::verify(
            &key,
            &peer::transcript(&other, &hello, "dialer").unwrap(),
            &signature
        )
        .is_err()
    );
}
async fn new_broker() -> (tempfile::TempDir, Broker) {
    let temp = tempfile::tempdir().unwrap();
    let mut broker = Broker::new(temp.path().into(), Budget::new(8 * MAX_FRAME), local_id());
    assert!(matches!(
        broker
            .local(Request::Register {
                app: app(),
                postgres_url: None
            })
            .await
            .unwrap(),
        Reply::Response(Response::Ok)
    ));
    (temp, broker)
}

#[tokio::test]
async fn registration_reopens_channels_after_data_tree_restore() {
    let (temp, mut broker) = new_broker().await;
    publish(&mut broker, 1).await;
    let previous = broker.apps.get(&app()).unwrap().store.clone();
    let root = temp.path().join(app().as_str());
    std::fs::rename(root.join("data"), root.join("previous-data")).unwrap();
    std::fs::create_dir_all(root.join("data/tako")).unwrap();
    assert!(matches!(
        broker
            .local(Request::Register {
                app: app(),
                postgres_url: None
            })
            .await
            .unwrap(),
        Reply::Response(Response::Ok)
    ));
    let current = broker.apps.get(&app()).unwrap().store.clone();
    assert!(
        !Arc::ptr_eq(&previous, &current),
        "restore must replace the old writer"
    );
    publish(&mut broker, 2).await;
    assert_eq!(current.read_after("events", None, 10).unwrap().len(), 1);
    let reader = ChannelStore::open_reader_config(tako_channels::ChannelStoreConfig::sqlite(
        root.join("data/tako/channels.sqlite"),
    ))
    .unwrap();
    assert_eq!(
        reader.read_after("events", None, 10).unwrap()[0].data,
        serde_json::json!(2)
    );
}
async fn publish(broker: &mut Broker, n: usize) -> ChannelMessage {
    match broker
        .local(Request::Publish {
            app: app(),
            channel: "events".into(),
            payload: payload(n),
            ephemeral: false,
            lifecycle: None,
        })
        .await
        .unwrap()
    {
        Reply::Response(Response::Published { message }) => message,
        _ => panic!("publish rejected"),
    }
}
fn stale(store: &ChannelStore, message: &ChannelMessage) {
    assert!(matches!(
        store.replay_cursor("events", Some(message.id.parse().unwrap())),
        Err(ChannelError::StaleCursor)
    ));
}
#[tokio::test]
async fn fifo_commit_and_mesh_order_across_concurrent_local_duplex_publishers() {
    let (_temp, mut broker) = new_broker().await;
    let (_, _, _, _, mb) = identities();
    let (sender, mut outgoing) = wire::queue(Budget::new(16 * MAX_FRAME));
    broker.peers.insert(mb.device_id, sender);
    let store = broker.apps[&app()].store.clone();
    let (commands, receiver) = mpsc::channel(128);
    let (_control, control_rx) = watch::channel(broker::Control::default());
    let (shutdown, shutdown_rx) = watch::channel(false);
    let actor = tokio::spawn(broker::run(broker, receiver, control_rx, shutdown_rx));
    let mut jobs = JoinSet::new();
    for n in 0..32 {
        let commands = commands.clone();
        jobs.spawn(async move {
            let (mut client, server) = tokio::io::duplex(8192);
            let task = tokio::spawn(ipc::serve(
                server,
                commands,
                Budget::new(MAX_FRAME),
                Budget::new(8 * MAX_FRAME),
            ));
            wire::write(
                &mut client,
                &Request::Publish {
                    app: app(),
                    channel: "events".into(),
                    payload: payload(n),
                    ephemeral: n % 2 == 0,
                    lifecycle: None,
                },
            )
            .await
            .unwrap();
            let response: Response = wire::read(&mut client, &Budget::new(MAX_FRAME))
                .await
                .unwrap()
                .value;
            task.await.unwrap().unwrap();
            assert!(matches!(response, Response::Published { .. }));
        });
    }
    while let Some(result) = jobs.join_next().await {
        result.unwrap();
    }
    let mut durable = Vec::new();
    for sequence in 1..=32 {
        let packet = outgoing.recv().await.unwrap();
        let frame: Frame = serde_json::from_slice(&packet.bytes).unwrap();
        let Frame::Publication {
            app: scope,
            publication: ChannelPublication::Event(event),
        } = frame
        else {
            panic!("missing event");
        };
        assert_eq!(scope, app());
        assert_eq!(event.id.sequence, sequence);
        if !event.ephemeral {
            durable.push(event.id);
        }
    }
    let rows = store.read_after("events", None, 100).unwrap();
    assert_eq!(
        rows.iter()
            .map(|m| m.event_id.clone().unwrap())
            .collect::<Vec<_>>(),
        durable
    );
    shutdown.send_replace(true);
    actor.await.unwrap().unwrap();
}
#[tokio::test]
async fn remote_ingest_is_idempotent_not_echoed_and_notified_without_repersisting() {
    let (_temp, mut broker) = new_broker().await;
    let source = ChannelStore::open_in_memory().unwrap();
    let mut publications = source.publications();
    let (_, _, _, _, mb) = identities();
    let (sender, mut outgoing) = wire::queue(Budget::new(MAX_FRAME));
    broker.peers.insert(mb.device_id, sender);
    let Reply::Subscription(mut notices) = broker
        .local(Request::Subscribe { app: app() })
        .await
        .unwrap()
    else {
        panic!()
    };
    for ephemeral in [false, true] {
        if ephemeral {
            source.publish_ephemeral("events", &payload(1)).unwrap();
        } else {
            source.append("events", &payload(1)).unwrap();
        }
        let publication = publications.recv().await.unwrap();
        let frame = Frame::Publication {
            app: app(),
            publication,
        };
        broker.remote(&remote_id(), frame.clone()).await.unwrap();
        broker.remote(&remote_id(), frame).await.unwrap();
        let packet = notices.recv().await.unwrap();
        let Notice::Notification { notification } = serde_json::from_slice(&packet.bytes).unwrap()
        else {
            panic!()
        };
        if ephemeral {
            assert!(matches!(
                notification,
                Notification::Committed {
                    ephemeral: true,
                    ..
                }
            ));
        } else {
            assert!(matches!(notification, Notification::Wake { .. }));
        }
        assert!(notices.try_recv().is_err());
    }
    assert!(outgoing.try_recv().is_err());
    assert_eq!(
        broker.apps[&app()]
            .store
            .read_after("events", None, 10)
            .unwrap()
            .len(),
        1
    );
}
#[tokio::test]
async fn closing_replicates_the_marker_once_and_closed_streams_reject_publishes() {
    let (_temp, mut broker) = new_broker().await;
    let (_, _, _, _, mb) = identities();
    let (sender, mut outgoing) = wire::queue(Budget::new(16 * MAX_FRAME));
    broker.peers.insert(mb.device_id, sender);
    let Reply::Subscription(mut notices) = broker
        .local(Request::Subscribe { app: app() })
        .await
        .unwrap()
    else {
        panic!()
    };
    let sent = publish(&mut broker, 1).await;
    let close = ChannelPublishPayload {
        r#type: tako_channels::CLOSE_MESSAGE_TYPE.into(),
        data: serde_json::Value::Null,
    };
    let request = |payload: ChannelPublishPayload| Request::Publish {
        app: app(),
        channel: "events".into(),
        payload,
        ephemeral: false,
        lifecycle: None,
    };
    let Reply::Response(Response::Published { message: marker }) =
        broker.local(request(close.clone())).await.unwrap()
    else {
        panic!("close rejected")
    };
    assert!(tako_channels::is_close_marker(&marker));
    // Closing again returns the marker without committing, notifying, or
    // fencing replay.
    let Reply::Response(Response::Published { message: again }) =
        broker.local(request(close)).await.unwrap()
    else {
        panic!("repeated close rejected")
    };
    assert_eq!(again.id, marker.id);
    let mut notified = 0;
    while let Ok(packet) = notices.try_recv() {
        let Notice::Notification { notification } = serde_json::from_slice(&packet.bytes).unwrap()
        else {
            panic!()
        };
        assert!(matches!(notification, Notification::Committed { .. }));
        notified += 1;
    }
    assert_eq!(notified, 2);
    // Over IPC, a publish to the closed stream is answered by name.
    let (commands, receiver) = mpsc::channel(8);
    let (_control, control_rx) = watch::channel(broker::Control::default());
    let (shutdown, shutdown_rx) = watch::channel(false);
    let store = broker.apps[&app()].store.clone();
    let actor = tokio::spawn(broker::run(broker, receiver, control_rx, shutdown_rx));
    let (mut client, server) = tokio::io::duplex(8192);
    let task = tokio::spawn(ipc::serve(
        server,
        commands,
        Budget::new(MAX_FRAME),
        Budget::new(8 * MAX_FRAME),
    ));
    wire::write(&mut client, &request(payload(2)))
        .await
        .unwrap();
    let response: Response = wire::read(&mut client, &Budget::new(MAX_FRAME))
        .await
        .unwrap()
        .value;
    task.await.unwrap().unwrap();
    assert!(matches!(response, Response::StreamClosed));
    shutdown.send_replace(true);
    actor.await.unwrap().unwrap();

    // Peers received the message and the marker, each exactly once.
    let mut replicated = Vec::new();
    while let Ok(packet) = outgoing.try_recv() {
        let Frame::Publication {
            publication: ChannelPublication::Event(event),
            ..
        } = serde_json::from_slice(&packet.bytes).unwrap()
        else {
            panic!("unexpected frame");
        };
        replicated.push(event.payload.r#type);
    }
    assert_eq!(
        replicated,
        vec!["update", tako_channels::CLOSE_MESSAGE_TYPE]
    );
    // The broker fenced replay when it stopped; the history itself is intact.
    let retained = store.read_after("events", None, 10).unwrap();
    assert_eq!(
        retained
            .iter()
            .map(|message| (message.event_id.clone(), message.r#type.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (sent.event_id, "update"),
            (marker.event_id, tako_channels::CLOSE_MESSAGE_TYPE)
        ]
    );
}
#[tokio::test]
async fn unknown_remote_app_never_creates_database() {
    let (temp, mut broker) = new_broker().await;
    broker
        .remote(
            &remote_id(),
            Frame::Publication {
                app: "unknown/production".parse().unwrap(),
                publication: ChannelPublication::Event(ChannelEvent {
                    id: ChannelEventId {
                        origin: "remote".into(),
                        sequence: 1,
                    },
                    channel: "events".into(),
                    payload: payload(1),
                    ephemeral: false,
                    lifecycle: None,
                    created_at_unix_ms: None,
                }),
            },
        )
        .await
        .unwrap();
    assert!(!temp.path().join("unknown").exists());
    assert_eq!(broker.apps.len(), 1);
}
#[tokio::test]
async fn queue_overflow_commits_successfully_and_persists_gap_without_subscribers() {
    let (_temp, mut broker) = new_broker().await;
    let first = publish(&mut broker, 1).await;
    let (_, _, _, _, mb) = identities();
    let (sender, _blocked_receiver) = wire::queue(Budget::new(1));
    broker.peers.insert(mb.device_id, sender);
    let second = publish(&mut broker, 2).await;
    stale(&broker.apps[&app()].store, &first);
    stale(&broker.apps[&app()].store, &second);
    assert_eq!(
        first.event_id.as_ref().unwrap().origin,
        second.event_id.as_ref().unwrap().origin
    );
    assert!(broker.peers.is_empty());
    assert_eq!(
        broker.apps[&app()]
            .store
            .read_after("events", None, 10)
            .unwrap()
            .len(),
        2
    );
}
#[tokio::test]
async fn first_registration_after_restart_invalidates_offline_cursor() {
    let (temp, mut broker) = new_broker().await;
    let before = publish(&mut broker, 1).await;
    drop(broker);
    let mut restarted = Broker::new(temp.path().into(), Budget::new(MAX_FRAME), local_id());
    restarted
        .local(Request::Register {
            app: app(),
            postgres_url: None,
        })
        .await
        .unwrap();
    stale(&restarted.apps[&app()].store, &before);
    let after = publish(&mut restarted, 2).await;
    assert_eq!(
        before.event_id.unwrap().origin,
        after.event_id.unwrap().origin
    );
}
#[tokio::test]
async fn control_disconnect_fences_persistently_and_rejects_old_generation() {
    let (_temp, mut broker) = new_broker().await;
    let before = publish(&mut broker, 1).await;
    let store = broker.apps[&app()].store.clone();
    let mut resets = store.resets("events");
    let (commands, receiver) = mpsc::channel(16);
    let (control, control_rx) = watch::channel(broker::Control::default());
    let (shutdown, shutdown_rx) = watch::channel(false);
    let actor = tokio::spawn(broker::run(broker, receiver, control_rx, shutdown_rx));
    control.send_replace(broker::Control {
        generation: 1,
        losses: BTreeMap::new(),
    });
    tokio::time::timeout(Duration::from_secs(3), resets.changed())
        .await
        .unwrap()
        .unwrap();
    stale(&store, &before);
    let (_, _, _, _, mb) = identities();
    let (sender, _rx) = wire::queue(Budget::new(MAX_FRAME));
    let (reply, answer) = oneshot::channel();
    commands
        .send(Command::Attach {
            peer: mb.device_id,
            generation: 0,
            sender,
            reply,
        })
        .await
        .unwrap();
    assert!(!answer.await.unwrap());
    shutdown.send_replace(true);
    actor.await.unwrap().unwrap();
}
#[tokio::test]
async fn bounded_framing_and_global_queue_budget_release() {
    let (mut tx, mut rx) = tokio::io::duplex(64);
    tx.write_u32((MAX_FRAME + 1) as u32).await.unwrap();
    assert!(
        wire::read::<_, Frame>(&mut rx, &Budget::new(MAX_FRAME))
            .await
            .is_err()
    );
    let budget = Budget::new(100);
    let (a, mut ar) = wire::queue(budget.clone());
    let (b, mut br) = wire::queue(budget);
    a.send(&"x".repeat(60)).unwrap();
    assert!(b.send(&"x".repeat(60)).is_err());
    drop(ar.recv().await.unwrap());
    b.send(&"x".repeat(60)).unwrap();
    drop(br.recv().await.unwrap());
    let (mut tx, mut rx) = tokio::io::duplex(64);
    tx.write_u32(10).await.unwrap();
    tx.write_all(b"{").await.unwrap();
    drop(tx);
    assert!(
        wire::read::<_, Frame>(&mut rx, &Budget::new(64))
            .await
            .is_err()
    );
}
#[tokio::test]
async fn pump_preserves_partial_inbound_frame_while_sending_keepalives() {
    let (transport, mut remote) = tokio::io::duplex(32 * 1024);
    let (_, _, _, _, mb) = identities();
    let (commands, mut receive) = mpsc::channel(4);
    let (_sender, outgoing) = wire::queue(Budget::new(MAX_FRAME));
    let task = tokio::spawn(peer::pump(
        transport,
        mb.device_id,
        0,
        commands,
        Budget::new(MAX_FRAME),
        outgoing,
    ));
    let bytes = wire::encode(&Frame::Gap { app: app() }).unwrap();
    remote.write_u32(bytes.len() as u32).await.unwrap();
    remote.write_all(&bytes[..2]).await.unwrap();
    let _: Frame = wire::read(&mut remote, &Budget::new(MAX_FRAME))
        .await
        .unwrap()
        .value;
    remote.write_all(&bytes[2..]).await.unwrap();
    let Command::Remote { frame, .. } = receive.recv().await.unwrap() else {
        panic!()
    };
    assert!(matches!(frame.value, Frame::Gap { .. }));
    drop(remote);
    assert!(task.await.unwrap().is_err());
}

#[tokio::test]
async fn postgres_wake_seam_never_reinserts_or_echoes_and_commit_dispatches_only_hint() {
    let (_temp, mut broker) = new_broker().await;
    broker.postgres_seam(&app());
    let store = broker.apps[&app()].store.clone();
    let changes = store.changes("events");
    let mut publications = store.publications();
    let (_, _, _, _, mb) = identities();
    let (sender, mut outgoing) = wire::queue(Budget::new(MAX_FRAME));
    broker.peers.insert(mb.device_id, sender);
    let Reply::Subscription(mut notifications) = broker
        .local(Request::Subscribe { app: app() })
        .await
        .unwrap()
    else {
        panic!()
    };
    broker
        .remote(
            &remote_id(),
            Frame::Publication {
                app: app(),
                publication: ChannelPublication::Wake {
                    channel: "events".into(),
                },
            },
        )
        .await
        .unwrap();
    assert!(changes.has_changed().unwrap());
    let notification = notifications.recv().await.unwrap();
    assert!(matches!(
        serde_json::from_slice::<Notice>(&notification.bytes).unwrap(),
        Notice::Notification {
            notification: Notification::Wake { .. }
        }
    ));
    broker
        .remote(
            &remote_id(),
            Frame::Publication {
                app: app(),
                publication: ChannelPublication::Event(ChannelEvent {
                    id: ChannelEventId {
                        origin: "remote".into(),
                        sequence: 1,
                    },
                    channel: "events".into(),
                    payload: payload(1),
                    ephemeral: false,
                    lifecycle: None,
                    created_at_unix_ms: None,
                }),
            },
        )
        .await
        .unwrap();
    assert!(store.read_after("events", None, 100).unwrap().is_empty());
    assert!(publications.try_recv().is_err());
    assert!(notifications.try_recv().is_err());
    assert!(outgoing.try_recv().is_err());
    let committed = ChannelMessage {
        id: "10".into(),
        event_id: None,
        channel: "events".into(),
        r#type: "update".into(),
        data: serde_json::json!(42),
    };
    broker
        .committed(
            &app(),
            &committed,
            false,
            Some(ChannelPublication::Wake {
                channel: "events".into(),
            }),
        )
        .await
        .unwrap();
    let packet = outgoing.recv().await.unwrap();
    assert!(matches!(
        serde_json::from_slice::<Frame>(&packet.bytes).unwrap(),
        Frame::Publication {
            publication: ChannelPublication::Wake { .. },
            ..
        }
    ));
    assert!(!String::from_utf8_lossy(&packet.bytes).contains("test credential"));
    assert!(store.read_after("events", None, 100).unwrap().is_empty());
}

#[tokio::test]
async fn transport_frame_overflow_is_gap_after_successful_commit() {
    let (_temp, mut broker) = new_broker().await;
    let before = publish(&mut broker, 1).await;
    let reply = broker
        .local(Request::Publish {
            app: app(),
            channel: "events".into(),
            payload: ChannelPublishPayload {
                r#type: "update".into(),
                data: serde_json::json!("x".repeat(MAX_FRAME)),
            },
            ephemeral: false,
            lifecycle: None,
        })
        .await
        .unwrap();
    assert!(matches!(reply, Reply::Response(Response::Published { .. })));
    stale(&broker.apps[&app()].store, &before);
    assert_eq!(
        broker.apps[&app()]
            .store
            .read_after("events", None, 10)
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn sync_auth_lifecycle_serializes_without_loss_and_rejects_path_traversal() {
    for ephemeral in [false, true] {
        let auth: tako_channels::ChannelAuthResponse = serde_json::from_value(
            serde_json::json!({"ok":true, "ephemeral":ephemeral, "keepaliveIntervalMs":123}),
        )
        .unwrap();
        let bytes = wire::encode(&Request::SyncChannel {
            app: app(),
            channel: "events".into(),
            auth: auth.clone(),
        })
        .unwrap();
        let Request::SyncChannel {
            auth: roundtrip, ..
        } = serde_json::from_slice(&bytes).unwrap()
        else {
            panic!()
        };
        assert_eq!(auth, roundtrip);
    }
    for scope in [
        "../production",
        "chat/../../etc",
        "chat/production/more",
        "/tmp/db",
    ] {
        assert!(scope.parse::<AppEnv>().is_err());
    }
}

#[tokio::test]
async fn expired_handshake_cannot_attach_a_ghost_peer() {
    let (_temp, mut broker) = new_broker().await;
    let (_, _, _, _, member) = identities();
    let (sender, receiver) = wire::queue(Budget::new(MAX_FRAME));
    drop(receiver);
    let (reply, answer) = oneshot::channel();
    broker
        .handle(Command::Attach {
            peer: member.device_id.clone(),
            generation: 0,
            sender,
            reply,
        })
        .await
        .unwrap();
    assert!(!answer.await.unwrap());
    let (sender, _receiver) = wire::queue(Budget::new(MAX_FRAME));
    let (reply, answer) = oneshot::channel();
    drop(answer);
    broker
        .handle(Command::Attach {
            peer: member.device_id,
            generation: 0,
            sender,
            reply,
        })
        .await
        .unwrap();
    assert!(broker.peers.is_empty());
}

#[tokio::test]
async fn rejects_reverse_canonical_connection_direction() {
    let (net, a, b, ma, mb) = identities();
    let ra = BTreeMap::from([(mb.device_id.clone(), mb.clone())]);
    let rb = BTreeMap::from([(ma.device_id.clone(), ma.clone())]);
    let (mut x, mut y) = tokio::io::duplex(32 * 1024);
    let (left, right) = tokio::join!(
        peer::authenticate(
            &mut x,
            &net,
            &ma.device_id,
            &a,
            &ra,
            net.address(&mb.request.wg_public_key).into(),
            None
        ),
        peer::authenticate(
            &mut y,
            &net,
            &mb.device_id,
            &b,
            &rb,
            net.address(&ma.request.wg_public_key).into(),
            Some(&ma.device_id)
        ),
    );
    assert!(left.is_err());
    assert!(right.is_err());
}

#[tokio::test]
async fn peer_loss_control_fences_offline_replay_without_membership_change() {
    let (_temp, mut broker) = new_broker().await;
    let before = publish(&mut broker, 1).await;
    let store = broker.apps[&app()].store.clone();
    let mut resets = store.resets("events");
    let (_, _, _, _, mb) = identities();
    let (sender, _outgoing) = wire::queue(Budget::new(MAX_FRAME));
    broker.peers.insert(mb.device_id.clone(), sender);
    let (_commands, receiver) = mpsc::channel(16);
    let (control, control_rx) = watch::channel(broker::Control::default());
    let (shutdown, shutdown_rx) = watch::channel(false);
    let actor = tokio::spawn(broker::run(broker, receiver, control_rx, shutdown_rx));
    control.send_replace(broker::Control {
        generation: 0,
        losses: BTreeMap::from([(mb.device_id, 1)]),
    });
    tokio::time::timeout(Duration::from_secs(3), resets.changed())
        .await
        .unwrap()
        .unwrap();
    stale(&store, &before);
    shutdown.send_replace(true);
    actor.await.unwrap().unwrap();
}

#[tokio::test]
async fn local_subscribers_receive_internal_gap_after_mesh_queue_loss() {
    let (_temp, mut broker) = new_broker().await;
    let Reply::Subscription(mut notices) = broker
        .local(Request::Subscribe { app: app() })
        .await
        .unwrap()
    else {
        panic!()
    };
    let (_, _, _, _, mb) = identities();
    let (sender, _blocked_receiver) = wire::queue(Budget::new(1));
    broker.peers.insert(mb.device_id, sender);
    let message = publish(&mut broker, 7).await;
    let committed = notices.recv().await.unwrap();
    assert!(
        matches!(serde_json::from_slice::<Notice>(&committed.bytes).unwrap(), Notice::Notification { notification: Notification::Committed { message: m, .. } } if m == message)
    );
    let gap = notices.recv().await.unwrap();
    assert!(matches!(
        serde_json::from_slice::<Notice>(&gap.bytes).unwrap(),
        Notice::Notification {
            notification: Notification::Gap
        }
    ));
    assert!(notices.try_recv().is_err());
}

#[tokio::test]
async fn stale_socket_cleanup_refuses_regular_files_and_symlink_locks() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("channels.sock");
    std::fs::write(&socket, b"not a socket").unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(ipc::bind(temp.path()).await.is_err());
    assert_eq!(std::fs::read(&socket).unwrap(), b"not a socket");
    std::fs::remove_file(temp.path().join("channels.lock")).unwrap();
    symlink(&socket, temp.path().join("channels.lock")).unwrap();
    assert!(ipc::bind(temp.path()).await.is_err());
    assert_eq!(std::fs::read(&socket).unwrap(), b"not a socket");
}

fn presence_member() -> tako_channels::PresenceMember {
    tako_channels::PresenceMember {
        connection_id: "ab".repeat(16),
        subject: Some("verified-user".into()),
        data: serde_json::json!({"typing":true}),
    }
}
async fn presence_members(broker: &mut Broker) -> Vec<tako_channels::PresenceMember> {
    let Reply::Response(Response::Presence { members }) = broker
        .local(Request::Presence {
            app: app(),
            channel: "room".into(),
        })
        .await
        .unwrap()
    else {
        panic!("presence rejected")
    };
    members
}
#[tokio::test]
async fn presence_updates_and_leaves_cross_peers_once_with_origin_bound_identity() {
    let (_left_dir, mut left) = new_broker().await;
    let right_dir = tempfile::tempdir().unwrap();
    let mut right = Broker::new(right_dir.path().into(), Budget::new(MAX_FRAME), remote_id());
    right
        .local(Request::Register {
            app: app(),
            postgres_url: None,
        })
        .await
        .unwrap();
    let (send, mut frames) = wire::queue(Budget::new(MAX_FRAME));
    left.peers.insert(remote_id(), send);
    let (send, mut echoes) = wire::queue(Budget::new(MAX_FRAME));
    right.peers.insert(local_id(), send);
    let member = presence_member();
    let Reply::Response(Response::Ok) = left
        .local(Request::UpdatePresence {
            app: app(),
            channel: "room".into(),
            member: member.clone(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(presence_members(&mut left).await, vec![member.clone()]);
    let frame: Frame = serde_json::from_slice(&frames.recv().await.unwrap().bytes).unwrap();
    right.remote(&local_id(), frame.clone()).await.unwrap();
    right.remote(&local_id(), frame).await.unwrap();
    let rows = presence_members(&mut right).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].connection_id,
        format!("{}:{}", local_id(), member.connection_id)
    );
    let probe: Frame = serde_json::from_slice(&echoes.try_recv().unwrap().bytes).unwrap();
    assert!(matches!(
        probe,
        Frame::Presence {
            publication: super::presence::Publication::Sync,
            ..
        }
    ));
    assert!(echoes.try_recv().is_err());
    let probe: Frame = serde_json::from_slice(&frames.try_recv().unwrap().bytes).unwrap();
    assert!(matches!(
        probe,
        Frame::Presence {
            publication: super::presence::Publication::Sync,
            ..
        }
    ));
    assert!(
        left.apps[&app()]
            .store
            .presence("room", 0)
            .unwrap()
            .is_empty()
    );
    assert!(
        right.apps[&app()]
            .store
            .presence("room", 0)
            .unwrap()
            .is_empty()
    );
    left.local(Request::RemovePresence {
        app: app(),
        channel: "room".into(),
        connection_id: member.connection_id,
    })
    .await
    .unwrap();
    let frame: Frame = serde_json::from_slice(&frames.recv().await.unwrap().bytes).unwrap();
    right.remote(&local_id(), frame).await.unwrap();
    assert!(presence_members(&mut left).await.is_empty());
    assert!(presence_members(&mut right).await.is_empty());
}
#[tokio::test]
async fn presence_rejects_foreign_identity_and_drops_peer_leases_on_gap() {
    let (_temp, mut broker) = new_broker().await;
    broker
        .local(Request::UpdatePresence {
            app: app(),
            channel: "room".into(),
            member: presence_member(),
        })
        .await
        .unwrap();
    let peer = remote_id();
    let member = super::presence::local_member(&peer, "room", &presence_member()).unwrap();
    let expires_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
        + 45_000;
    let frame = Frame::Presence {
        app: app(),
        publication: super::presence::Publication::Update {
            channel: "room".into(),
            member,
            expires_at_ms,
            sequence: 1,
        },
    };
    assert!(broker.remote(&local_id(), frame.clone()).await.is_err());
    broker.remote(&peer, frame).await.unwrap();
    assert_eq!(presence_members(&mut broker).await.len(), 2);
    broker.fence().await.unwrap();
    assert_eq!(presence_members(&mut broker).await, vec![presence_member()]);
}
#[tokio::test]
async fn postgres_presence_seam_uses_shared_table_and_does_not_forward_or_reinsert() {
    let (_temp, mut broker) = new_broker().await;
    broker.postgres_seam(&app());
    let (sender, mut outgoing) = wire::queue(Budget::new(MAX_FRAME));
    broker.peers.insert(remote_id(), sender);
    broker
        .local(Request::UpdatePresence {
            app: app(),
            channel: "room".into(),
            member: presence_member(),
        })
        .await
        .unwrap();
    assert_eq!(presence_members(&mut broker).await, vec![presence_member()]);
    assert_eq!(
        broker.apps[&app()].store.presence("room", 0).unwrap().len(),
        1
    );
    assert!(outgoing.try_recv().is_err());
    let remote_member =
        super::presence::local_member(&remote_id(), "room", &presence_member()).unwrap();
    broker
        .remote(
            &remote_id(),
            Frame::Presence {
                app: app(),
                publication: super::presence::Publication::Update {
                    channel: "room".into(),
                    member: remote_member,
                    expires_at_ms: i64::MAX,
                    sequence: 1,
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(
        broker.apps[&app()].store.presence("room", 0).unwrap().len(),
        1
    );
    broker
        .local(Request::RemovePresence {
            app: app(),
            channel: "room".into(),
            connection_id: presence_member().connection_id,
        })
        .await
        .unwrap();
    assert!(presence_members(&mut broker).await.is_empty());
    assert!(outgoing.try_recv().is_err());
}
#[tokio::test]
async fn presence_ipc_roundtrips_over_real_duplex() {
    let (_temp, broker) = new_broker().await;
    let (commands, receiver) = mpsc::channel(8);
    let (_control, control_rx) = watch::channel(broker::Control::default());
    let (shutdown, shutdown_rx) = watch::channel(false);
    let actor = tokio::spawn(broker::run(broker, receiver, control_rx, shutdown_rx));
    for request in [
        Request::UpdatePresence {
            app: app(),
            channel: "room".into(),
            member: presence_member(),
        },
        Request::Presence {
            app: app(),
            channel: "room".into(),
        },
    ] {
        let (mut client, server) = tokio::io::duplex(8192);
        let task = tokio::spawn(ipc::serve(
            server,
            commands.clone(),
            Budget::new(MAX_FRAME),
            Budget::new(8 * MAX_FRAME),
        ));
        let querying = matches!(request, Request::Presence { .. });
        wire::write(&mut client, &request).await.unwrap();
        let response: Response = wire::read(&mut client, &Budget::new(MAX_FRAME))
            .await
            .unwrap()
            .value;
        if querying {
            assert!(
                matches!(response, Response::Presence { members } if members == vec![presence_member()])
            );
        } else {
            assert!(matches!(response, Response::Ok));
        }
        task.await.unwrap().unwrap();
    }
    shutdown.send_replace(true);
    actor.await.unwrap().unwrap();
}

#[tokio::test]
async fn first_presence_query_discovers_existing_peer_leases_without_client_renewal() {
    let (_left_dir, mut left) = new_broker().await;
    left.local(Request::UpdatePresence {
        app: app(),
        channel: "room".into(),
        member: presence_member(),
    })
    .await
    .unwrap();
    let right_dir = tempfile::tempdir().unwrap();
    let mut right = Broker::new(right_dir.path().into(), Budget::new(MAX_FRAME), remote_id());
    right
        .local(Request::Register {
            app: app(),
            postgres_url: None,
        })
        .await
        .unwrap();
    let (left_sender, mut left_out) = wire::queue(Budget::new(MAX_FRAME));
    let (right_sender, mut right_out) = wire::queue(Budget::new(MAX_FRAME));
    left.peers.insert(remote_id(), left_sender);
    right.peers.insert(local_id(), right_sender);
    assert!(presence_members(&mut right).await.is_empty());
    let query: Frame = serde_json::from_slice(&right_out.recv().await.unwrap().bytes).unwrap();
    left.remote(&remote_id(), query).await.unwrap();
    let snapshot: Frame = serde_json::from_slice(&left_out.recv().await.unwrap().bytes).unwrap();
    right.remote(&local_id(), snapshot).await.unwrap();
    assert_eq!(presence_members(&mut right).await.len(), 1);
    assert!(right_out.try_recv().is_err());
}

#[tokio::test]
async fn disconnected_or_expired_ipc_cannot_commit_queued_mutations() {
    let (_temp, mut broker) = new_broker().await;
    let store = broker.apps[&app()].store.clone();
    for expired in [false, true] {
        let (mut client, server) = tokio::io::duplex(8192);
        let (commands, mut receiver) = mpsc::channel(1);
        let budget = Budget::new(MAX_FRAME);
        let replies = Budget::new(MAX_FRAME);
        let task = tokio::spawn(ipc::serve(
            server,
            commands,
            budget.clone(),
            replies.clone(),
        ));
        wire::write(
            &mut client,
            &Request::Publish {
                app: app(),
                channel: "events".into(),
                payload: payload(1),
                ephemeral: false,
                lifecycle: None,
            },
        )
        .await
        .unwrap();
        let mut command = receiver.recv().await.unwrap();
        if expired {
            let Command::Local { deadline, .. } = &mut command else {
                panic!("local")
            };
            *deadline = tokio::time::Instant::now();
        } else {
            drop(client);
            assert!(task.await.unwrap().is_err());
            broker.handle(command).await.unwrap();
            assert!(budget.take(MAX_FRAME).is_ok());
            assert!(replies.take(MAX_FRAME).is_ok());
            continue;
        }
        broker.handle(command).await.unwrap();
        assert!(task.await.unwrap().is_err());
        assert!(budget.take(MAX_FRAME).is_ok());
        assert!(replies.take(MAX_FRAME).is_ok());
    }
    assert!(store.read_after("events", None, 10).unwrap().is_empty());
}

#[tokio::test]
async fn ipc_reply_budget_survives_blocked_write_and_releases_on_disconnect() {
    let (mut client, server) = tokio::io::duplex(256);
    let (commands, mut receiver) = mpsc::channel(2);
    let replies = Budget::new(MAX_FRAME);
    let task = tokio::spawn(ipc::serve(
        server,
        commands.clone(),
        Budget::new(MAX_FRAME),
        replies.clone(),
    ));
    wire::write(
        &mut client,
        &Request::Presence {
            app: app(),
            channel: "events".into(),
        },
    )
    .await
    .unwrap();
    let Command::Local {
        reply, admission, ..
    } = receiver.recv().await.unwrap()
    else {
        panic!("local")
    };
    let mut member = presence_member();
    member.data = serde_json::json!("x".repeat(8192));
    assert!(
        reply
            .send((
                Reply::Response(Response::Presence {
                    members: vec![member]
                }),
                admission
            ))
            .is_ok()
    );
    // The response is larger than the duplex capacity. Its reservation remains owned by IPC.
    assert!(replies.take(1).is_err());
    let (mut second, server) = tokio::io::duplex(256);
    let second_task = tokio::spawn(ipc::serve(
        server,
        commands,
        Budget::new(MAX_FRAME),
        replies.clone(),
    ));
    wire::write(
        &mut second,
        &Request::Presence {
            app: app(),
            channel: "events".into(),
        },
    )
    .await
    .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), receiver.recv())
            .await
            .is_err()
    );
    drop(client);
    assert!(task.await.unwrap().is_err());
    let next = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    drop(second);
    assert!(second_task.await.unwrap().is_err());
    drop(next);
    assert!(replies.take(MAX_FRAME).is_ok());
}

#[tokio::test]
async fn backend_registration_change_rejects_without_replacing_writer_and_same_config_recovers() {
    let (_temp, mut broker) = new_broker().await;
    let old = broker.apps[&app()].store.clone();
    let first = publish(&mut broker, 1).await;
    assert!(matches!(
        broker
            .local(Request::Register {
                app: app(),
                postgres_url: Some(zeroize::Zeroizing::new("postgres://secret".into())),
            })
            .await
            .unwrap(),
        Reply::Response(Response::Rejected)
    ));
    assert!(Arc::ptr_eq(&old, &broker.apps[&app()].store));
    assert!(matches!(
        broker
            .local(Request::Register {
                app: app(),
                postgres_url: None
            })
            .await
            .unwrap(),
        Reply::Response(Response::Ok)
    ));
    publish(&mut broker, 2).await;
    assert_eq!(
        old.read_after("events", Some(first.id.parse().unwrap()), 10)
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn unauthenticated_peer_failure_cannot_attach_or_request_replay_fence() {
    let (net, a, _b, ma, mb) = identities();
    let rogue = LocalDeviceKeys::generate().unwrap();
    let roster = BTreeMap::from([(mb.device_id.clone(), mb.clone())]);
    let remote_roster = BTreeMap::from([(ma.device_id.clone(), ma.clone())]);
    let (server, mut remote) = tokio::io::duplex(8192);
    let (commands, mut receiver) = mpsc::channel(1);
    let (end, _) = tokio::join!(
        peer::serve_stream(
            server,
            &net,
            &ma.device_id,
            &a,
            &roster,
            IpAddr::V6(net.address(&mb.request.wg_public_key)),
            Some(&mb.device_id),
            0,
            commands,
            Budget::new(MAX_FRAME),
            Budget::new(MAX_FRAME)
        ),
        peer::authenticate(
            &mut remote,
            &net,
            &mb.device_id,
            &rogue,
            &remote_roster,
            IpAddr::V6(net.address(&ma.request.wg_public_key)),
            None
        ),
    );
    assert!(!end.authenticated);
    assert!(end.result.is_err());
    assert!(receiver.try_recv().is_err());
}

#[tokio::test]
async fn authenticated_peer_loss_requests_fence_even_when_admission_fails() {
    let (net, a, b, ma, mb) = identities();
    let roster = BTreeMap::from([(mb.device_id.clone(), mb.clone())]);
    let remote_roster = BTreeMap::from([(ma.device_id.clone(), ma.clone())]);
    let (server, mut remote) = tokio::io::duplex(8192);
    let (commands, receiver) = mpsc::channel(1);
    drop(receiver);
    let (end, authenticated) = tokio::join!(
        peer::serve_stream(
            server,
            &net,
            &ma.device_id,
            &a,
            &roster,
            IpAddr::V6(net.address(&mb.request.wg_public_key)),
            Some(&mb.device_id),
            0,
            commands,
            Budget::new(MAX_FRAME),
            Budget::new(MAX_FRAME)
        ),
        peer::authenticate(
            &mut remote,
            &net,
            &mb.device_id,
            &b,
            &remote_roster,
            IpAddr::V6(net.address(&ma.request.wg_public_key)),
            None
        ),
    );
    assert!(authenticated.is_ok());
    assert!(end.authenticated);
    assert!(end.result.is_err());
}

#[tokio::test]
async fn re_registration_with_a_changed_backend_replaces_the_store_and_signals_a_gap() {
    let (_temp, mut broker) = new_broker().await;
    let before = publish(&mut broker, 1).await;
    let previous = broker.apps[&app()].store.clone();
    let Reply::Subscription(mut notices) = broker
        .local(Request::Subscribe { app: app() })
        .await
        .unwrap()
    else {
        panic!()
    };
    broker.postgres_seam(&app());
    assert!(matches!(
        broker
            .local(Request::Register {
                app: app(),
                postgres_url: None
            })
            .await
            .unwrap(),
        Reply::Response(Response::Ok)
    ));
    let application = &broker.apps[&app()];
    assert!(!Arc::ptr_eq(&previous, &application.store));
    assert!(application.postgres_url.is_none());
    assert_eq!(application.subscribers.len(), 1);
    let packet = notices.recv().await.unwrap();
    assert!(matches!(
        serde_json::from_slice::<Notice>(&packet.bytes).unwrap(),
        Notice::Notification {
            notification: Notification::Gap
        }
    ));
    stale(&broker.apps[&app()].store, &before);
    let after = publish(&mut broker, 2).await;
    let rows = broker.apps[&app()]
        .store
        .read_after("events", None, 10)
        .unwrap();
    assert_eq!(rows.last().map(|row| row.id.clone()), Some(after.id));
}

#[tokio::test]
async fn unresponsive_backend_registration_ends_at_the_deadline_without_stalling_other_apps() {
    let (_temp, mut broker) = new_broker().await;
    // A server that accepts the connection and then never answers the startup
    // message, released shortly after the broker's deadline so the worker exits.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        drop(socket);
    });
    let stalled: AppEnv = "stalled/production".parse().unwrap();
    let started = tokio::time::Instant::now();
    let reply = broker
        .local_until(
            Request::Register {
                app: stalled.clone(),
                postgres_url: Some(zeroize::Zeroizing::new(format!(
                    "postgres://tako:secret@127.0.0.1:{port}/tako"
                ))),
            },
            started + Duration::from_millis(300),
        )
        .await
        .unwrap();
    assert!(matches!(reply, Reply::Response(Response::Rejected)));
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(!broker.apps.contains_key(&stalled));
    // Other applications keep registering and publishing meanwhile.
    let other: AppEnv = "other/production".parse().unwrap();
    assert!(matches!(
        tokio::time::timeout(
            Duration::from_secs(2),
            broker.local(Request::Register {
                app: other.clone(),
                postgres_url: None
            })
        )
        .await
        .unwrap()
        .unwrap(),
        Reply::Response(Response::Ok)
    ));
    publish(&mut broker, 1).await;
    server.await.unwrap();
}

/// The request kinds a broker from the previous release understands. Like the
/// real one, it drops the connection without an answer on any other frame.
#[derive(serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum PreviousRequest {
    Register {
        #[allow(dead_code)]
        app: AppEnv,
        #[allow(dead_code)]
        postgres_url: Option<String>,
    },
    Publish {
        #[allow(dead_code)]
        app: AppEnv,
        channel: String,
        payload: ChannelPublishPayload,
        #[allow(dead_code)]
        ephemeral: bool,
    },
    SyncChannel {
        #[allow(dead_code)]
        app: AppEnv,
        #[allow(dead_code)]
        channel: String,
        auth: serde_json::Value,
    },
}
/// How a [`broker_stub`] behaves; tests may change it between requests.
#[derive(Default)]
struct StubControl {
    /// Parse the frame set of the previous release instead of the current one.
    previous: std::sync::atomic::AtomicBool,
    /// Connections still to drop after the length prefix, as a busy or
    /// stopping current broker does; recorded as `busy`.
    drops: std::sync::atomic::AtomicUsize,
}

/// A broker stub on a real socket. `previous` selects the frame set of the
/// previous release; otherwise it parses the current one. Each answered frame
/// is recorded, unparsable ones as `dropped`.
async fn broker_stub(
    previous: bool,
) -> (
    tempfile::TempDir,
    Client,
    Arc<parking_lot::Mutex<Vec<String>>>,
) {
    let control = StubControl::default();
    control
        .previous
        .store(previous, std::sync::atomic::Ordering::SeqCst);
    let (temp, client, seen, _) = controlled_broker_stub(control).await;
    (temp, client, seen)
}

async fn controlled_broker_stub(
    control: StubControl,
) -> (
    tempfile::TempDir,
    Client,
    Arc<parking_lot::Mutex<Vec<String>>>,
    Arc<StubControl>,
) {
    use std::sync::atomic::Ordering;
    use tokio::io::AsyncReadExt;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("channels.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let log = seen.clone();
    let control = Arc::new(control);
    let controls = control.clone();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let budget = Budget::new(MAX_FRAME);
            let previous = controls.previous.load(Ordering::SeqCst);
            if controls
                .drops
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
            {
                let _ = stream.read_u32().await;
                log.lock().push("busy".into());
                continue;
            }
            let answer = |channel: String, payload: ChannelPublishPayload| Response::Published {
                message: ChannelMessage {
                    id: "1".into(),
                    event_id: None,
                    channel,
                    r#type: payload.r#type,
                    data: payload.data,
                },
            };
            let response = if previous {
                match wire::read::<_, PreviousRequest>(&mut stream, &budget).await {
                    Ok(request) => match request.value {
                        PreviousRequest::Register { .. } => {
                            log.lock().push("register".into());
                            Response::Ok
                        }
                        PreviousRequest::SyncChannel { auth, .. } => {
                            log.lock().push(format!("sync {}", auth["replayWindowMs"]));
                            Response::Ok
                        }
                        PreviousRequest::Publish {
                            channel, payload, ..
                        } => {
                            log.lock().push("publish".into());
                            answer(channel, payload)
                        }
                    },
                    Err(_) => {
                        log.lock().push("dropped".into());
                        continue;
                    }
                }
            } else {
                match wire::read::<_, Request>(&mut stream, &budget).await {
                    Ok(request) => match request.value {
                        Request::Features => {
                            log.lock().push("features".into());
                            Response::Features {
                                features: vec![ipc::PUBLISH_LIFECYCLE.into()],
                            }
                        }
                        Request::SyncChannel { .. } => {
                            log.lock().push("sync".into());
                            Response::Ok
                        }
                        Request::Publish {
                            channel,
                            payload,
                            lifecycle,
                            ..
                        } => {
                            log.lock().push(match lifecycle {
                                Some(lifecycle) => {
                                    format!("publish {}", lifecycle.replay_window_ms)
                                }
                                None => "publish".into(),
                            });
                            answer(channel, payload)
                        }
                        _ => Response::Rejected,
                    },
                    Err(_) => {
                        log.lock().push("dropped".into());
                        continue;
                    }
                }
            };
            let _ = wire::write(&mut stream, &response).await;
        }
    });
    (temp, Client::new(path), seen, control)
}
fn long_lifecycle() -> tako_channels::ChannelLifecycle {
    serde_json::from_value(serde_json::json!({ "replayWindowMs": 86_400_000 })).unwrap()
}

#[tokio::test]
async fn a_lifecycle_publish_falls_back_to_sync_then_publish_on_a_previous_broker() {
    let (_temp, client, seen) = broker_stub(true).await;
    for n in 0..2 {
        let message = client
            .publish(
                app(),
                "events".into(),
                payload(n),
                false,
                Some(long_lifecycle()),
            )
            .await
            .unwrap();
        assert_eq!(message.data, serde_json::json!(n));
    }
    // The probe goes unanswered twice, so the broker is taken to be the
    // previous one; afterwards each publish syncs the lifecycle
    // first, as publishers did before the lifecycle travelled with the append.
    assert_eq!(
        *seen.lock(),
        vec![
            "dropped",
            "dropped",
            "sync 86400000",
            "publish",
            "sync 86400000",
            "publish"
        ]
    );
    // Publishes without a lifecycle never need the probe.
    let (_temp, client, seen) = broker_stub(true).await;
    client
        .publish(app(), "events".into(), payload(1), false, None)
        .await
        .unwrap();
    assert_eq!(*seen.lock(), vec!["publish"]);
}

#[tokio::test]
async fn a_lifecycle_publish_travels_with_the_append_on_a_current_broker() {
    let (_temp, client, seen) = broker_stub(false).await;
    for n in 0..2 {
        client
            .publish(
                app(),
                "events".into(),
                payload(n),
                false,
                Some(long_lifecycle()),
            )
            .await
            .unwrap();
    }
    assert_eq!(
        *seen.lock(),
        vec!["features", "publish 86400000", "publish 86400000"]
    );
}

#[tokio::test]
async fn a_probe_dropped_once_by_a_busy_current_broker_is_asked_again() {
    let control = StubControl::default();
    control.drops.store(1, std::sync::atomic::Ordering::SeqCst);
    let (_temp, client, seen, _) = controlled_broker_stub(control).await;
    for n in 0..2 {
        client
            .publish(
                app(),
                "events".into(),
                payload(n),
                false,
                Some(long_lifecycle()),
            )
            .await
            .unwrap();
    }
    // One dropped connection is not an answer: the second probe finds the
    // current broker, and the lifecycle travels with every append.
    assert_eq!(
        *seen.lock(),
        vec!["busy", "features", "publish 86400000", "publish 86400000"]
    );
}

#[tokio::test]
async fn a_previous_broker_answer_is_rechecked_after_a_minute() {
    let control = StubControl::default();
    control
        .previous
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (_temp, client, seen, control) = controlled_broker_stub(control).await;
    let publish = |n| {
        client.publish(
            app(),
            "events".into(),
            payload(n),
            false,
            Some(long_lifecycle()),
        )
    };
    publish(0).await.unwrap();
    // The network agent restarts with the current broker.
    control
        .previous
        .store(false, std::sync::atomic::Ordering::SeqCst);
    // Within the minute the cached answer holds and no probe is sent.
    client.age_feature_probe(Duration::from_secs(59));
    publish(1).await.unwrap();
    // Past it, the next publish asks again and finds the current broker.
    client.age_feature_probe(Duration::from_secs(2));
    publish(2).await.unwrap();
    assert_eq!(
        *seen.lock(),
        vec![
            "dropped",
            "dropped",
            "sync 86400000",
            "publish",
            "sync",
            "publish",
            "features",
            "publish 86400000"
        ]
    );
}

#[tokio::test]
async fn an_unreachable_broker_fails_the_publish_without_caching_an_answer() {
    let temp = tempfile::tempdir().unwrap();
    let client = Client::new(temp.path().join("channels.sock"));
    let error = client
        .publish(
            app(),
            "events".into(),
            payload(0),
            false,
            Some(long_lifecycle()),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            error.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
        ),
        "{error:?}"
    );
    assert_eq!(client.cached_publish_lifecycle(), None);
}

#[tokio::test]
async fn the_broker_answers_the_feature_probe_without_queueing_a_command() {
    let (commands, _receiver) = mpsc::channel(1);
    let (mut client, server) = tokio::io::duplex(8192);
    let task = tokio::spawn(ipc::serve(
        server,
        commands,
        Budget::new(MAX_FRAME),
        Budget::new(8 * MAX_FRAME),
    ));
    wire::write(&mut client, &Request::Features).await.unwrap();
    let response: Response = wire::read(&mut client, &Budget::new(MAX_FRAME))
        .await
        .unwrap()
        .value;
    assert!(matches!(
        response,
        Response::Features { features } if features == vec![ipc::PUBLISH_LIFECYCLE.to_owned()]
    ));
    task.await.unwrap().unwrap();
}

#[test]
fn stream_limit_drops_are_reported_at_most_once_a_minute_with_their_count() {
    let mut warning = broker::StreamLimitWarning::default();
    let start = tokio::time::Instant::now();
    assert_eq!(warning.dropped(start), Some(1));
    for second in 1..60 {
        assert_eq!(warning.dropped(start + Duration::from_secs(second)), None);
    }
    assert_eq!(warning.dropped(start + Duration::from_secs(60)), Some(60));
    assert_eq!(warning.dropped(start + Duration::from_secs(61)), None);
}
