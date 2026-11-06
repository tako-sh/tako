use reviewed_adapter::{BearerToken, CorrosionStore, RegistryStore};
use tako_network::api::{RuntimeError, RuntimeResult};

fn adapter(snapshot: &RegistrySnapshot) -> CorrosionStore {
    let mut store = CorrosionStore::new(
        "http://127.0.0.1:8080",
        BearerToken::new("review-token-never-sent".into()).unwrap(),
        snapshot.network.clone(),
    )
    .unwrap();
    store.transport = Some(mock_http::post);
    store
}

#[test]
fn historical_acks_compact_and_cannot_block_revocations() {
    let (mut snapshot, _, root) = support::bootstrap();
    let (_, target) = support::add_laptop(&mut snapshot, &root, Permissions::default(), false, 2);
    let server = support::add_server(&mut snapshot, &root, 3);
    let revocation = support::revoke(&mut snapshot, &root, &target, 4);
    let next = snapshot
        .plan_revocation(RevocationId::random().unwrap(), &root, &server, 5)
        .unwrap();
    for _ in 0..reviewed_adapter::MAX_REGISTRY_ROWS - 6 {
        snapshot.acks.push(Ack {
            revocation: revocation.clone(),
            server: server.clone(),
            boot: BootId::random().unwrap(),
        });
    }
    snapshot.validate().unwrap();
    mock_http::seed(&snapshot);
    assert_eq!(mock_http::count(), 9_999);
    let mut store = adapter(&snapshot);
    store
        .append_ack(&Ack {
            revocation: revocation.clone(),
            server: server.clone(),
            boot: BootId::random().unwrap(),
        })
        .unwrap();
    assert_eq!(mock_http::count(), 6);
    store.append_revocation(&next).unwrap();
    let ack = Ack {
        revocation,
        server,
        boot: BootId::random().unwrap(),
    };
    store.append_ack(&ack).unwrap();
    assert_eq!(mock_http::count(), 7);
    let result = store.snapshot().unwrap();
    assert!(result.revocations.contains(&next));
    assert_eq!(result.acks, vec![ack]);
}

#[test]
fn byte_admission_limit_preserves_reads_and_denials_after_convergence() {
    let (base, admin_keys, root) = support::bootstrap();
    let permissions = Permissions {
        deployments: (0..1024)
            .map(|i| format!("a{:058}{i:04}/e{:049}", 0, 0).parse().unwrap())
            .collect(),
        ..Permissions::default()
    };
    let new_member = || {
        let keys = LocalDeviceKeys::generate().unwrap();
        let member = base
            .approve(
                support::verified(&keys, &base.network.id, DeviceKind::Laptop, 2),
                support::approval(
                    MemberKind::Laptop,
                    permissions.clone(),
                    Some(root.clone()),
                    2,
                ),
            )
            .unwrap();
        assert!(serde_json::to_vec(&member).unwrap().len() < reviewed_adapter::MAX_RECORD_BYTES);
        tako_network::api::SignedRequest::sign(
            &admin_keys,
            base.network.id.clone(),
            admin_keys
                .device_id(&base.network.id, DeviceKind::Server)
                .unwrap(),
            DeviceKind::Laptop,
            2,
            tako_network::api::RpcRequest::Approve(Box::new(tako_network::api::ApproveRequest {
                pending_id: RequestId::random().unwrap(),
                request: member.request.clone(),
                kind: member.kind.clone(),
                permissions: member.permissions.clone(),
                expires_at: 1000,
                member_expires_at: None,
            })),
        )
        .unwrap();
        member
    };
    let mut snapshot = base.clone();
    for _ in 0..136 {
        snapshot.members.push(new_member());
    }
    mock_http::seed(&snapshot);
    let mut store = adapter(&snapshot);
    store.snapshot().unwrap();
    let count_before = mock_http::count();
    assert!(matches!(
        store.append_member(&new_member()),
        Err(RuntimeError::Capacity("registry admissions"))
    ));
    assert_eq!(mock_http::count(), count_before);
    store.snapshot().unwrap();

    snapshot.members.push(new_member());
    snapshot.members.push(new_member());
    for members in snapshot.members[1..].chunks(46) {
        let mut replica = base.clone();
        replica
            .members
            .extend_from_slice(&members[..members.len() - 1]);
        mock_http::seed(&replica);
        let mut store = adapter(&replica);
        store.append_member(members.last().unwrap()).unwrap();
        assert_eq!(
            store.snapshot().unwrap().members.len(),
            replica.members.len() + 1
        );
    }
    mock_http::seed(&snapshot);
    let converged = store.snapshot().unwrap();
    assert_eq!(converged.members.len(), snapshot.members.len());
    let denial = snapshot
        .plan_revocation(
            RevocationId::random().unwrap(),
            &root,
            &snapshot.members[1].device_id,
            3,
        )
        .unwrap();
    store.append_revocation(&denial).unwrap();
    assert!(store.snapshot().unwrap().revocations.contains(&denial));
}

