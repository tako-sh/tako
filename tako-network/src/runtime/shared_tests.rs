fn secret_mutation(
    fixture: &Fixture,
    scope: &str,
    kind: crate::shared::SecretKind,
    name: &str,
    value: Option<&str>,
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
    let value = value.map(|value| {
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
            &address.context(&fixture.config.network.id, &id).unwrap(),
        )
        .unwrap()
    });
    SecretMutation { id, address, value }
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
        assert_eq!(store.shared.secrets.len(), 2);
        assert_eq!(store.shared.latest().unwrap()[&first.address], &newer);
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
    assert_eq!(shared.grants.len(), 1);
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
