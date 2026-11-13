use super::*;

async fn publish_live(broker: &mut Broker, n: usize) -> ChannelMessage {
    let Reply::Response(Response::Published { message }) = broker
        .local(Request::Publish {
            app: app(),
            channel: "live".into(),
            payload: payload(n),
            ephemeral: true,
            lifecycle: None,
        })
        .await
        .unwrap()
    else {
        panic!("live publish rejected")
    };
    message
}

#[tokio::test]
async fn three_servers_fan_out_without_local_subscribers_and_tolerate_missing_events() {
    let (_a, mut a) = new_broker().await;
    let (_b, mut b) = new_broker().await;
    let (_c, mut c) = new_broker().await;
    let before = publish(&mut b, 0).await;
    let cursor = before.id.parse().unwrap();
    let b_store = b.apps[&app()].store.clone();
    let mut on_b = b_store.subscribe_ephemeral("live").unwrap();
    let mut on_c = c.apps[&app()].store.subscribe_ephemeral("live").unwrap();
    let (to_b, mut for_b) = wire::queue(Budget::new(MAX_FRAME));
    let (to_c, mut for_c) = wire::queue(Budget::new(MAX_FRAME));
    a.peers.lock().insert(remote_id(), to_b);
    a.peers
        .lock()
        .insert("22".repeat(32).parse().unwrap(), to_c);
    let (back, mut echoes) = wire::queue(Budget::new(MAX_FRAME));
    b.peers.lock().insert(local_id(), back);

    for n in 1..=3 {
        let published = publish_live(&mut a, n).await;
        let for_b: Frame = serde_json::from_slice(&for_b.recv().await.unwrap().bytes).unwrap();
        let for_c: Frame = serde_json::from_slice(&for_c.recv().await.unwrap().bytes).unwrap();
        c.remote(&local_id(), for_c).await.unwrap();
        assert_eq!(on_c.recv().await.unwrap().event_id, published.event_id);
        if n == 2 {
            continue;
        } // B was disconnected for one publication; no catch-up.
        b.remote(&local_id(), for_b.clone()).await.unwrap();
        b.remote(&local_id(), for_b).await.unwrap(); // Duplicate delivery is suppressed.
        assert_eq!(on_b.recv().await.unwrap().data, n);
    }
    assert!(echoes.try_recv().is_err());
    assert_eq!(
        b_store.replay_cursor("events", Some(cursor)).unwrap(),
        Some(cursor)
    );
    assert!(b_store.read_after("live", None, 10).unwrap().is_empty());
    assert!(
        tokio::time::timeout(Duration::from_millis(20), on_b.recv())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn live_overflow_preserves_peers_and_capacity_for_durable_events_and_control() {
    let (_temp, mut broker) = new_broker().await;
    let before = publish(&mut broker, 0).await;
    let store = broker.apps[&app()].store.clone();
    let (sender, mut outgoing) = wire::queue(Budget::new(MAX_FRAME));
    broker.peers.lock().insert(remote_id(), sender);
    let Reply::Subscription(mut notices) = broker
        .local(Request::Subscribe { app: app() })
        .await
        .unwrap()
    else {
        panic!("subscription")
    };
    for n in 0..200 {
        publish_live(&mut broker, n).await;
    }
    assert_eq!(broker.peers.lock().len(), 1);
    assert_eq!(
        store
            .replay_cursor("events", before.id.parse().ok())
            .unwrap(),
        before.id.parse().ok()
    );
    let durable = publish(&mut broker, 1).await;
    let frame: Frame = serde_json::from_slice(&outgoing.recv().await.unwrap().bytes).unwrap();
    assert!(
        matches!(frame, Frame::Publication { publication: ChannelPublication::Event(event), .. } if !event.ephemeral)
    );
    let notice: Notice = serde_json::from_slice(&notices.recv().await.unwrap().bytes).unwrap();
    assert!(
        matches!(notice, Notice::Notification { notification: Notification::Committed { message, ephemeral: false } } if message.id == durable.id)
    );
    // The bounded slow queues retain only their admitted prefix, in order.
    let mut count = 0;
    while let Ok(packet) = outgoing.try_recv() {
        let frame: Frame = serde_json::from_slice(&packet.bytes).unwrap();
        let Frame::Publication {
            publication: ChannelPublication::Event(event),
            ..
        } = frame
        else {
            panic!("unexpected durable reset")
        };
        count += 1;
        assert!(event.ephemeral);
        assert_eq!(event.id.sequence, count);
    }
    assert_eq!(count, 64);
}

#[tokio::test]
async fn live_byte_budget_cannot_exhaust_reliable_budget() {
    let budget = Budget::new(32);
    let (sender, mut receiver) = wire::queue(budget);
    let bytes = wire::encode(&"123456789012345678901234567890").unwrap();
    sender.send_ephemeral_bytes(bytes.clone());
    sender.send_ephemeral_bytes(bytes.clone()); // Dropped; reliable budget remains reserved.
    sender.send_bytes(bytes).unwrap();
    assert!(receiver.recv().await.is_some());
    assert!(receiver.recv().await.is_some());
    assert!(receiver.try_recv().is_err());
}

#[tokio::test]
async fn full_live_inbound_queue_does_not_block_a_later_control_frame() {
    let source = ChannelStore::open_in_memory().unwrap();
    let mut events = source.ephemeral_publications();
    let (transport, mut remote) = tokio::io::duplex(32 * 1024);
    let (commands, mut receiver) = broker::commands(1);
    let (sender, outgoing) = wire::queue(Budget::new(MAX_FRAME));
    let task = tokio::spawn(peer::pump(
        transport,
        remote_id(),
        0,
        sender.session(),
        commands,
        Budget::new(MAX_FRAME),
        outgoing,
    ));
    for n in 0..3 {
        source.publish_ephemeral("live", &payload(n)).unwrap();
        wire::write(
            &mut remote,
            &Frame::Publication {
                app: app(),
                publication: ChannelPublication::Event(events.try_recv().unwrap()),
            },
        )
        .await
        .unwrap();
    }
    wire::write(&mut remote, &Frame::Gap { app: app() })
        .await
        .unwrap();
    // Do not consume live frames until the control message has been admitted.
    let command = tokio::time::timeout(Duration::from_secs(2), receiver.recv_durable())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(command, Command::Remote { frame, .. } if matches!(frame.value, Frame::Gap { .. }))
    );
    assert!(receiver.try_recv().is_ok());
    assert!(receiver.try_recv().is_err());
    drop(remote);
    assert!(task.await.unwrap().is_err());
}

async fn request(commands: broker::Commands, request: Request) -> Response {
    let (mut client, server) = tokio::io::duplex(8192);
    let task = tokio::spawn(ipc::serve(
        server,
        commands,
        Budget::new(MAX_FRAME),
        Budget::new(MAX_FRAME),
    ));
    wire::write(&mut client, &request).await.unwrap();
    let response = wire::read(&mut client, &Budget::new(MAX_FRAME))
        .await
        .unwrap()
        .value;
    task.await.unwrap().unwrap();
    response
}

#[tokio::test]
async fn live_ipc_publication_progresses_while_durable_storage_is_locked() {
    let (temp, broker) = new_broker().await;
    let mut db = rusqlite::Connection::open(
        temp.path()
            .join(app().as_str())
            .join("data/tako/channels.sqlite"),
    )
    .unwrap();
    let transaction = db
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    let (commands, mut receiver) = broker::commands(8);
    let durable = tokio::spawn(request(
        commands.clone(),
        Request::Publish {
            app: app(),
            channel: "events".into(),
            payload: payload(1),
            ephemeral: false,
            lifecycle: None,
        },
    ));
    // Put the durable request first, without depending on scheduling sleeps.
    let first = receiver.recv().await.unwrap();
    commands.send(first).await.unwrap();
    let (_control, control) = watch::channel(broker::Control::default());
    let (shutdown, stopping) = watch::channel(false);
    let actor = tokio::spawn(broker::run(broker, receiver, control, stopping));
    let live = tokio::time::timeout(
        Duration::from_secs(2),
        request(
            commands,
            Request::Publish {
                app: app(),
                channel: "live".into(),
                payload: payload(2),
                ephemeral: true,
                lifecycle: None,
            },
        ),
    )
    .await
    .unwrap();
    assert!(matches!(live, Response::Published { message } if message.id.is_empty()));
    assert!(!durable.is_finished());
    drop(transaction);
    assert!(matches!(durable.await.unwrap(), Response::Published { .. }));
    shutdown.send_replace(true);
    actor.await.unwrap().unwrap();
}

#[tokio::test]
async fn obsolete_connection_cannot_deliver_into_a_reconnected_peer_session() {
    let (_temp, broker) = new_broker().await;
    let mut subscription = broker.apps[&app()]
        .store
        .subscribe_ephemeral("live")
        .unwrap();
    let source = ChannelStore::open_in_memory().unwrap();
    let mut events = source.ephemeral_publications();
    let (old, _old_rx) = wire::queue(Budget::new(MAX_FRAME));
    let (new, _new_rx) = wire::queue(Budget::new(MAX_FRAME));
    let current = new.session();
    broker.peers.lock().insert(remote_id(), new);
    let (commands, receiver) = broker::commands(8);
    let (_control, control) = watch::channel(broker::Control::default());
    let (shutdown, stopping) = watch::channel(false);
    let actor = tokio::spawn(broker::run(broker, receiver, control, stopping));
    for (n, session) in [(1, old.session()), (2, current)] {
        source.publish_ephemeral("live", &payload(n)).unwrap();
        let frame = Frame::Publication {
            app: app(),
            publication: ChannelPublication::Event(events.try_recv().unwrap()),
        };
        let (mut tx, mut rx) = tokio::io::duplex(8192);
        wire::write(&mut tx, &frame).await.unwrap();
        let received = wire::read(&mut rx, &Budget::new(MAX_FRAME)).await.unwrap();
        commands
            .send(Command::Remote {
                peer: remote_id(),
                generation: 0,
                session,
                frame: received,
            })
            .await
            .unwrap();
    }
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), subscription.recv())
            .await
            .unwrap()
            .unwrap()
            .data,
        2
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(20), subscription.recv())
            .await
            .is_err()
    );
    shutdown.send_replace(true);
    actor.await.unwrap().unwrap();
}