#[test]
fn conflicting_immutable_records_across_page_boundary_fail_closed() {
    let (mut snapshot, _, root) = support::bootstrap();
    for _ in 0..8 {
        support::add_laptop(&mut snapshot, &root, Permissions::default(), false, 2);
    }
    snapshot.members.sort_by_key(|m| m.device_id.clone());
    let mut conflict = snapshot.members[7].clone();
    conflict.created_at += 1;
    snapshot.members.push(conflict);
    mock_http::seed(&snapshot);
    assert!(matches!(
        adapter(&snapshot).snapshot(),
        Err(RuntimeError::Conflict("record ID"))
    ));
}

#[test]
fn positive_row_limit_keeps_denials_and_ack_progress_available() {
    let (mut snapshot, _, root) = support::bootstrap();
    let (_, target) = support::add_laptop(&mut snapshot, &root, Permissions::default(), false, 2);
    let server = support::add_server(&mut snapshot, &root, 3);
    let denial = snapshot
        .plan_revocation(RevocationId::random().unwrap(), &root, &target, 4)
        .unwrap();
    for _ in 0..reviewed_adapter::MAX_REGISTRY_ROWS / 2 - 4 {
        let mut denial = denial.clone();
        denial.id = RevocationId::random().unwrap();
        snapshot.revocations.push(denial);
    }
    let mut candidate_snapshot = snapshot.clone();
    let (_, candidate_id) = support::add_laptop(
        &mut candidate_snapshot,
        &root,
        Permissions::default(),
        false,
        5,
    );
    let candidate = candidate_snapshot
        .members
        .iter()
        .find(|m| m.device_id == candidate_id)
        .unwrap();
    mock_http::seed(&snapshot);
    let mut store = adapter(&snapshot);
    assert!(matches!(
        store.append_member(candidate),
        Err(RuntimeError::Capacity("registry admissions"))
    ));
    assert_eq!(mock_http::count(), 5_000);
    store.append_revocation(&denial).unwrap();
    let ack = Ack {
        revocation: denial.id.clone(),
        server,
        boot: BootId::random().unwrap(),
    };
    store.append_ack(&ack).unwrap();
    let projected = store.snapshot().unwrap();
    assert!(projected.revocations.contains(&denial));
    assert!(projected.acks.contains(&ack));
}

#[test]
fn denial_writes_do_not_require_materializing_an_oversized_history() {
    let (mut snapshot, _, root) = support::bootstrap();
    let (_, target) = support::add_laptop(&mut snapshot, &root, Permissions::default(), false, 2);
    let denial = snapshot
        .plan_revocation(RevocationId::random().unwrap(), &root, &target, 4)
        .unwrap();
    for _ in 0..reviewed_adapter::MAX_REGISTRY_ROWS {
        let mut denial = denial.clone();
        denial.id = RevocationId::random().unwrap();
        snapshot.revocations.push(denial);
    }
    mock_http::seed(&snapshot);
    let mut store = adapter(&snapshot);
    assert!(matches!(store.snapshot(), Err(RuntimeError::Capacity(_))));
    let count = mock_http::count();
    store.append_revocation(&denial).unwrap();
    store.append_revocation(&denial).unwrap();
    assert_eq!(mock_http::count(), count + 1);
    let mut conflict = denial;
    conflict.created_at += 1;
    assert!(matches!(
        store.append_revocation(&conflict),
        Err(RuntimeError::Conflict(_))
    ));
    assert_eq!(mock_http::count(), count + 1);
}

