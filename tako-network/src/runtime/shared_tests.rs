use crate::shared::RecordReceipt;

#[test]
fn observed_secrets_retain_large_folded_recipient_maps_after_restart() {
    use crate::shared::{RecordContent, RecordKind};
    let fixture = Fixture::new();
    let mut runtime = fixture.open();
    let mutation = secret_mutation(
        &fixture,
        "shop/production",
        RecordKind::App,
        "TOKEN",
        Some("retained"),
    );
    fixture
        .call(
            &mut runtime,
            &fixture.admin,
            RpcRequest::PublishRecord(Box::new(mutation)),
            10,
        )
        .unwrap();
    {
        let mut store = fixture.store.0.borrow_mut();
        let record = &mut store.shared.records[0];
        let key = record.keys[&fixture.config.server_id].clone();
        // Other recipient envelopes are opaque to this reader; preserve them
        // while verifying that the administrator's own key remains usable.
        for i in 0..800 {
            record
                .keys
                .insert(format!("{i:064x}").parse().unwrap(), key.clone());
        }
        assert!(serde_json::to_vec(record).unwrap().len() > 128 * 1024);
    }
    for now in [11, 12] {
        let RpcResponse::Records(snapshot) = fixture
            .call(
                &mut runtime,
                &fixture.admin,
                RpcRequest::PullRecords { scope: None },
                now,
            )
            .unwrap()
        else {
            panic!("records expected");
        };
        assert_eq!(snapshot.records[0].keys.len(), 802);
        let plaintext = snapshot.records[0]
            .open(
                &snapshot,
                &fixture.admin,
                &fixture.config.network.initial_admin,
            )
            .unwrap()
            .unwrap();
        let content: RecordContent = serde_json::from_slice(&plaintext).unwrap();
        assert_eq!(&*content.value, "retained");
        drop(runtime);
        runtime = fixture.open();
    }
}

