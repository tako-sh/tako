#![allow(dead_code)]

mod support;

use std::{cell::RefCell, rc::Rc};
use tako_network::{
    api::{RpcRequest, RpcResponse, RuntimeError, RuntimeResult, SignedRequest},
    corrosion::RegistryStore,
    runtime::{RuntimeConfig, RuntimeLimits, ServerRuntime},
    *,
};

#[derive(Clone)]
struct Store(Rc<RefCell<RegistrySnapshot>>);

impl RegistryStore for Store {
    fn snapshot(&mut self) -> RuntimeResult<RegistrySnapshot> {
        Ok(self.0.borrow().clone())
    }

    fn append_member(&mut self, member: &Member) -> RuntimeResult<()> {
        let mut snapshot = self.0.borrow_mut();
        if !snapshot.members.contains(member) {
            snapshot.members.push(member.clone());
        }
        Ok(())
    }

    fn append_revocation(&mut self, revocation: &Revocation) -> RuntimeResult<()> {
        let mut snapshot = self.0.borrow_mut();
        if !snapshot.revocations.contains(revocation) {
            snapshot.revocations.push(revocation.clone());
        }
        Ok(())
    }

    fn append_ack(&mut self, ack: &Ack) -> RuntimeResult<()> {
        self.0.borrow_mut().acks.push(ack.clone());
        Ok(())
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    store: Store,
    admin: LocalDeviceKeys,
    laptop: LocalDeviceKeys,
    laptop_id: DeviceId,
    config: RuntimeConfig,
}

impl Fixture {
    fn new(expiry: Option<u64>) -> Self {
        let (mut snapshot, admin, admin_id) = support::bootstrap();
        let dir = tempfile::tempdir().unwrap();
        let server_keys = LocalDeviceKeys::generate().unwrap();
        server_keys.save_new(&dir.path().join("keys")).unwrap();
        let server_id = server_keys
            .device_id(&snapshot.network.id, DeviceKind::Server)
            .unwrap();
        let server = snapshot
            .approve_independent_admin(
                support::verified(&server_keys, &snapshot.network.id, DeviceKind::Server, 2),
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
                    created_at: 2,
                    expires_at: None,
                },
            )
            .unwrap();
        snapshot.members.push(server);
        let laptop = LocalDeviceKeys::generate().unwrap();
        let laptop_id = laptop
            .device_id(&snapshot.network.id, DeviceKind::Laptop)
            .unwrap();
        let mut approval = support::approval(
            MemberKind::Laptop,
            Permissions {
                ssh_user: Some("app".parse().unwrap()),
                ..Permissions::default()
            },
            Some(admin_id),
            3,
        );
        approval.expires_at = expiry;
        let member = snapshot
            .approve(
                support::verified(&laptop, &snapshot.network.id, DeviceKind::Laptop, 3),
                approval,
            )
            .unwrap();
        snapshot.members.push(member);
        let config = RuntimeConfig {
            network: snapshot.network.clone(),
            server_id,
            boot_id: BootId::random().unwrap(),
            limits: RuntimeLimits::default(),
        };
        Self {
            dir,
            store: Store(Rc::new(RefCell::new(snapshot))),
            admin,
            laptop,
            laptop_id,
            config,
        }
    }

    fn open(&self) -> ServerRuntime<Store> {
        ServerRuntime::open(
            self.store.clone(),
            LocalDeviceKeys::load(&self.dir.path().join("keys")).unwrap(),
            self.config.clone(),
            &self.dir.path().join("runtime.db"),
        )
        .unwrap()
    }

    fn call<S: RegistryStore>(
        &self,
        runtime: &mut ServerRuntime<S>,
        keys: &LocalDeviceKeys,
        request: RpcRequest,
        timestamp: u64,
        now: u64,
    ) -> RuntimeResult<RpcResponse> {
        let path = request.path();
        runtime.handle(
            SignedRequest::sign(
                keys,
                self.config.network.id.clone(),
                self.config.server_id.clone(),
                DeviceKind::Laptop,
                timestamp,
                request,
            )?,
            "POST",
            path,
            now,
        )
    }
}