#[test]
fn ack_projection_never_uses_authority_capacity_or_invents_a_boot() {
    let (mut snapshot, _, root) = support::bootstrap();
    let (_, target) = support::add_laptop(&mut snapshot, &root, Permissions::default(), false, 2);
    let server = support::add_server(&mut snapshot, &root, 3);
    let denial = snapshot
        .plan_revocation(RevocationId::random().unwrap(), &root, &target, 4)
        .unwrap();
    let boot = BootId::random().unwrap();
    for _ in 0..reviewed_adapter::MAX_REGISTRY_ROWS - 4 {
        let mut denial = denial.clone();
        denial.id = RevocationId::random().unwrap();
        snapshot.acks.push(Ack {
            revocation: denial.id.clone(),
            server: server.clone(),
            boot: boot.clone(),
        });
        snapshot.revocations.push(denial);
    }
    mock_http::seed(&snapshot);
    let projected = adapter(&snapshot).snapshot().unwrap();
    assert_eq!(projected.revocations.len(), snapshot.revocations.len());
    assert!(projected.acks.is_empty());
    let progress = projected
        .revocation_progress(
            &projected.revocations[0].id,
            &[(server.clone(), boot)].into_iter().collect(),
        )
        .unwrap();
    assert!(progress.applied_servers.is_empty());
    assert!(progress.pending_servers.contains(&server));
}

#[test]
fn targeted_mutations_still_pin_the_network_descriptor() {
    let (mut snapshot, _, root) = support::bootstrap();
    let (_, target) = support::add_laptop(&mut snapshot, &root, Permissions::default(), false, 2);
    let denial = snapshot
        .plan_revocation(RevocationId::random().unwrap(), &root, &target, 3)
        .unwrap();
    let mut store = adapter(&snapshot);
    snapshot.network = support::bootstrap().0.network;
    mock_http::seed(&snapshot);
    assert!(matches!(
        store.append_revocation(&denial),
        Err(RuntimeError::Conflict("network descriptor"))
    ));
}

#[test]
fn denial_record_can_capture_every_server_in_a_bounded_projection() {
    let (mut snapshot, _, root) = support::bootstrap();
    let (_, target) = support::add_laptop(&mut snapshot, &root, Permissions::default(), false, 2);
    let mut denial = snapshot
        .plan_revocation(RevocationId::random().unwrap(), &root, &target, 3)
        .unwrap();
    denial.captured_servers = (0..reviewed_adapter::MAX_REGISTRY_ROWS)
        .map(|i| format!("{i:064x}").parse().unwrap())
        .collect();
    assert!(serde_json::to_vec(&denial).unwrap().len() > reviewed_adapter::MAX_RECORD_BYTES);
    mock_http::seed(&snapshot);
    let mut store = adapter(&snapshot);
    store.append_revocation(&denial).unwrap();
    assert_eq!(store.snapshot().unwrap().revocations, vec![denial]);
}

