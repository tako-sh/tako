//! Exercise actual CLI processes against the signed HTTPS enrollment/secret API.
use crate::support::*;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, UdpSocket};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tako_network::api::{RpcResponse, RuntimeResult, SignedRequest};
use tako_network::corrosion::RegistryStore;
use tako_network::runtime::{RuntimeConfig, RuntimeLimits, ServerRuntime};
use tako_network::shared::{SecretGrant, SecretRecord, SharedSnapshot, SharedStore};
use tako_network::*;

#[derive(Clone)]
struct Store(Arc<Mutex<(RegistrySnapshot, SharedSnapshot)>>);
impl RegistryStore for Store {
    fn snapshot(&mut self) -> RuntimeResult<RegistrySnapshot> {
        Ok(self.0.lock().unwrap().0.clone())
    }
    fn append_member(&mut self, value: &Member) -> RuntimeResult<()> {
        let mut state = self.0.lock().unwrap();
        if !state.0.members.contains(value) {
            state.0.members.push(value.clone());
        }
        Ok(())
    }
    fn append_revocation(&mut self, value: &Revocation) -> RuntimeResult<()> {
        let mut state = self.0.lock().unwrap();
        if !state.0.revocations.contains(value) {
            state.0.revocations.push(value.clone());
        }
        Ok(())
    }
    fn append_ack(&mut self, value: &Ack) -> RuntimeResult<()> {
        self.0.lock().unwrap().0.acks.push(value.clone());
        Ok(())
    }
    fn shared_snapshot(&mut self) -> RuntimeResult<SharedSnapshot> {
        Ok(self.0.lock().unwrap().1.clone())
    }
    fn append_secret(&mut self, value: &SecretRecord) -> RuntimeResult<()> {
        SharedStore::append_secret(&mut self.0.lock().unwrap().1, value)
    }
    fn append_secret_grant(&mut self, value: &SecretGrant) -> RuntimeResult<()> {
        SharedStore::append_secret_grant(&mut self.0.lock().unwrap().1, value)
    }
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
fn admit(
    snapshot: &mut RegistrySnapshot,
    keys: &LocalDeviceKeys,
    request: EnrollmentRequest,
    kind: MemberKind,
    parent: Option<DeviceId>,
) {
    let (challenge, pending) =
        WgChallenge::begin(&request, snapshot.network.id.clone(), now(), now() + 60).unwrap();
    let verified = pending
        .verify(
            &keys.answer_challenge(&challenge, &request, now()).unwrap(),
            now(),
        )
        .unwrap();
    let member = snapshot
        .approve(
            verified,
            Approval {
                kind,
                permissions: Permissions {
                    network_admin: true,
                    server_admin: true,
                    ..Default::default()
                },
                approved_by: parent,
                independent_admin: false,
                created_at: now(),
                expires_at: None,
            },
        )
        .unwrap();
    snapshot.members.push(member);
}
fn protected_directory(path: &Path) {
    fs::create_dir_all(path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
}

pub(super) struct ApiServer {
    directory: TempDir,
    pub(super) owner_home: PathBuf,
    pub(super) network: NetworkId,
    store: Store,
    stop: Arc<AtomicBool>,
    drop_publication: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl ApiServer {
    pub(super) fn new() -> Self {
        let directory = TempDir::new().unwrap();
        protected_directory(directory.path());
        let listener = TcpListener::bind(("0.0.0.0", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let route = UdpSocket::bind("0.0.0.0:0").unwrap();
        route.connect("192.0.2.1:80").unwrap();
        let ip = route.local_addr().unwrap().ip();
        let network = NetworkId::random().unwrap();
        let owner = LocalDeviceKeys::generate().unwrap();
        let request = owner
            .enrollment_request(
                Some(network.clone()),
                DeviceKind::Laptop,
                "owner".parse().unwrap(),
            )
            .unwrap();
        let owner_id = request.device_id_for(&network).unwrap();
        let mut snapshot = RegistrySnapshot {
            network: NetworkDescriptor {
                id: network.clone(),
                name: "fleet".parse().unwrap(),
                initial_admin: owner_id.clone(),
            },
            members: vec![],
            revocations: vec![],
            acks: vec![],
        };
        admit(
            &mut snapshot,
            &owner,
            request.clone(),
            MemberKind::Laptop,
            None,
        );
        let server = LocalDeviceKeys::generate().unwrap();
        let server_request = server
            .enrollment_request(
                Some(network.clone()),
                DeviceKind::Server,
                "edge".parse().unwrap(),
            )
            .unwrap();
        let server_id = server_request.device_id_for(&network).unwrap();
        let hostname = format!(
            "node-{}.{}.tako.internal",
            &server_id.to_string()[..32],
            network
        );
        let certified = rcgen::generate_simple_self_signed(vec![hostname]).unwrap();
        let certificate = certified.cert.pem();
        let info = ServerInfo {
            wireguard_endpoint: std::net::SocketAddr::new(ip, 51820),
            ssh_host_key: server.ssh_public_key().unwrap(),
            ssh_port: 22,
            https_port: listener.local_addr().unwrap().port(),
            https_certificate_pem: Some(certificate.clone()),
            corrosion_ca_certificate_pem: None,
            https_spki_sha256: None,
        };
        admit(
            &mut snapshot,
            &server,
            server_request,
            MemberKind::Server(info.clone()),
            Some(owner_id.clone()),
        );
        let owner_home = directory.path().join("owner-home");
        let key_directory = owner_home.join("network-keys");
        protected_directory(&key_directory);
        owner
            .save_new(&key_directory.join(format!("{}.json", request.request_id)))
            .unwrap();
        let profile_path = owner_home
            .join("networks")
            .join(network.to_string())
            .join(owner_id.to_string())
            .join("profile.json");
        protected_directory(profile_path.parent().unwrap());
        let profile = NetworkPublicProfile {
            request,
            trusted_servers: BTreeMap::from([(server_id.clone(), info)]),
            snapshot: snapshot.clone(),
        };
        fs::write(
            profile_path,
            serde_json::to_vec(&serde_json::json!({"state":"active","profile":profile})).unwrap(),
        )
        .unwrap();
        fs::write(
            owner_home.join("networks/identities.json"),
            serde_json::to_vec(&serde_json::json!([{"id":network,"device":owner_id}])).unwrap(),
        )
        .unwrap();
        let store = Store(Arc::new(Mutex::new((
            snapshot.clone(),
            SharedSnapshot::empty(network.clone()),
        ))));
        let stop = Arc::new(AtomicBool::new(false));
        let drop_publication = Arc::new(AtomicBool::new(false));
        let worker = {
            let stop = stop.clone();
            let store = store.clone();
            let drop_publication = drop_publication.clone();
            let db = directory.path().join("runtime.db");
            std::thread::spawn(move || {
                use openssl::ssl::{SslAcceptor, SslMethod};
                let mut tls = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
                tls.set_private_key(
                    &openssl::pkey::PKey::private_key_from_pem(
                        certified.signing_key.serialize_pem().as_bytes(),
                    )
                    .unwrap(),
                )
                .unwrap();
                tls.set_certificate(
                    &openssl::x509::X509::from_pem(certificate.as_bytes()).unwrap(),
                )
                .unwrap();
                let tls = tls.build();
                let mut runtime = ServerRuntime::open(
                    store,
                    server,
                    RuntimeConfig {
                        network: snapshot.network,
                        server_id,
                        boot_id: BootId::random().unwrap(),
                        limits: RuntimeLimits::default(),
                    },
                    &db,
                )
                .unwrap();
                while !stop.load(Ordering::Relaxed) {
                    let stream = match listener.accept() {
                        Ok((stream, _)) => stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        Err(error) => panic!("{error}"),
                    };
                    stream
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    let Ok(mut stream) = tls.accept(stream) else {
                        continue;
                    };
                    let mut headers = Vec::new();
                    while !headers.ends_with(b"\r\n\r\n") && headers.len() < 16 * 1024 {
                        let mut byte = [0];
                        if stream.read_exact(&mut byte).is_err() {
                            break;
                        }
                        headers.push(byte[0]);
                    }
                    let headers = String::from_utf8(headers).unwrap();
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.split_once(':')
                                .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                                .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap();
                    assert!(length <= tako_network::api::MAX_RPC_BYTES);
                    let mut bytes = vec![0; length];
                    stream.read_exact(&mut bytes).unwrap();
                    let signed = SignedRequest::from_slice(&bytes).unwrap();
                    let path = signed.request.path();
                    let result = runtime.handle(signed, "POST", path, now());
                    if matches!(&result, Ok(RpcResponse::SecretPublished(_)))
                        && drop_publication.swap(false, Ordering::Relaxed)
                    {
                        continue;
                    }
                    let (status, body) = match result {
                        Ok(value) => (200, serde_json::to_vec(&value).unwrap()),
                        Err(_) => (403, b"{}".to_vec()),
                    };
                    let response = format!(
                        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nX-Tako-Network-Ready: true\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.write_all(&body);
                    let _ = stream.flush();
                }
            })
        };
        Self {
            directory,
            owner_home,
            network,
            store,
            stop,
            drop_publication,
            worker: Some(worker),
        }
    }
    pub(super) fn project(&self, name: &str) -> PathBuf {
        let path = self.directory.path().join(name);
        fs::create_dir_all(&path).unwrap();
        fs::write(
            path.join("tako.toml"),
            format!(
                "name='demo'\n[envs.production]\nnetwork='{}'\nroute='demo.example.test'\n",
                self.network
            ),
        )
        .unwrap();
        path
    }
    pub(super) fn run(
        &self,
        home: &Path,
        project: &Path,
        args: &[&str],
        input: Option<&str>,
    ) -> std::process::Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_tako"));
        command
            .env("TAKO_HOME", home)
            .env("TAKO_TELEMETRY", "0")
            .env_remove("RUST_LOG")
            .current_dir(project)
            .args(["--ci", "--json"])
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        apply_coverage_env(&mut command);
        let mut child = command.spawn().unwrap();
        if let Some(input) = input {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
        } else {
            drop(child.stdin.take());
        }
        child.wait_with_output().unwrap()
    }
    pub(super) fn ok(
        &self,
        home: &Path,
        project: &Path,
        args: &[&str],
        input: Option<&str>,
    ) -> serde_json::Value {
        let result = self.run(home, project, args, input);
        assert!(result.status.success(), "{args:?}: {}", stderr_str(&result));
        serde_json::from_slice(&result.stdout).unwrap()
    }
}
impl Drop for ApiServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}

#[test]
fn invitations_and_worktree_secret_publication_use_real_cli_and_pinned_https() {
    let server = ApiServer::new();
    let owner = server.project("owner-checkout");
    server.ok(
        &server.owner_home,
        &owner,
        &["secrets", "set", "ALPHA", "--env", "production"],
        Some("initial-alpha"),
    );
    server.ok(
        &server.owner_home,
        &owner,
        &["secrets", "set", "BETA", "--env", "production"],
        Some("initial-beta"),
    );
    server.ok(
        &server.owner_home,
        &owner,
        &["credentials", "set", "postgres_url", "--env", "production"],
        Some("postgres://fixture:example@db.example.test/app"),
    );
    assert!(server.store.0.lock().unwrap().1.secrets.is_empty());
    server.ok(
        &server.owner_home,
        &owner,
        &["secrets", "sync", "--env", "production"],
        None,
    );
    let invitation = server.ok(
        &server.owner_home,
        &owner,
        &["network", "client", "add", "--admin", "--network", "fleet"],
        None,
    )["invitation"]
        .as_str()
        .unwrap()
        .to_string();
    let client_home = server.directory.path().join("client-home");
    let client = server.project("client-checkout");
    server.ok(
        &client_home,
        &client,
        &["network", "join", "--name", "second", "--invitation-stdin"],
        Some(&invitation),
    );
    let listed = server.ok(&client_home, &client, &["secrets", "list"], None);
    assert_eq!(listed["secrets"].as_array().unwrap().len(), 2);
    let replay_home = server.directory.path().join("replay-home");
    assert!(
        !server
            .run(
                &replay_home,
                &client,
                &["network", "join", "--invitation-stdin"],
                Some(&invitation)
            )
            .status
            .success()
    );
    server.ok(
        &client_home,
        &client,
        &["secrets", "set", "BETA", "--env", "production"],
        Some("client-draft"),
    );
    server.ok(
        &server.owner_home,
        &owner,
        &["secrets", "set", "ALPHA", "--env", "production"],
        Some("owner-edit"),
    );
    server.ok(
        &server.owner_home,
        &owner,
        &["secrets", "sync", "--env", "production"],
        None,
    );
    let before = server.store.0.lock().unwrap().1.secrets.len();
    server.drop_publication.store(true, Ordering::Relaxed);
    assert!(
        !server
            .run(
                &client_home,
                &client,
                &["secrets", "sync", "--env", "production"],
                None
            )
            .status
            .success()
    );
    // A lost response is recovered from the server state without a new revision.
    server.ok(
        &client_home,
        &client,
        &["secrets", "sync", "--env", "production"],
        None,
    );
    let state = server.store.0.lock().unwrap().1.clone();
    assert_eq!(state.secrets.len(), before + 1);
    let latest = state.latest().unwrap();
    let alpha = latest
        .values()
        .find(|record| record.address.name == "ALPHA")
        .unwrap();
    let beta = latest
        .values()
        .find(|record| record.address.name == "BETA")
        .unwrap();
    assert_eq!(alpha.revision, 2);
    assert_eq!(beta.revision, 2);
    // Removing a draft after an uncertain write must publish a tombstone after
    // recovering that write, rather than resurrecting the acknowledged value.
    server.ok(
        &client_home,
        &client,
        &["secrets", "set", "GAMMA", "--env", "production"],
        Some("uncertain"),
    );
    server.drop_publication.store(true, Ordering::Relaxed);
    assert!(
        !server
            .run(
                &client_home,
                &client,
                &["secrets", "sync", "--env", "production"],
                None
            )
            .status
            .success()
    );
    server.ok(
        &client_home,
        &client,
        &["secrets", "rm", "GAMMA", "--env", "production", "--yes"],
        None,
    );
    server.ok(
        &client_home,
        &client,
        &["secrets", "sync", "--env", "production"],
        None,
    );
    let state = server.store.0.lock().unwrap().1.clone();
    let latest = state.latest().unwrap();
    let gamma = latest
        .values()
        .find(|record| record.address.name == "GAMMA")
        .unwrap();
    assert!(gamma.value.is_none());
    assert_eq!(gamma.revision, 2);
    let untouched = server.project("new-worktree");
    server.ok(&client_home, &untouched, &["secrets", "list"], None);
    server.ok(
        &client_home,
        &untouched,
        &["secrets", "sync", "--env", "production"],
        None,
    );
    assert_eq!(
        server.store.0.lock().unwrap().1.secrets.len(),
        state.secrets.len()
    );
    let scoped_invitation = server.ok(
        &server.owner_home,
        &owner,
        &[
            "network",
            "client",
            "add",
            "--network",
            "fleet",
            "--deploy",
            "demo/production",
        ],
        None,
    )["invitation"]
        .as_str()
        .unwrap()
        .to_owned();
    let scoped_home = server.directory.path().join("scoped-home");
    let scoped_project = server.project("scoped-checkout");
    server.ok(
        &scoped_home,
        &scoped_project,
        &["network", "join", "--name", "scoped", "--invitation-stdin"],
        Some(&scoped_invitation),
    );
    let devices = fs::read_dir(
        scoped_home
            .join("networks")
            .join(server.network.to_string()),
    )
    .unwrap();
    let device = devices
        .map(|entry| entry.unwrap())
        .find(|entry| entry.path().is_dir())
        .unwrap();
    let cache: SharedSnapshot =
        serde_json::from_slice(&fs::read(device.path().join("secrets.json")).unwrap()).unwrap();
    assert!(
        cache
            .secrets
            .iter()
            .all(|record| record.address.kind == tako_network::shared::SecretKind::App)
    );
    server.ok(
        &scoped_home,
        &scoped_project,
        &["secrets", "set", "SCOPED", "--env", "production"],
        Some("allowed"),
    );
    server.ok(
        &scoped_home,
        &scoped_project,
        &["secrets", "sync", "--env", "production"],
        None,
    );
    let count = server.store.0.lock().unwrap().1.secrets.len();
    server.ok(
        &scoped_home,
        &scoped_project,
        &["credentials", "set", "postgres_url", "--env", "production"],
        Some("postgres://fixture:example@db.example.test/forbidden"),
    );
    assert!(
        !server
            .run(
                &scoped_home,
                &scoped_project,
                &["secrets", "sync", "--env", "production"],
                None
            )
            .status
            .success()
    );
    assert_eq!(server.store.0.lock().unwrap().1.secrets.len(), count);
    server.ok(
        &server.owner_home,
        &owner,
        &[
            "network",
            "client",
            "remove",
            "second",
            "--network",
            "fleet",
        ],
        None,
    );
    assert!(
        !server
            .run(&client_home, &client, &["network", "sync", "fleet"], None)
            .status
            .success()
    );
}