#[test]
fn observed_revocation_survives_store_rollback_and_reopen() {
    for restart in [false, true] {
        let fixture = Fixture::new(None);
        let before_revocation = fixture.store.0.borrow().clone();
        let mut runtime = fixture.open();
        fixture
            .call(
                &mut runtime,
                &fixture.admin,
                RpcRequest::Revoke {
                    id: RevocationId::random().unwrap(),
                    target: fixture.laptop_id.clone(),
                },
                10,
                10,
            )
            .unwrap();
        let denied = runtime.desired_host_state(11).unwrap();
        assert!(
            !denied
                .peers
                .iter()
                .any(|p| p.device_id == fixture.laptop_id)
        );
        runtime.acknowledge_applied(&denied.snapshot).unwrap();
        assert!(matches!(
            fixture.call(&mut runtime, &fixture.laptop, RpcRequest::Snapshot, 11, 11),
            Err(RuntimeError::Denied)
        ));

        *fixture.store.0.borrow_mut() = before_revocation;
        if restart {
            drop(runtime);
            runtime = fixture.open();
        }
        let restored = runtime.desired_host_state(12).unwrap();
        assert!(
            !restored
                .peers
                .iter()
                .any(|p| p.device_id == fixture.laptop_id)
        );
        assert!(
            !restored
                .ssh_authorizations
                .iter()
                .any(|p| p.device_id == fixture.laptop_id)
        );
        assert!(matches!(
            fixture.call(&mut runtime, &fixture.laptop, RpcRequest::Snapshot, 12, 12),
            Err(RuntimeError::Denied)
        ));
    }
}

#[test]
fn host_projection_enforces_persisted_clock_floor() {
    for restart in [false, true] {
        let fixture = Fixture::new(Some(30));
        let mut runtime = fixture.open();
        fixture
            .call(&mut runtime, &fixture.admin, RpcRequest::Snapshot, 40, 40)
            .unwrap();
        let expired = runtime.desired_host_state(40).unwrap();
        assert!(
            !expired
                .peers
                .iter()
                .any(|p| p.device_id == fixture.laptop_id)
        );
        if restart {
            drop(runtime);
            runtime = fixture.open();
        }
        assert!(matches!(
            fixture.call(&mut runtime, &fixture.admin, RpcRequest::Snapshot, 20, 20),
            Err(RuntimeError::Invalid("server clock moved backwards"))
        ));
        assert!(matches!(
            runtime.desired_host_state(20),
            Err(RuntimeError::Invalid("server clock moved backwards"))
        ));
    }
}

#[test]
fn member_replay_quota_reserves_revocation_capacity() {
    for budget in [1, 3, 16] {
        let mut fixture = Fixture::new(None);
        fixture.config.limits.max_replays = budget;
        let mut runtime = fixture.open();
        fixture
            .call(&mut runtime, &fixture.laptop, RpcRequest::Snapshot, 130, 10)
            .unwrap();
        assert!(matches!(
            fixture.call(&mut runtime, &fixture.laptop, RpcRequest::Snapshot, 130, 10),
            Err(RuntimeError::Capacity("replays"))
        ));
        drop(runtime);
        let mut runtime = fixture.open();
        assert!(matches!(
            fixture.call(
                &mut runtime,
                &fixture.admin,
                RpcRequest::Revoke {
                    id: RevocationId::random().unwrap(),
                    target: fixture.laptop_id.clone(),
                },
                10,
                10
            ),
            Ok(RpcResponse::Revoked(_))
        ));
        assert!(
            !runtime
                .desired_host_state(10)
                .unwrap()
                .peers
                .iter()
                .any(|p| p.device_id == fixture.laptop_id)
        );
    }
}

#[test]
fn projection_alone_advances_clock_across_reopen() {
    let fixture = Fixture::new(Some(30));
    let mut runtime = fixture.open();
    assert!(
        !runtime
            .desired_host_state(40)
            .unwrap()
            .peers
            .iter()
            .any(|p| p.device_id == fixture.laptop_id)
    );
    drop(runtime);
    let mut runtime = fixture.open();
    assert!(matches!(
        runtime.desired_host_state(20),
        Err(RuntimeError::Invalid("server clock moved backwards"))
    ));
    assert!(matches!(
        fixture.call(&mut runtime, &fixture.admin, RpcRequest::Snapshot, 20, 20),
        Err(RuntimeError::Invalid("server clock moved backwards"))
    ));
}

