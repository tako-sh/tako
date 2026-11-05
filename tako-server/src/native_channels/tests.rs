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
    let mut broker = Broker::new(temp.path().into(), Budget::new(8 * MAX_FRAME));
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
async fn publish(broker: &mut Broker, n: usize) -> ChannelMessage {
    match broker
        .local(Request::Publish {
            app: app(),
            channel: "events".into(),
            payload: payload(n),
            ephemeral: false,
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
            let task = tokio::spawn(ipc::serve(server, commands, Budget::new(MAX_FRAME)));
            wire::write(
                &mut client,
                &Request::Publish {
                    app: app(),
                    channel: "events".into(),
                    payload: payload(n),
                    ephemeral: n % 2 == 0,
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
        broker.remote(frame.clone()).await.unwrap();
        broker.remote(frame).await.unwrap();
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
async fn unknown_remote_app_never_creates_database() {
    let (temp, mut broker) = new_broker().await;
    broker
        .remote(Frame::Publication {
            app: "unknown/production".parse().unwrap(),
            publication: ChannelPublication::Event(ChannelEvent {
                id: ChannelEventId {
                    origin: "remote".into(),
                    sequence: 1,
                },
                channel: "events".into(),
                payload: payload(1),
                ephemeral: false,
            }),
        })
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
    let mut restarted = Broker::new(temp.path().into(), Budget::new(MAX_FRAME));
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
        .remote(Frame::Publication {
            app: app(),
            publication: ChannelPublication::Wake {
                channel: "events".into(),
            },
        })
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
        .remote(Frame::Publication {
            app: app(),
            publication: ChannelPublication::Event(ChannelEvent {
                id: ChannelEventId {
                    origin: "remote".into(),
                    sequence: 1,
                },
                channel: "events".into(),
                payload: payload(1),
                ephemeral: false,
            }),
        })
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