#[test]
fn populated_observed_secrets_compact_and_repair_in_one_snapshot() {
    use crate::shared::{RecordAddress, RecordKind, SharedRecord, SharedSnapshot};
    let fixture = Fixture::new();
    let mut runtime = fixture.open();
    let records: Vec<_> = (0..2000)
        .map(|i| SharedRecord {
            id: RequestId::random().unwrap(),
            address: RecordAddress {
                scope: "shop/production".parse().unwrap(),
                kind: RecordKind::App,
                name: format!("TOKEN_{i:04}"),
            },
            revision: 2,
            issuer: fixture.config.server_id.clone(),
            author: fixture.config.network.initial_admin.clone(),
            value: None,
            keys: BTreeMap::new(),
            plaintext: None,
        })
        .collect();
    let expected = SharedSnapshot {
        network: fixture.config.network.id.clone(),
        records,
    };
    let mut stale = expected.records[0].clone();
    stale.id = RequestId::random().unwrap();
    stale.revision = 1;
    {
        let tx = runtime.db.transaction().unwrap();
        // Old caches can contain multiple revisions of the same address.
        for record in expected.records.iter().chain(std::iter::once(&stale)) {
            tx.execute(
                "INSERT INTO shared_observed VALUES ('secret',?1,?2)",
                params![record.id.to_string(), serde_json::to_string(record).unwrap()],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        let mut store = fixture.store.0.borrow_mut();
        store.shared = expected.clone();
        store.shared.records[0] = stale;
    }
    for now in [10, 11] {
        let RpcResponse::Records(snapshot) = fixture
            .call(
                &mut runtime,
                &fixture.admin,
                RpcRequest::PullRecords { scope: None },
                now,
            )
            .unwrap()
        else {
            panic!("records expected");
        };
        assert_eq!(snapshot, expected);
        assert_eq!(fixture.store.0.borrow().shared, expected);
        let count: i64 = runtime
            .db
            .query_row("SELECT count(*) FROM shared_observed", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2000);
    }
}

#[test]
fn publication_window_tolerates_authenticated_client_clock_skew() {
    let fixture = Fixture::new();
    let mut runtime = fixture.open();
    let mut mutation = secret_mutation(
        &fixture,
        "shop/production",
        crate::shared::RecordKind::App,
        "TOKEN",
        None,
    );
    mutation.expires_at =
        10 + crate::shared::PUBLICATION_RETRY_SECONDS + fixture.config.limits.auth_window_seconds;
    fixture
        .call(
            &mut runtime,
            &fixture.admin,
            RpcRequest::PublishRecord(Box::new(mutation.clone())),
            10,
        )
        .unwrap();
    mutation.id = RequestId::random().unwrap();
    mutation.expires_at += 1;
    assert!(
        fixture
            .call(
                &mut runtime,
                &fixture.admin,
                RpcRequest::PublishRecord(Box::new(mutation)),
                10
            )
            .is_err()
    );
}
fn secret_mutation(
    fixture: &Fixture,
    scope: &str,
    kind: crate::shared::RecordKind,
    name: &str,
    value: Option<&str>,
) -> crate::shared::RecordMutation {
    secret_mutation_at_revision(fixture, scope, kind, name, value, 0)
}

fn secret_mutation_at_revision(
    fixture: &Fixture,
    scope: &str,
    kind: crate::shared::RecordKind,
    name: &str,
    value: Option<&str>,
    observed_revision: u64,
) -> crate::shared::RecordMutation {
    use crate::shared::{RecordAddress, RecordContent, RecordMutation};
    let id = RequestId::random().unwrap();
    let address = RecordAddress {
        scope: scope.parse().unwrap(),
        kind,
        name: name.into(),
    };
    let public = fixture
        .store
        .0
        .borrow()
        .snapshot
        .members
        .iter()
        .find(|member| member.device_id == fixture.config.server_id)
        .unwrap()
        .request
        .encryption_public_key
        .clone();
    let mut mutation = RecordMutation {
        id,
        address,
        value: None,
        observed_revision,
        expires_at: 86400,
    };
    mutation.value = value.map(|value| {
        let bytes = Zeroizing::new(
            serde_json::to_vec(&RecordContent {
                value: Zeroizing::new(value.into()),
                expires_on: None,
            })
            .unwrap(),
        );
        crate::sealed::SealedValue::seal(
            &public,
            &bytes,
            &mutation.context(&fixture.config.network.id).unwrap(),
        )
        .unwrap()
    });
    mutation
}

#[test]
fn secret_publication_survives_lost_response_and_retry_cannot_win_again() {
    use crate::shared::RecordKind;
    for fail_after in [false, true] {
        let fixture = Fixture::new();
        let mut runtime = fixture.open();
        let first = secret_mutation(
            &fixture,
            "shop/production",
            RecordKind::App,
            "API_KEY",
            Some("first"),
        );
        {
            let mut store = fixture.store.0.borrow_mut();
            store.fail_before_record = !fail_after;
            store.fail_after_record = fail_after;
        }
        assert!(
            fixture
                .call(
                    &mut runtime,
                    &fixture.admin,
                    RpcRequest::PublishRecord(Box::new(first.clone())),
                    10
                )
                .is_err()
        );
        drop(runtime);
        let mut runtime = fixture.open();
        let RpcResponse::RecordPublished(original) = fixture
            .call(
                &mut runtime,
                &fixture.admin,
                RpcRequest::PublishRecord(Box::new(first.clone())),
                11,
            )
            .unwrap()
        else {
            panic!("publication expected");
        };
        let second = secret_mutation(
            &fixture,
            "shop/production",
            RecordKind::App,
            "API_KEY",
            Some("second"),
        );
        let RpcResponse::RecordPublished(newer) = fixture
            .call(
                &mut runtime,
                &fixture.admin,
                RpcRequest::PublishRecord(Box::new(second)),
                12,
            )
            .unwrap()
        else {
            panic!("publication expected");
        };
        assert!(newer.order() > original.order());
        let RpcResponse::RecordPublished(retry) = fixture
            .call(
                &mut runtime,
                &fixture.admin,
                RpcRequest::PublishRecord(Box::new(first.clone())),
                13,
            )
            .unwrap()
        else {
            panic!("publication expected");
        };
        assert_eq!(retry, original);
        let store = fixture.store.0.borrow();
        assert_eq!(store.shared.records.len(), 1);
        assert_eq!(
            RecordReceipt::from(store.shared.latest().unwrap()[&first.address]),
            newer
        );
        // Completed acknowledgements cannot keep the overwritten ciphertext alive.
        let payloads: Vec<String> = runtime
            .db
            .prepare("SELECT payload FROM shared_publications")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(payloads.len(), 2);
        for payload in payloads {
            assert!(
                serde_json::from_str::<serde_json::Value>(&payload).unwrap()["pending"].is_null()
            );
            assert!(!payload.contains("ciphertext"));
        }
        let observed: String = runtime
            .db
            .query_row(
                "SELECT payload FROM shared_observed WHERE kind='secret'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<crate::shared::SharedRecord>(&observed)
                .unwrap()
                .id,
            newer.id
        );
    }
}

#[test]
fn secret_access_is_scoped_and_new_clients_receive_old_value_keys_via_server() {
    use crate::shared::{RecordContent, RecordKind};
    let fixture = Fixture::new();
    let mut runtime = fixture.open();
    for (scope, kind, name) in [
        ("shop/production", RecordKind::App, "API_KEY"),
        ("shop/staging", RecordKind::App, "API_KEY"),
        ("shop/production", RecordKind::Credential, "postgres_url"),
    ] {
        let mutation = secret_mutation(&fixture, scope, kind, name, Some("original"));
        fixture
            .call(
                &mut runtime,
                &fixture.admin,
                RpcRequest::PublishRecord(Box::new(mutation)),
                10,
            )
            .unwrap();
    }
    let client = LocalDeviceKeys::generate().unwrap();
    let mut grant = fixture.grant(&client);
    grant.permissions = Permissions::deploy(["shop/production".parse().unwrap()]);
    fixture.approve(&mut runtime, &grant);
    let complete = fixture.challenge(&mut runtime, &client, &grant, 11);
    fixture.call(&mut runtime, &client, complete, 12).unwrap();
    let RpcResponse::Records(shared) = fixture
        .call(
            &mut runtime,
            &client,
            RpcRequest::PullRecords { scope: None },
            13,
        )
        .unwrap()
    else {
        panic!("records expected");
    };
    assert_eq!(shared.records.len(), 1);
    assert!(
        shared.records[0].keys.contains_key(
            &client
                .device_id(&fixture.config.network.id, DeviceKind::Laptop)
                .unwrap()
        )
    );
    let client_id = client
        .device_id(&fixture.config.network.id, DeviceKind::Laptop)
        .unwrap();
    let plaintext = shared.records[0]
        .open(&shared, &client, &client_id)
        .unwrap()
        .unwrap();
    let content: RecordContent = serde_json::from_slice(&plaintext).unwrap();
    assert_eq!(&*content.value, "original");
    assert!(
        fixture
            .call(
                &mut runtime,
                &client,
                RpcRequest::PullRecords {
                    scope: Some("shop/staging".parse().unwrap()),
                },
                14
            )
            .is_err()
    );
    let denied = secret_mutation(
        &fixture,
        "shop/production",
        RecordKind::Credential,
        "postgres_url",
        Some("changed"),
    );
    assert!(
        fixture
            .call(
                &mut runtime,
                &client,
                RpcRequest::PublishRecord(Box::new(denied)),
                15
            )
            .is_err()
    );
    let accepted = secret_mutation(
        &fixture,
        "shop/production",
        RecordKind::App,
        "API_KEY",
        Some("changed"),
    );
    fixture
        .call(
            &mut runtime,
            &client,
            RpcRequest::PublishRecord(Box::new(accepted.clone())),
            16,
        )
        .unwrap();
    fixture
        .call(
            &mut runtime,
            &fixture.admin,
            RpcRequest::Revoke {
                id: RevocationId::random().unwrap(),
                target: client_id,
            },
            17,
        )
        .unwrap();
    assert!(
        fixture
            .call(
                &mut runtime,
                &client,
                RpcRequest::PullRecords { scope: None },
                18
            )
            .is_err()
    );
    assert!(
        fixture
            .call(
                &mut runtime,
                &client,
                RpcRequest::PublishRecord(Box::new(accepted)),
                18
            )
            .is_err()
    );
}

#[test]
fn deployers_pull_database_records_but_not_snapshot_keys() {
    use crate::shared::{DATABASE_SCOPE, RecordContent, RecordKind};
    let fixture = Fixture::new();
    let mut runtime = fixture.open();
    for (kind, value) in [
        (RecordKind::Database, r#"{"name":"shop"}"#),
        (RecordKind::DatabaseKey, "a2V5"),
    ] {
        let mutation = secret_mutation(&fixture, DATABASE_SCOPE, kind, "shop", Some(value));
        fixture
            .call(
                &mut runtime,
                &fixture.admin,
                RpcRequest::PublishRecord(Box::new(mutation)),
                10,
            )
            .unwrap();
    }
    let client = LocalDeviceKeys::generate().unwrap();
    let mut grant = fixture.grant(&client);
    grant.permissions = Permissions::deploy(["shop/production".parse().unwrap()]);
    fixture.approve(&mut runtime, &grant);
    let complete = fixture.challenge(&mut runtime, &client, &grant, 11);
    fixture.call(&mut runtime, &client, complete, 12).unwrap();
    let client_id = client
        .device_id(&fixture.config.network.id, DeviceKind::Laptop)
        .unwrap();
    let RpcResponse::Records(shared) = fixture
        .call(
            &mut runtime,
            &client,
            RpcRequest::PullRecords {
                scope: Some(DATABASE_SCOPE.parse().unwrap()),
            },
            13,
        )
        .unwrap()
    else {
        panic!("records expected");
    };
    assert_eq!(shared.records.len(), 1, "the snapshot key is left out");
    assert_eq!(shared.records[0].address.kind, RecordKind::Database);
    // The directory record is not secret, so it is stored as plaintext.
    assert!(shared.records[0].plaintext.is_some());
    let plaintext = shared.records[0]
        .open(&shared, &client, &client_id)
        .unwrap()
        .unwrap();
    let content: RecordContent = serde_json::from_slice(&plaintext).unwrap();
    assert_eq!(&*content.value, r#"{"name":"shop"}"#);
    // Only administrators publish database records.
    let forged = secret_mutation(
        &fixture,
        DATABASE_SCOPE,
        RecordKind::Database,
        "shop",
        Some(r#"{"name":"other"}"#),
    );
    assert!(
        fixture
            .call(
                &mut runtime,
                &client,
                RpcRequest::PublishRecord(Box::new(forged)),
                14
            )
            .is_err()
    );
}

#[test]
fn secret_tampering_and_operation_reuse_fail_without_publishing() {
    use crate::shared::RecordKind;
    let fixture = Fixture::new();
    let mut runtime = fixture.open();
    let mut mutation = secret_mutation(
        &fixture,
        "shop/production",
        RecordKind::App,
        "API_KEY",
        Some("value"),
    );
    let original = mutation.clone();
    mutation.address.scope = "shop/staging".parse().unwrap();
    assert!(
        fixture
            .call(
                &mut runtime,
                &fixture.admin,
                RpcRequest::PublishRecord(Box::new(mutation)),
                10
            )
            .is_err()
    );
    assert!(fixture.store.0.borrow().shared.records.is_empty());
    fixture
        .call(
            &mut runtime,
            &fixture.admin,
            RpcRequest::PublishRecord(Box::new(original.clone())),
            11,
        )
        .unwrap();
    let mut changed = original;
    changed.value = None;
    assert!(
        fixture
            .call(
                &mut runtime,
                &fixture.admin,
                RpcRequest::PublishRecord(Box::new(changed)),
                12
            )
            .is_err()
    );
    assert_eq!(fixture.store.0.borrow().shared.records.len(), 1);
}

#[test]
fn client_observation_orders_edits_after_a_revision_missing_on_this_server() {
    use crate::shared::RecordKind;
    let fixture = Fixture::new();
    let mut runtime = fixture.open();
    let mutation = secret_mutation_at_revision(
        &fixture,
        "shop/production",
        RecordKind::App,
        "TOKEN",
        Some("causally-later"),
        40,
    );
    let RpcResponse::RecordPublished(record) = fixture
        .call(
            &mut runtime,
            &fixture.admin,
            RpcRequest::PublishRecord(Box::new(mutation.clone())),
            10,
        )
        .unwrap()
    else {
        panic!("publication expected")
    };
    assert_eq!(record.revision, 41);
    // A retry after restart retains revision 41, even if another write won.
    let newer = secret_mutation(
        &fixture,
        "shop/production",
        RecordKind::App,
        "TOKEN",
        Some("newer"),
    );
    let RpcResponse::RecordPublished(newer) = fixture
        .call(
            &mut runtime,
            &fixture.admin,
            RpcRequest::PublishRecord(Box::new(newer)),
            11,
        )
        .unwrap()
    else {
        panic!("publication expected")
    };
    assert_eq!(newer.revision, 42);
    drop(runtime);
    let mut runtime = fixture.open();
    let RpcResponse::RecordPublished(retry) = fixture
        .call(
            &mut runtime,
            &fixture.admin,
            RpcRequest::PublishRecord(Box::new(mutation.clone())),
            12,
        )
        .unwrap()
    else {
        panic!("publication expected")
    };
    assert_eq!(retry, record);
    let mut overflow = secret_mutation(&fixture, "shop/production", RecordKind::App, "TOKEN", None);
    overflow.observed_revision = u64::MAX;
    assert!(
        fixture
            .call(
                &mut runtime,
                &fixture.admin,
                RpcRequest::PublishRecord(Box::new(overflow)),
                13
            )
            .is_err()
    );
    assert_eq!(
        RecordReceipt::from(fixture.store.0.borrow().shared.latest().unwrap()[&mutation.address]),
        newer
    );
}

#[test]
fn expired_receipts_are_collected_and_deadlines_and_observations_are_authenticated() {
    let fixture = Fixture::new();
    let mut runtime = fixture.open();
    let mutation = secret_mutation(
        &fixture,
        "shop/production",
        crate::shared::RecordKind::App,
        "TOKEN",
        Some("value"),
    );
    for variant in 0..2 {
        let mut changed = mutation.clone();
        if variant == 0 {
            changed.expires_at += 1;
        } else {
            changed.observed_revision = 10;
        }
        assert!(
            fixture
                .call(
                    &mut runtime,
                    &fixture.admin,
                    RpcRequest::PublishRecord(Box::new(changed)),
                    10
                )
                .is_err()
        );
    }
    assert!(fixture.store.0.borrow().shared.records.is_empty());
    fixture
        .call(
            &mut runtime,
            &fixture.admin,
            RpcRequest::PublishRecord(Box::new(mutation.clone())),
            11,
        )
        .unwrap();
    runtime
        .reconcile_record_keys(mutation.expires_at)
        .unwrap();
    let count: i64 = runtime
        .db
        .query_row("SELECT count(*) FROM shared_publications", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 0);
    assert!(
        fixture
            .call(
                &mut runtime,
                &fixture.admin,
                RpcRequest::PublishRecord(Box::new(mutation.clone())),
                mutation.expires_at
            )
            .is_err()
    );
    let mut extended = mutation.clone();
    extended.expires_at += 10;
    assert!(
        fixture
            .call(
                &mut runtime,
                &fixture.admin,
                RpcRequest::PublishRecord(Box::new(extended)),
                mutation.expires_at
            )
            .is_err()
    );
    assert_eq!(fixture.store.0.borrow().shared.records.len(), 1);
}

#[test]
fn backup_key_selection_is_published_as_plaintext() {
    use crate::shared::RecordKind;
    let fixture = Fixture::new();
    let mut runtime = fixture.open();
    let mutation = secret_mutation(
        &fixture,
        "shop/production",
        RecordKind::BackupActive,
        "active",
        Some("backup-key-0123456789abcdef"),
    );
    fixture
        .call(
            &mut runtime,
            &fixture.admin,
            RpcRequest::PublishRecord(Box::new(mutation)),
            10,
        )
        .unwrap();
    let store = fixture.store.0.borrow();
    let record = &store.shared.records[0];
    assert!(record.value.is_none());
    assert!(record.keys.is_empty());
    assert_eq!(
        record.plaintext.as_ref().unwrap().value,
        "backup-key-0123456789abcdef"
    );
}

#[test]
fn pull_returns_records_this_server_cannot_open_instead_of_failing() {
    use crate::shared::{RecordAddress, RecordContent, RecordKind, SharedRecord, SharedStore};
    let fixture = Fixture::new();
    let mut runtime = fixture.open();
    let mutation = secret_mutation(
        &fixture,
        "shop/production",
        RecordKind::App,
        "API_KEY",
        Some("readable"),
    );
    fixture
        .call(
            &mut runtime,
            &fixture.admin,
            RpcRequest::PublishRecord(Box::new(mutation)),
            10,
        )
        .unwrap();
    // A record another server published before this server received its key.
    let admin = fixture.config.network.initial_admin.clone();
    let public = fixture
        .store
        .0
        .borrow()
        .snapshot
        .members
        .iter()
        .find(|member| member.device_id == admin)
        .unwrap()
        .request
        .encryption_public_key
        .clone();
    let id = RequestId::random().unwrap();
    let address = RecordAddress {
        scope: "shop/production".parse().unwrap(),
        kind: RecordKind::App,
        name: "PENDING".into(),
    };
    let content_key = crate::sealed::random_content_key().unwrap();
    let context = address.context(&fixture.config.network.id, &id).unwrap();
    let unreadable = SharedRecord {
        id,
        address,
        revision: 1,
        issuer: admin.clone(),
        author: admin.clone(),
        value: Some(crate::sealed::Ciphertext::encrypt(&content_key, b"later", &context).unwrap()),
        keys: [(
            admin.clone(),
            crate::sealed::SealedValue::seal(&public, content_key.as_slice(), b"elsewhere")
                .unwrap(),
        )]
        .into(),
        plaintext: None,
    };
    SharedStore::append_record(&mut fixture.store.0.borrow_mut().shared, &unreadable).unwrap();

    let client = LocalDeviceKeys::generate().unwrap();
    let mut grant = fixture.grant(&client);
    grant.permissions = Permissions::deploy(["shop/production".parse().unwrap()]);
    fixture.approve(&mut runtime, &grant);
    let complete = fixture.challenge(&mut runtime, &client, &grant, 11);
    fixture.call(&mut runtime, &client, complete, 12).unwrap();
    let client_id = client
        .device_id(&fixture.config.network.id, DeviceKind::Laptop)
        .unwrap();
    let RpcResponse::Records(shared) = fixture
        .call(
            &mut runtime,
            &client,
            RpcRequest::PullRecords {
                scope: Some("shop/production".parse().unwrap()),
            },
            13,
        )
        .unwrap()
    else {
        panic!("records expected");
    };
    let latest = shared.latest().unwrap();
    assert_eq!(latest.len(), 2);
    let readable = latest
        .values()
        .find(|record| record.address.name == "API_KEY")
        .unwrap();
    let bytes = readable.open(&shared, &client, &client_id).unwrap().unwrap();
    let content: RecordContent = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(&*content.value, "readable");
    let pending = latest
        .values()
        .find(|record| record.address.name == "PENDING")
        .unwrap();
    assert!(!pending.keys.contains_key(&client_id));
    assert!(pending.open(&shared, &client, &client_id).is_err());
}