#[test]
fn replay_quotas_never_evict_unexpired_nonces() {
    let mut fixture = Fixture::new(None);
    fixture.config.limits.max_replays = 3;
    let mut runtime = fixture.open();
    let envelope = SignedRequest::sign(
        &fixture.admin,
        fixture.config.network.id.clone(),
        fixture.config.server_id.clone(),
        DeviceKind::Laptop,
        130,
        RpcRequest::Snapshot,
    )
    .unwrap();
    runtime
        .handle(envelope.clone(), "POST", RpcRequest::Snapshot.path(), 10)
        .unwrap();
    fixture
        .call(&mut runtime, &fixture.laptop, RpcRequest::Snapshot, 130, 10)
        .unwrap();
    drop(runtime);
    let mut runtime = fixture.open();
    for now in [10, 250] {
        assert!(matches!(
            runtime.handle(envelope.clone(), "POST", RpcRequest::Snapshot.path(), now),
            Err(RuntimeError::Replay)
        ));
        assert!(matches!(
            fixture.call(&mut runtime, &fixture.admin, RpcRequest::Snapshot, now, now),
            Err(RuntimeError::Capacity("replays"))
        ));
    }
    fixture
        .call(&mut runtime, &fixture.admin, RpcRequest::Snapshot, 251, 251)
        .unwrap();
}

#[test]
fn observed_authority_cannot_be_replaced_after_reopen() {
    let fixture = Fixture::new(None);
    drop(fixture.open());
    fixture.store.0.borrow_mut().members[2]
        .permissions
        .network_admin = true;
    assert!(matches!(
        ServerRuntime::open(
            fixture.store.clone(),
            LocalDeviceKeys::load(&fixture.dir.path().join("keys")).unwrap(),
            fixture.config.clone(),
            &fixture.dir.path().join("runtime.db")
        ),
        Err(RuntimeError::Conflict("observed registry record"))
    ));
}

#[test]
fn oversized_store_failure_remains_closed_after_rollback_and_reopen() {
    let fixture = Fixture::new(None);
    let original = fixture.store.0.borrow().clone();
    let mut runtime = fixture.open();
    let member = original.members[2].clone();
    fixture.store.0.borrow_mut().members =
        vec![member; tako_network::corrosion::MAX_REGISTRY_ROWS + 1];
    assert!(matches!(
        runtime.desired_host_state(10),
        Err(RuntimeError::Capacity(_))
    ));
    *fixture.store.0.borrow_mut() = original;
    assert!(matches!(
        runtime.desired_host_state(11),
        Err(RuntimeError::Capacity(_))
    ));
    drop(runtime);
    assert!(matches!(
        ServerRuntime::open(
            fixture.store.clone(),
            LocalDeviceKeys::load(&fixture.dir.path().join("keys")).unwrap(),
            fixture.config.clone(),
            &fixture.dir.path().join("runtime.db")
        ),
        Err(RuntimeError::Capacity(_))
    ));
}

#[test]
fn revocation_receipt_is_durable_before_any_projection_or_ack() {
    let fixture = Fixture::new(None);
    let original = fixture.store.0.borrow().clone();
    let mut runtime = fixture.open();
    fixture
        .call(
            &mut runtime,
            &fixture.admin,
            RpcRequest::Revoke {
                id: RevocationId::random().unwrap(),
                target: fixture.laptop_id.clone(),
            },
            10,
            10,
        )
        .unwrap();
    *fixture.store.0.borrow_mut() = original;
    drop(runtime);
    let mut runtime = fixture.open();
    assert!(
        !runtime
            .desired_host_state(11)
            .unwrap()
            .peers
            .iter()
            .any(|p| p.device_id == fixture.laptop_id)
    );
    assert!(matches!(
        fixture.call(&mut runtime, &fixture.laptop, RpcRequest::Snapshot, 11, 11),
        Err(RuntimeError::Denied)
    ));
}