#[test]
fn ack_republication_compacts_history_without_a_serialization_conflict() {
    let (mut snapshot, _, root) = support::bootstrap();
    let (_, target) = support::add_laptop(&mut snapshot, &root, Permissions::default(), false, 2);
    let server = support::add_server(&mut snapshot, &root, 3);
    let revocation = support::revoke(&mut snapshot, &root, &target, 4);
    let current = Ack {
        revocation: revocation.clone(),
        server: server.clone(),
        boot: BootId::random().unwrap(),
    };
    snapshot.acks = vec![
        current.clone(),
        Ack {
            revocation,
            server: server.clone(),
            boot: BootId::random().unwrap(),
        },
    ];
    mock_http::seed(&snapshot);
    let mut store = adapter(&snapshot);
    store.append_ack(&current).unwrap();
    store.append_ack(&current).unwrap();
    let projected = store.snapshot().unwrap();
    assert_eq!(projected.acks, vec![current.clone()]);
    assert!(
        projected
            .revocation_progress(
                &current.revocation,
                &[(server.clone(), BootId::random().unwrap())]
                    .into_iter()
                    .collect()
            )
            .unwrap()
            .applied_servers
            .is_empty()
    );
    assert!(
        projected
            .revocation_progress(
                &current.revocation,
                &[(server.clone(), current.boot)].into_iter().collect()
            )
            .unwrap()
            .applied_servers
            .contains(&server)
    );
}

use serde_json::{Value, json};

fn raw_record(kind: &str, id: &str, payload: &str) {
    mock_http::post(
        "/v1/transactions",
        &json!([[
            "INSERT INTO tako_network_records VALUES (?, ?, ?, ?)",
            [
                kind,
                id,
                hex::encode(openssl::sha::sha256(payload.as_bytes())),
                payload
            ]
        ]]),
    )
    .unwrap();
}

fn replace_encoding(kind: &str, id: &str, payload: &str) {
    mock_http::post(
        "/v1/transactions",
        &json!([[
            "UPDATE tako_network_records SET digest=?, payload=? WHERE kind=? AND record_id=?",
            [
                hex::encode(openssl::sha::sha256(payload.as_bytes())),
                payload,
                kind,
                id
            ]
        ]]),
    )
    .unwrap();
}

#[test]
fn noncanonical_payload_bytes_cannot_break_admission_projection_accounting() {
    let (mut snapshot, _, root) = support::bootstrap();
    let (_, target) = support::add_laptop(&mut snapshot, &root, Permissions::default(), false, 2);
    for _ in 0..25 {
        let denial = snapshot
            .plan_revocation(RevocationId::random().unwrap(), &root, &target, 3)
            .unwrap();
        snapshot.revocations.push(denial);
    }
    mock_http::seed(&snapshot);
    for denial in &snapshot.revocations {
        let mut payload = serde_json::to_string(denial).unwrap();
        payload.extend(std::iter::repeat_n(' ', 670_000 - payload.len()));
        replace_encoding("revocation", &denial.id.to_string(), &payload);
    }
    let mut store = adapter(&snapshot);
    let initial = store.snapshot().unwrap();
    assert_eq!(initial.revocations.len(), 25);
    let permissions = Permissions {
        deployments: (0..1024)
            .map(|i| format!("a{:058}{i:04}/e{:049}", 0, 0).parse().unwrap())
            .collect(),
        ..Permissions::default()
    };
    let (_, candidate_id) = support::add_laptop(&mut snapshot, &root, permissions, false, 4);
    let candidate = snapshot
        .members
        .iter()
        .find(|m| m.device_id == candidate_id)
        .unwrap();
    assert!(serde_json::to_vec(candidate).unwrap().len() < reviewed_adapter::MAX_RECORD_BYTES);
    assert!(
        serde_json::to_vec(&snapshot).unwrap().len() < reviewed_adapter::MAX_REGISTRY_BYTES / 2
    );
    let before = mock_http::count();
    let result = store.append_member(candidate);
    result.expect("canonical admission budget must agree with projection accounting");
    assert_eq!(mock_http::count(), before + 1);
    let projected = store
        .snapshot()
        .expect("successful admission must remain readable");
    assert!(projected.members.contains(candidate));
    assert_eq!(projected.revocations.len(), 25);
}

