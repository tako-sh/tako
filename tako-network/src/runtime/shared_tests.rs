use crate::shared::SecretReceipt;

#[test]
fn observed_secrets_retain_large_folded_recipient_maps_after_restart() {
    use crate::shared::{SecretContent, SecretKind};
    let fixture = Fixture::new();
    let mut runtime = fixture.open();
    let mutation = secret_mutation(
        &fixture,
        "shop/production",
        SecretKind::App,
        "TOKEN",
        Some("retained"),
    );
    fixture
        .call(
            &mut runtime,
            &fixture.admin,
            RpcRequest::PublishSecret(Box::new(mutation)),
            10,
        )
        .unwrap();
    {
        let mut store = fixture.store.0.borrow_mut();
        let record = &mut store.shared.secrets[0];
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
        let RpcResponse::Secrets(snapshot) = fixture
            .call(
                &mut runtime,
                &fixture.admin,
                RpcRequest::PullSecrets { scope: None },
                now,
            )
            .unwrap()
        else {
            panic!("secrets expected");
        };
        assert_eq!(snapshot.secrets[0].keys.len(), 802);
        let plaintext = snapshot.secrets[0]
            .decrypt(
                &snapshot,
                &fixture.admin,
                &fixture.config.network.initial_admin,
            )
            .unwrap()
            .unwrap();
        let content: SecretContent = serde_json::from_slice(&plaintext).unwrap();
        assert_eq!(&*content.value, "retained");
        drop(runtime);
        runtime = fixture.open();
    }
}