#[test]
fn failed_denial_publication_retains_local_intent_and_retries_after_reopen() {
    struct Unavailable(Store);
    impl RegistryStore for Unavailable {
        fn snapshot(&mut self) -> RuntimeResult<RegistrySnapshot> {
            self.0.snapshot()
        }
        fn append_member(&mut self, member: &Member) -> RuntimeResult<()> {
            self.0.append_member(member)
        }
        fn append_revocation(&mut self, _: &Revocation) -> RuntimeResult<()> {
            Err(RuntimeError::Transport)
        }
        fn append_ack(&mut self, ack: &Ack) -> RuntimeResult<()> {
            self.0.append_ack(ack)
        }
    }
    let fixture = Fixture::new(None);
    let mut runtime = ServerRuntime::open(
        Unavailable(fixture.store.clone()),
        LocalDeviceKeys::load(&fixture.dir.path().join("keys")).unwrap(),
        fixture.config.clone(),
        &fixture.dir.path().join("runtime.db"),
    )
    .unwrap();
    let request = RpcRequest::Revoke {
        id: RevocationId::random().unwrap(),
        target: fixture.laptop_id.clone(),
    };
    let signed = SignedRequest::sign(
        &fixture.admin,
        fixture.config.network.id.clone(),
        fixture.config.server_id.clone(),
        DeviceKind::Laptop,
        10,
        request.clone(),
    )
    .unwrap();
    assert!(matches!(
        runtime.handle(signed, "POST", request.path(), 10),
        Err(RuntimeError::Transport)
    ));
    assert!(fixture.store.0.borrow().revocations.is_empty());
    drop(runtime);
    let mut runtime = fixture.open();
    assert!(
        !runtime
            .desired_host_state(11)
            .unwrap()
            .peers
            .iter()
            .any(|p| p.device_id == fixture.laptop_id)
    );
    assert!(matches!(
        fixture.call(&mut runtime, &fixture.admin, request, 11, 11),
        Ok(RpcResponse::Revoked(_))
    ));
    assert_eq!(fixture.store.0.borrow().revocations.len(), 1);
}

#[derive(Clone, Copy)]
enum StoreFailure {
    AdmissionQuota,
    CandidateBytes,
    RevocationHistory,
    AckHistory,
}

struct FailingStore(Store, StoreFailure);

impl RegistryStore for FailingStore {
    fn snapshot(&mut self) -> RuntimeResult<RegistrySnapshot> {
        self.0.snapshot()
    }
    fn append_member(&mut self, member: &Member) -> RuntimeResult<()> {
        match self.1 {
            StoreFailure::AdmissionQuota => Err(RuntimeError::Capacity("registry admissions")),
            StoreFailure::CandidateBytes => Err(RuntimeError::Capacity("registry record")),
            _ => self.0.append_member(member),
        }
    }
    fn append_revocation(&mut self, revocation: &Revocation) -> RuntimeResult<()> {
        match self.1 {
            StoreFailure::RevocationHistory => Err(RuntimeError::Capacity("registry projection")),
            _ => self.0.append_revocation(revocation),
        }
    }
    fn append_ack(&mut self, ack: &Ack) -> RuntimeResult<()> {
        match self.1 {
            StoreFailure::AckHistory => Err(RuntimeError::Capacity("registry stored record")),
            _ => self.0.append_ack(ack),
        }
    }
}