#[test]
fn rereview_ack_digest_conflicts_and_metadata_mismatches_fail_closed() {
    let (mut snapshot, _, root) = support::bootstrap();
    let (_, target) = support::add_laptop(&mut snapshot, &root, Permissions::default(), false, 2);
    let server = support::add_server(&mut snapshot, &root, 3);
    let revocation = support::revoke(&mut snapshot, &root, &target, 4);
    let ack = Ack {
        revocation,
        server,
        boot: BootId::random().unwrap(),
    };
    let id = format!("{}:{}:{}", ack.revocation, ack.server, ack.boot);
    snapshot.acks.push(ack.clone());
    mock_http::seed(&snapshot);
    let payload = format!("{} ", serde_json::to_string(&ack).unwrap());
    raw_record("ack", &id, &payload);
    let mut store = adapter(&snapshot);
    assert!(matches!(
        store.snapshot(),
        Err(RuntimeError::Conflict("record ID"))
    ));
    assert!(matches!(
        store.append_ack(&ack),
        Err(RuntimeError::Conflict("record ID"))
    ));

    snapshot.acks.clear();
    mock_http::seed(&snapshot);
    let mut wrong = ack.clone();
    wrong.boot = BootId::random().unwrap();
    raw_record("ack", &id, &serde_json::to_string(&wrong).unwrap());
    assert!(matches!(
        store.snapshot(),
        Err(RuntimeError::Corrosion("ACK ID"))
    ));
    assert!(matches!(
        store.append_ack(&ack),
        Err(RuntimeError::Conflict("ACK ID"))
    ));
}

impl tako_network::corrosion::RegistryStore for CorrosionStore {
    fn snapshot(&mut self) -> RuntimeResult<RegistrySnapshot> {
        RegistryStore::snapshot(self)
    }
    fn append_member(&mut self, member: &Member) -> RuntimeResult<()> {
        RegistryStore::append_member(self, member)
    }
    fn append_revocation(&mut self, revocation: &Revocation) -> RuntimeResult<()> {
        RegistryStore::append_revocation(self, revocation)
    }
    fn append_ack(&mut self, ack: &Ack) -> RuntimeResult<()> {
        RegistryStore::append_ack(self, ack)
    }
}

thread_local! {
    static REPLACE_ON_SCAN: std::cell::RefCell<Option<(usize, RegistrySnapshot)>> = const { std::cell::RefCell::new(None) };
}

fn replace_on_scan(path: &str, body: &Value) -> RuntimeResult<Vec<u8>> {
    if path == "/v1/queries" && body[0].as_str().unwrap().contains("ORDER BY kind DESC") {
        let replacement = REPLACE_ON_SCAN.with(|state| {
            let mut state = state.borrow_mut();
            let (remaining, _) = state.as_mut()?;
            *remaining -= 1;
            if *remaining == 0 {
                state.take().map(|(_, snapshot)| snapshot)
            } else {
                None
            }
        });
        if let Some(snapshot) = replacement {
            mock_http::seed(&snapshot);
        }
    }
    mock_http::post(path, body)
}