#[test]
fn populated_observed_secrets_compact_and_repair_in_one_snapshot() {
    use crate::shared::{SecretAddress, SecretKind, SecretRecord, SharedSnapshot};
    let fixture = Fixture::new();
    let mut runtime = fixture.open();
    let secrets: Vec<_> = (0..2000)
        .map(|i| SecretRecord {
            id: RequestId::random().unwrap(),
            address: SecretAddress {
                scope: "shop/production".parse().unwrap(),
                kind: SecretKind::App,
                name: format!("TOKEN_{i:04}"),
            },
            revision: 2,
            issuer: fixture.config.server_id.clone(),
            author: fixture.config.network.initial_admin.clone(),
            value: None,
            keys: BTreeMap::new(),
        })
        .collect();
    let expected = SharedSnapshot {
        network: fixture.config.network.id.clone(),
        secrets,
    };
    let mut stale = expected.secrets[0].clone();
    stale.id = RequestId::random().unwrap();
    stale.revision = 1;
    {
        let tx = runtime.db.transaction().unwrap();
        // Old caches can contain multiple revisions of the same address.
        for record in expected.secrets.iter().chain(std::iter::once(&stale)) {
            tx.execute(
                "INSERT INTO shared_observed VALUES ('secret',?1,?2)",
                params![record.id.to_string(), serde_json::to_string(record).unwrap()],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        let mut store = fixture.store.0.borrow_mut();
        store.shared = expected.clone();
        store.shared.secrets[0] = stale;
    }
    for now in [10, 11] {
        let RpcResponse::Secrets(snapshot) = fixture
            .call(
                &mut runtime,
                &fixture.admin,
                RpcRequest::PullSecrets { scope: None },
                now,
            )
            .unwrap()
        else {
            panic!("secrets expected");
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
        crate::shared::SecretKind::App,
        "TOKEN",
        None,
    );
    mutation.expires_at =
        10 + crate::shared::SECRET_RETRY_SECONDS + fixture.config.limits.auth_window_seconds;
    fixture
        .call(
            &mut runtime,
            &fixture.admin,
            RpcRequest::PublishSecret(Box::new(mutation.clone())),
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
                RpcRequest::PublishSecret(Box::new(mutation)),
                10
            )
            .is_err()
    );
}
fn secret_mutation(
    fixture: &Fixture,
    scope: &str,
    kind: crate::shared::SecretKind,
    name: &str,
    value: Option<&str>,
) -> crate::shared::SecretMutation {
    secret_mutation_at_revision(fixture, scope, kind, name, value, 0)
}

fn secret_mutation_at_revision(
    fixture: &Fixture,
    scope: &str,
    kind: crate::shared::SecretKind,
    name: &str,
    value: Option<&str>,
    observed_revision: u64,
) -> crate::shared::SecretMutation {
    use crate::shared::{SecretAddress, SecretContent, SecretMutation};
    let id = RequestId::random().unwrap();
    let address = SecretAddress {
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
    let mut mutation = SecretMutation {
        id,
        address,
        value: None,
        observed_revision,
        expires_at: 86400,
    };
    mutation.value = value.map(|value| {
        let bytes = Zeroizing::new(
            serde_json::to_vec(&SecretContent {
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
    use crate::shared::SecretKind;
    for fail_after in [false, true] {
        let fixture = Fixture::new();
        let mut runtime = fixture.open();
        let first = secret_mutation(
            &fixture,
            "shop/production",
            SecretKind::App,
            "API_KEY",
            Some("first"),
        );
        {
            let mut store = fixture.store.0.borrow_mut();
            store.fail_before_secret = !fail_after;
            store.fail_after_secret = fail_after;
        }
        assert!(
            fixture
                .call(
                    &mut runtime,
                    &fixture.admin,
                    RpcRequest::PublishSecret(Box::new(first.clone())),
                    10
                )
                .is_err()
        );
        drop(runtime);
        let mut runtime = fixture.open();
        let RpcResponse::SecretPublished(original) = fixture
            .call(
                &mut runtime,
                &fixture.admin,
                RpcRequest::PublishSecret(Box::new(first.clone())),
                11,
            )
            .unwrap()
        else {
            panic!("publication expected");
        };
        let second = secret_mutation(
            &fixture,
            "shop/production",
            SecretKind::App,
            "API_KEY",
            Some("second"),
        );
        let RpcResponse::SecretPublished(newer) = fixture
            .call(
                &mut runtime,
                &fixture.admin,
                RpcRequest::PublishSecret(Box::new(second)),
                12,
            )
            .unwrap()
        else {
            panic!("publication expected");
        };
        assert!(newer.order() > original.order());
        let RpcResponse::SecretPublished(retry) = fixture
            .call(
                &mut runtime,
                &fixture.admin,
                RpcRequest::PublishSecret(Box::new(first.clone())),
                13,
            )
            .unwrap()
        else {
            panic!("publication expected");
        };
        assert_eq!(retry, original);
        let store = fixture.store.0.borrow();
        assert_eq!(store.shared.secrets.len(), 1);
        assert_eq!(
            SecretReceipt::from(store.shared.latest().unwrap()[&first.address]),
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
            serde_json::from_str::<crate::shared::SecretRecord>(&observed)
                .unwrap()
                .id,
            newer.id
        );
    }
}

#[test]
fn secret_access_is_scoped_and_new_clients_receive_old_value_keys_via_server() {
    use crate::shared::{SecretContent, SecretKind};
    let fixture = Fixture::new();
    let mut runtime = fixture.open();
    for (scope, kind, name) in [
        ("shop/production", SecretKind::App, "API_KEY"),
        ("shop/staging", SecretKind::App, "API_KEY"),
        ("shop/production", SecretKind::Credential, "postgres_url"),
    ] {
        let mutation = secret_mutation(&fixture, scope, kind, name, Some("original"));
        fixture
            .call(
                &mut runtime,
                &fixture.admin,
                RpcRequest::PublishSecret(Box::new(mutation)),
                10,
            )
            .unwrap();
    }
    let client = LocalDeviceKeys::generate().unwrap();
    let mut grant = fixture.grant(&client);
    grant
        .permissions
        .deployments
        .insert("shop/production".parse().unwrap());
    fixture.approve(&mut runtime, &grant);
    let complete = fixture.challenge(&mut runtime, &client, &grant, 11);
    fixture.call(&mut runtime, &client, complete, 12).unwrap();
    let RpcResponse::Secrets(shared) = fixture
        .call(
            &mut runtime,
            &client,
            RpcRequest::PullSecrets { scope: None },
            13,
        )
        .unwrap()
    else {
        panic!("secrets expected");
    };
    assert_eq!(shared.secrets.len(), 1);
    assert!(
        shared.secrets[0].keys.contains_key(
            &client
                .device_id(&fixture.config.network.id, DeviceKind::Laptop)
                .unwrap()
        )
    );
    let client_id = client
        .device_id(&fixture.config.network.id, DeviceKind::Laptop)
        .unwrap();
    let plaintext = shared.secrets[0]
        .decrypt(&shared, &client, &client_id)
        .unwrap()
        .unwrap();
    let content: SecretContent = serde_json::from_slice(&plaintext).unwrap();
    assert_eq!(&*content.value, "original");
    assert!(
        fixture
            .call(
                &mut runtime,
                &client,
                RpcRequest::PullSecrets {
                    scope: Some("shop/staging".parse().unwrap()),
                },
                14
            )
            .is_err()
    );
    let denied = secret_mutation(
        &fixture,
        "shop/production",
        SecretKind::Credential,
        "postgres_url",
        Some("changed"),
    );
    assert!(
        fixture
            .call(
                &mut runtime,
                &client,
                RpcRequest::PublishSecret(Box::new(denied)),
                15
            )
            .is_err()
    );
    let accepted = secret_mutation(
        &fixture,
        "shop/production",
        SecretKind::App,
        "API_KEY",
        Some("changed"),
    );
    fixture
        .call(
            &mut runtime,
            &client,
            RpcRequest::PublishSecret(Box::new(accepted.clone())),
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
                RpcRequest::PullSecrets { scope: None },
                18
            )
            .is_err()
    );
    assert!(
        fixture
            .call(
                &mut runtime,
                &client,
                RpcRequest::PublishSecret(Box::new(accepted)),
                18
            )
            .is_err()
    );
}

#[test]
fn secret_tampering_and_operation_reuse_fail_without_publishing() {
    use crate::shared::SecretKind;
    let fixture = Fixture::new();
    let mut runtime = fixture.open();
    let mut mutation = secret_mutation(
        &fixture,
        "shop/production",
        SecretKind::App,
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
                RpcRequest::PublishSecret(Box::new(mutation)),
                10
            )
            .is_err()
    );
    assert!(fixture.store.0.borrow().shared.secrets.is_empty());
    fixture
        .call(
            &mut runtime,
            &fixture.admin,
            RpcRequest::PublishSecret(Box::new(original.clone())),
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
                RpcRequest::PublishSecret(Box::new(changed)),
                12
            )
            .is_err()
    );
    assert_eq!(fixture.store.0.borrow().shared.secrets.len(), 1);
}

#[test]
fn client_observation_orders_edits_after_a_revision_missing_on_this_server() {
    use crate::shared::SecretKind;
    let fixture = Fixture::new();
    let mut runtime = fixture.open();
    let mutation = secret_mutation_at_revision(
        &fixture,
        "shop/production",
        SecretKind::App,
        "TOKEN",
        Some("causally-later"),
        40,
    );
    let RpcResponse::SecretPublished(record) = fixture
        .call(
            &mut runtime,
            &fixture.admin,
            RpcRequest::PublishSecret(Box::new(mutation.clone())),
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
        SecretKind::App,
        "TOKEN",
        Some("newer"),
    );
    let RpcResponse::SecretPublished(newer) = fixture
        .call(
            &mut runtime,
            &fixture.admin,
            RpcRequest::PublishSecret(Box::new(newer)),
            11,
        )
        .unwrap()
    else {
        panic!("publication expected")
    };
    assert_eq!(newer.revision, 42);
    drop(runtime);
    let mut runtime = fixture.open();
    let RpcResponse::SecretPublished(retry) = fixture
        .call(
            &mut runtime,
            &fixture.admin,
            RpcRequest::PublishSecret(Box::new(mutation.clone())),
            12,
        )
        .unwrap()
    else {
        panic!("publication expected")
    };
    assert_eq!(retry, record);
    let mut overflow = secret_mutation(&fixture, "shop/production", SecretKind::App, "TOKEN", None);
    overflow.observed_revision = u64::MAX;
    assert!(
        fixture
            .call(
                &mut runtime,
                &fixture.admin,
                RpcRequest::PublishSecret(Box::new(overflow)),
                13
            )
            .is_err()
    );
    assert_eq!(
        SecretReceipt::from(fixture.store.0.borrow().shared.latest().unwrap()[&mutation.address]),
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
        crate::shared::SecretKind::App,
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
                    RpcRequest::PublishSecret(Box::new(changed)),
                    10
                )
                .is_err()
        );
    }
    assert!(fixture.store.0.borrow().shared.secrets.is_empty());
    fixture
        .call(
            &mut runtime,
            &fixture.admin,
            RpcRequest::PublishSecret(Box::new(mutation.clone())),
            11,
        )
        .unwrap();
    runtime
        .reconcile_secret_grants(mutation.expires_at)
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
                RpcRequest::PublishSecret(Box::new(mutation.clone())),
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
                RpcRequest::PublishSecret(Box::new(extended)),
                mutation.expires_at
            )
            .is_err()
    );
    assert_eq!(fixture.store.0.borrow().shared.secrets.len(), 1);
}