#[test]
fn candidate_capacity_rejection_does_not_latch_the_runtime() {
    for fault in [StoreFailure::AdmissionQuota, StoreFailure::CandidateBytes] {
        let fixture = Fixture::new(None);
        let mut runtime = ServerRuntime::open(
            FailingStore(fixture.store.clone(), fault),
            LocalDeviceKeys::load(&fixture.dir.path().join("keys")).unwrap(),
            fixture.config.clone(),
            &fixture.dir.path().join("runtime.db"),
        )
        .unwrap();
        let applicant = LocalDeviceKeys::generate().unwrap();
        let grant = tako_network::api::ApproveRequest {
            pending_id: RequestId::random().unwrap(),
            request: applicant
                .enrollment_request(None, DeviceKind::Laptop, "applicant".parse().unwrap())
                .unwrap(),
            kind: MemberKind::Laptop,
            permissions: Permissions::default(),
            expires_at: 1000,
            member_expires_at: None,
        };
        fixture
            .call(
                &mut runtime,
                &fixture.admin,
                RpcRequest::Approve(Box::new(grant.clone())),
                10,
                10,
            )
            .unwrap();
        let RpcResponse::Challenge {
            challenge_id,
            challenge,
        } = fixture
            .call(
                &mut runtime,
                &applicant,
                RpcRequest::Begin {
                    pending_id: grant.pending_id.clone(),
                },
                11,
                11,
            )
            .unwrap()
        else {
            panic!("expected challenge")
        };
        let proof = applicant
            .answer_challenge(&challenge, &grant.request, 12)
            .unwrap();
        assert!(matches!(
            fixture.call(
                &mut runtime,
                &applicant,
                RpcRequest::Complete {
                    pending_id: grant.pending_id,
                    challenge_id,
                    proof,
                },
                12,
                12
            ),
            Err(RuntimeError::Capacity(
                "registry admissions" | "registry record"
            ))
        ));
        assert!(
            runtime
                .desired_host_state(13)
                .unwrap()
                .peers
                .iter()
                .any(|p| p.device_id == fixture.laptop_id)
        );
        drop(runtime);
        let mut runtime = fixture.open();
        fixture
            .call(&mut runtime, &fixture.admin, RpcRequest::Snapshot, 14, 14)
            .unwrap();
    }
}

#[test]
fn hard_capacity_in_denial_or_ack_publication_latches_before_rollback() {
    for fault in [StoreFailure::RevocationHistory, StoreFailure::AckHistory] {
        let fixture = Fixture::new(None);
        let original = fixture.store.0.borrow().clone();
        let mut runtime = ServerRuntime::open(
            FailingStore(fixture.store.clone(), fault),
            LocalDeviceKeys::load(&fixture.dir.path().join("keys")).unwrap(),
            fixture.config.clone(),
            &fixture.dir.path().join("runtime.db"),
        )
        .unwrap();
        let command = RpcRequest::Revoke {
            id: RevocationId::random().unwrap(),
            target: fixture.laptop_id.clone(),
        };
        let revoke = fixture.call(&mut runtime, &fixture.admin, command.clone(), 10, 10);
        match fault {
            StoreFailure::RevocationHistory => {
                assert!(matches!(revoke, Err(RuntimeError::Capacity(_))))
            }
            StoreFailure::AckHistory => {
                revoke.unwrap();
                let applied = runtime.desired_host_state(11).unwrap().snapshot;
                assert!(matches!(
                    runtime.acknowledge_applied(&applied),
                    Err(RuntimeError::Capacity(_))
                ));
            }
            _ => unreachable!(),
        }
        *fixture.store.0.borrow_mut() = original;
        assert!(matches!(
            fixture.call(&mut runtime, &fixture.admin, command, 12, 12),
            Err(RuntimeError::Capacity(_))
        ));
        assert!(fixture.store.0.borrow().revocations.is_empty());
        assert!(matches!(
            runtime.desired_host_state(12),
            Err(RuntimeError::Capacity(_))
        ));
        assert!(matches!(
            fixture.call(&mut runtime, &fixture.laptop, RpcRequest::Snapshot, 12, 12),
            Err(RuntimeError::Capacity(_))
        ));
        drop(runtime);
        assert!(matches!(
            ServerRuntime::open(
                fixture.store.clone(),
                LocalDeviceKeys::load(&fixture.dir.path().join("keys")).unwrap(),
                fixture.config.clone(),
                &fixture.dir.path().join("runtime.db")
            ),
            Err(RuntimeError::Capacity(_))
        ));
    }
}