#[test]
fn nested_member_publication_capacity_stays_latched_after_rollback() {
    for failing_scan in [2, 3] {
        use tako_network::api::{ApproveRequest, RpcRequest, RpcResponse, SignedRequest};
        use tako_network::runtime::{RuntimeConfig, RuntimeLimits, ServerRuntime};

        let (mut original, admin, root) = support::bootstrap();
        let (victim, victim_id) = support::add_laptop(
            &mut original,
            &root,
            Permissions {
                ssh_user: Some("app".parse().unwrap()),
                ..Permissions::default()
            },
            false,
            2,
        );
        let dir = private_tempdir();
        let server_keys = LocalDeviceKeys::generate().unwrap();
        server_keys.save_new(&dir.path().join("keys")).unwrap();
        let server = original
            .approve_independent_admin(
                support::verified(&server_keys, &original.network.id, DeviceKind::Server, 3),
                Approval {
                    kind: MemberKind::Server(ServerInfo {
                        wireguard_endpoint: "203.0.113.7:51820".parse().unwrap(),
                        ssh_host_key: server_keys.ssh_public_key().unwrap(),
                        ssh_port: 22,
                        https_port: 443,
                        https_certificate_pem: Some(support::certificate_pem()),
                        corrosion_ca_certificate_pem: None,
                        https_spki_sha256: None,
                    }),
                    permissions: support::admin(),
                    approved_by: None,
                    independent_admin: true,
                    created_at: 3,
                    expires_at: None,
                },
            )
            .unwrap();
        let server_id = server.device_id.clone();
        original.members.push(server);
        let config = RuntimeConfig {
            network: original.network.clone(),
            server_id: server_id.clone(),
            boot_id: BootId::random().unwrap(),
            limits: RuntimeLimits::default(),
        };
        mock_http::seed(&original);
        let mut store = adapter(&original);
        store.transport = Some(replace_on_scan);
        let mut runtime = ServerRuntime::open(
            store,
            server_keys,
            config.clone(),
            &dir.path().join("runtime.db"),
        )
        .unwrap();
        let call = |runtime: &mut ServerRuntime<CorrosionStore>,
                    keys: &LocalDeviceKeys,
                    request: RpcRequest,
                    now: u64| {
            let path = request.path();
            runtime.handle(
                SignedRequest::sign(
                    keys,
                    original.network.id.clone(),
                    server_id.clone(),
                    DeviceKind::Laptop,
                    now,
                    request,
                )
                .unwrap(),
                "POST",
                path,
                now,
            )
        };
        let applicant = LocalDeviceKeys::generate().unwrap();
        let grant = ApproveRequest {
            pending_id: RequestId::random().unwrap(),
            request: applicant
                .enrollment_request(None, DeviceKind::Laptop, "applicant".parse().unwrap())
                .unwrap(),
            kind: MemberKind::Laptop,
            permissions: Permissions::default(),
            expires_at: 1000,
            member_expires_at: None,
        };
        call(
            &mut runtime,
            &admin,
            RpcRequest::Approve(Box::new(grant.clone())),
            10,
        )
        .unwrap();
        let RpcResponse::Challenge {
            challenge_id,
            challenge,
        } = call(
            &mut runtime,
            &applicant,
            RpcRequest::Begin {
                pending_id: grant.pending_id.clone(),
            },
            11,
        )
        .unwrap()
        else {
            panic!("expected challenge")
        };
        let mut oversized = original.clone();
        let denial = original
            .plan_revocation(RevocationId::random().unwrap(), &root, &victim_id, 12)
            .unwrap();
        for i in 0..10_000 {
            let mut denial = denial.clone();
            denial.id = format!("{i:032x}").parse().unwrap();
            oversized.revocations.push(denial);
        }
        REPLACE_ON_SCAN.with(|state| *state.borrow_mut() = Some((failing_scan, oversized)));
        let proof = applicant
            .answer_challenge(&challenge, &grant.request, 12)
            .unwrap();
        assert!(matches!(
            call(
                &mut runtime,
                &applicant,
                RpcRequest::Complete {
                    pending_id: grant.pending_id,
                    challenge_id,
                    proof
                },
                12
            ),
            Err(RuntimeError::Capacity("registry projection"))
        ));
        assert!(REPLACE_ON_SCAN.with(|state| state.borrow().is_none()));
        assert_eq!(mock_http::count(), 10_004);

        mock_http::seed(&original);
        assert!(matches!(
            runtime.desired_host_state(13),
            Err(RuntimeError::Capacity(_))
        ));
        assert!(matches!(
            call(&mut runtime, &victim, RpcRequest::Snapshot, 13),
            Err(RuntimeError::Capacity(_))
        ));
        drop(runtime);
        assert!(matches!(
            ServerRuntime::open(
                adapter(&original),
                LocalDeviceKeys::load(&dir.path().join("keys")).unwrap(),
                config,
                &dir.path().join("runtime.db")
            ),
            Err(RuntimeError::Capacity(_))
        ));
    }
}