#[test]
fn rereview_full_ordinary_pool_still_allows_administrative_revocation() {
    let mut fixture = Fixture::new(None);
    fixture.config.limits.max_replays = 16;
    let root = fixture.config.network.initial_admin.clone();
    let others: Vec<_> = (0..15)
        .map(|_| {
            support::add_laptop(
                &mut fixture.store.0.borrow_mut(),
                &root,
                Permissions::default(),
                false,
                4,
            )
            .0
        })
        .collect();
    let mut runtime = fixture.open();
    for keys in std::iter::once(&fixture.laptop).chain(others.iter()) {
        fixture
            .call(&mut runtime, keys, RpcRequest::Snapshot, 130, 10)
            .unwrap();
    }
    assert!(matches!(
        fixture.call(&mut runtime, &fixture.admin, RpcRequest::Snapshot, 130, 10),
        Err(RuntimeError::Capacity("replays"))
    ));
    drop(runtime);
    let mut runtime = fixture.open();
    fixture
        .call(
            &mut runtime,
            &fixture.admin,
            RpcRequest::Revoke {
                id: RevocationId::random().unwrap(),
                target: fixture.laptop_id.clone(),
            },
            10,
            10,
        )
        .unwrap();
    assert!(matches!(
        fixture.call(
            &mut runtime,
            &fixture.admin,
            RpcRequest::Revoke {
                id: RevocationId::random().unwrap(),
                target: others[0]
                    .device_id(&fixture.config.network.id, DeviceKind::Laptop)
                    .unwrap()
            },
            10,
            10
        ),
        Err(RuntimeError::Capacity("replays"))
    ));
    let keys = LocalDeviceKeys::load(&fixture.dir.path().join("keys")).unwrap();
    let request = RpcRequest::Revoke {
        id: RevocationId::random().unwrap(),
        target: others[0]
            .device_id(&fixture.config.network.id, DeviceKind::Laptop)
            .unwrap(),
    };
    let path = request.path();
    let signed = SignedRequest::sign(
        &keys,
        fixture.config.network.id.clone(),
        fixture.config.server_id.clone(),
        DeviceKind::Server,
        10,
        request,
    )
    .unwrap();
    runtime.handle(signed, "POST", path, 10).unwrap();
    assert_eq!(fixture.store.0.borrow().revocations.len(), 2);
}

#[test]
fn failed_self_or_serving_server_denial_can_retry_without_authority() {
    struct FailPublication(Store);
    impl RegistryStore for FailPublication {
        fn snapshot(&mut self) -> RuntimeResult<RegistrySnapshot> {
            self.0.snapshot()
        }
        fn append_member(&mut self, member: &Member) -> RuntimeResult<()> {
            self.0.append_member(member)
        }
        fn append_revocation(&mut self, _: &Revocation) -> RuntimeResult<()> {
            Err(RuntimeError::Transport)
        }
        fn append_ack(&mut self, ack: &Ack) -> RuntimeResult<()> {
            self.0.append_ack(ack)
        }
    }
    for revoke_server in [false, true] {
        let fixture = Fixture::new(None);
        let target = if revoke_server {
            fixture.config.server_id.clone()
        } else {
            fixture.config.network.initial_admin.clone()
        };
        let request = RpcRequest::Revoke {
            id: RevocationId::random().unwrap(),
            target,
        };
        let mut runtime = ServerRuntime::open(
            FailPublication(fixture.store.clone()),
            LocalDeviceKeys::load(&fixture.dir.path().join("keys")).unwrap(),
            fixture.config.clone(),
            &fixture.dir.path().join("runtime.db"),
        )
        .unwrap();
        let signed = SignedRequest::sign(
            &fixture.admin,
            fixture.config.network.id.clone(),
            fixture.config.server_id.clone(),
            DeviceKind::Laptop,
            10,
            request.clone(),
        )
        .unwrap();
        assert!(matches!(
            runtime.handle(signed.clone(), "POST", request.path(), 10),
            Err(RuntimeError::Transport)
        ));
        assert_eq!(runtime.snapshot().unwrap().revocations.len(), 1);
        assert!(fixture.store.0.borrow().revocations.is_empty());
        drop(runtime);
        let mut runtime = fixture.open();
        let desired = runtime.desired_host_state(11).unwrap();
        assert!(desired.peers.is_empty());
        assert!(desired.ssh_authorizations.is_empty());
        for command in [
            RpcRequest::Snapshot,
            RpcRequest::Authorize {
                action: tako_network::api::ManagementAction::Rpc {
                    scope: Some("app/production".parse().unwrap()),
                    sha256: "ab".repeat(32),
                },
            },
            RpcRequest::Revoke {
                id: RevocationId::random().unwrap(),
                target: fixture.laptop_id.clone(),
            },
        ] {
            assert!(matches!(
                fixture.call(&mut runtime, &fixture.admin, command, 11, 11),
                Err(RuntimeError::Denied)
            ));
        }
        let RpcRequest::Revoke { id, .. } = &request else {
            unreachable!()
        };
        assert!(matches!(
            fixture.call(
                &mut runtime,
                &fixture.admin,
                RpcRequest::Revoke {
                    id: id.clone(),
                    target: fixture.laptop_id.clone(),
                },
                11,
                11
            ),
            Err(RuntimeError::Conflict("revocation ID"))
        ));
        assert!(
            fixture
                .call(&mut runtime, &fixture.laptop, request.clone(), 11, 11)
                .is_err()
        );
        let mut forged = SignedRequest::sign(
            &fixture.laptop,
            fixture.config.network.id.clone(),
            fixture.config.server_id.clone(),
            DeviceKind::Laptop,
            11,
            request.clone(),
        )
        .unwrap();
        forged.signer = fixture.config.network.initial_admin.clone();
        assert!(matches!(
            runtime.handle(forged, "POST", request.path(), 11),
            Err(RuntimeError::Authentication)
        ));
        assert!(matches!(
            runtime.handle(signed, "POST", request.path(), 11),
            Err(RuntimeError::Replay)
        ));
        assert!(fixture.store.0.borrow().revocations.is_empty());

        let retry = SignedRequest::sign(
            &fixture.admin,
            fixture.config.network.id.clone(),
            fixture.config.server_id.clone(),
            DeviceKind::Laptop,
            11,
            request.clone(),
        )
        .unwrap();
        let result = runtime
            .handle(retry.clone(), "POST", request.path(), 11)
            .unwrap();
        let RpcResponse::Revoked(denial) = &result else {
            panic!("expected stored denial")
        };
        assert_eq!(denial.created_at, 10);
        assert_eq!(fixture.store.0.borrow().revocations, vec![denial.clone()]);
        assert!(matches!(
            runtime.handle(retry, "POST", request.path(), 11),
            Err(RuntimeError::Replay)
        ));
        assert_eq!(
            fixture
                .call(&mut runtime, &fixture.admin, request, 12, 12)
                .unwrap(),
            result
        );
        assert_eq!(fixture.store.0.borrow().revocations.len(), 1);
        assert!(runtime.desired_host_state(12).unwrap().peers.is_empty());
        assert!(matches!(
            fixture.call(&mut runtime, &fixture.admin, RpcRequest::Snapshot, 12, 12),
            Err(RuntimeError::Denied)
        ));
    }
}

#[test]
fn snapshot_capacity_always_latches_even_with_a_candidate_quota_label() {
    struct FailingRead(Store, bool);
    impl RegistryStore for FailingRead {
        fn snapshot(&mut self) -> RuntimeResult<RegistrySnapshot> {
            if std::mem::take(&mut self.1) {
                self.0.snapshot()
            } else {
                Err(RuntimeError::Capacity("registry record"))
            }
        }
        fn append_member(&mut self, member: &Member) -> RuntimeResult<()> {
            self.0.append_member(member)
        }
        fn append_revocation(&mut self, revocation: &Revocation) -> RuntimeResult<()> {
            self.0.append_revocation(revocation)
        }
        fn append_ack(&mut self, ack: &Ack) -> RuntimeResult<()> {
            self.0.append_ack(ack)
        }
    }
    let fixture = Fixture::new(None);
    let mut runtime = ServerRuntime::open(
        FailingRead(fixture.store.clone(), true),
        LocalDeviceKeys::load(&fixture.dir.path().join("keys")).unwrap(),
        fixture.config.clone(),
        &fixture.dir.path().join("runtime.db"),
    )
    .unwrap();
    assert!(matches!(
        runtime.desired_host_state(10),
        Err(RuntimeError::Capacity("registry record"))
    ));
    assert!(matches!(
        runtime.desired_host_state(11),
        Err(RuntimeError::Capacity("registry requires local recovery"))
    ));
    drop(runtime);
    assert!(matches!(
        ServerRuntime::open(
            fixture.store.clone(),
            LocalDeviceKeys::load(&fixture.dir.path().join("keys")).unwrap(),
            fixture.config.clone(),
            &fixture.dir.path().join("runtime.db")
        ),
        Err(RuntimeError::Capacity("registry requires local recovery"))
    ));
}
