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
use tako_network::shared::{SharedRecord, SharedSnapshot, SharedStore};
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
    fn append_record(&mut self, value: &SharedRecord) -> RuntimeResult<()> {
        SharedStore::append_record(&mut self.0.lock().unwrap().1, value)
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
                permissions: Permissions::Full,
                approved_by: parent,
                independent_root: false,
                created_at: now(),
                expires_at: None,
                client: None,
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
                    stream.set_nonblocking(false).unwrap();
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
                    let (status, body) = if headers.starts_with("POST /v1/management/rpc ") {
                        let command: tako_core::Command = serde_json::from_slice(&bytes).unwrap();
                        assert!(
                            matches!(command, tako_core::Command::BackupNow {ref app, backup: None}
                            if app == &tako_core::deployment_app_id("demo", "production"))
                        );
                        let backup = serde_json::json!({
                            "id":"b1-test", "app":"demo", "environment":"production", "server":"edge",
                            "created_at_unix_secs":1, "size_bytes":10, "sha256_hex":"hash",
                            "archive_key":"archive", "manifest_key":"manifest", "encryption":{
                                "algorithm":"aes-256-gcm", "key_id":"fixture", "nonce_base64":"AA==", "tag_base64":"AA=="
                            }
                        });
                        (
                            200,
                            serde_json::to_vec(&tako_core::Response::ok(backup)).unwrap(),
                        )
                    } else {
                        let signed = SignedRequest::from_slice(&bytes).unwrap();
                        let path = signed.request.path();
                        let result = runtime.handle(signed, "POST", path, now());
                        if matches!(&result, Ok(RpcResponse::RecordPublished(_)))
                            && drop_publication.swap(false, Ordering::Relaxed)
                        {
                            continue;
                        }
                        match result {
                            Ok(value) => (200, serde_json::to_vec(&value).unwrap()),
                            Err(_) => (403, b"{}".to_vec()),
                        }
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
fn cached_network_secrets_do_not_block_legacy_key_recovery() {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

    let server = ApiServer::new();
    let publisher = server.project("publisher");
    server.ok(
        &server.owner_home,
        &publisher,
        &["secrets", "set", "REMOTE", "--env", "production"],
        Some("published"),
    );
    server.ok(
        &server.owner_home,
        &publisher,
        &["secrets", "sync", "--env", "production"],
        None,
    );
    for (name, passphrase, explicit_env) in [
        ("exported", false, true),
        ("inferred", false, false),
        ("passphrase", true, true),
    ] {
        let project = server.project(name);
        let config = fs::read(project.join("tako.toml")).unwrap();
        // Model a pre-migration checkout with a key originally stored elsewhere.
        fs::write(
            project.join("tako.toml"),
            "name='demo'\n[envs.production]\nroute='demo.example.test'\n",
        )
        .unwrap();
        if passphrase {
            server.ok(
                &server.owner_home,
                &project,
                &[
                    "secrets",
                    "key",
                    "import",
                    "--passphrase",
                    "--env",
                    "production",
                ],
                Some("recovery-test-passphrase"),
            );
        }
        server.ok(
            &server.owner_home,
            &project,
            &["secrets", "set", "LEGACY", "--env", "production"],
            Some("recovered-local"),
        );
        let local = project.join(".tako/secrets.local.json");
        let state: serde_json::Value = serde_json::from_slice(&fs::read(&local).unwrap()).unwrap();
        let values = &state["values"];
        let key_id = values["production"]["key_id"].as_str().unwrap();
        let key_path = server.owner_home.join("keys").join(key_id);
        let key = fs::read_to_string(&key_path).unwrap();
        let bundle = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&serde_json::json!({
                "version": 1, "id": key_id, "key": key.trim(),
            }))
            .unwrap(),
        );
        let legacy = serde_json::to_vec(values).unwrap();
        fs::write(project.join(".tako/secrets.json"), &legacy).unwrap();
        fs::remove_file(local).unwrap();
        fs::remove_file(&key_path).unwrap();
        fs::write(project.join("tako.toml"), config).unwrap();

        let mut args = vec!["secrets", "key", "import"];
        if passphrase {
            args.push("--passphrase");
        }
        if explicit_env {
            args.extend(["--env", "production"]);
        }
        server.ok(
            &server.owner_home,
            &project,
            &args,
            Some(if passphrase {
                "recovery-test-passphrase"
            } else {
                &bundle
            }),
        );
        assert!(key_path.is_file());
        assert_eq!(
            fs::read(project.join(".tako/secrets.json")).unwrap(),
            legacy
        );
        server.ok(&server.owner_home, &project, &["secrets", "migrate"], None);
        server.ok(
            &server.owner_home,
            &project,
            &["secrets", "sync", "--env", "production"],
            None,
        );
        let migrated: serde_json::Value =
            serde_json::from_slice(&fs::read(project.join(".tako/secrets.local.json")).unwrap())
                .unwrap();
        let env = &migrated["values"]["production"];
        let local_key = fs::read_to_string(
            server
                .owner_home
                .join("keys")
                .join(env["key_id"].as_str().unwrap()),
        )
        .unwrap();
        for (name, expected) in [("LEGACY", "recovered-local"), ("REMOTE", "published")] {
            let ciphertext = env["app"][name]["value"].as_str().unwrap();
            assert!(
                super::secret_commands::decrypt_test_secret(ciphertext, &local_key) == expected
            );
        }
    }
}

#[test]
fn deploy_and_secret_commands_materialize_the_selected_config() {
    let server = ApiServer::new();
    for (app, key) in [("demo", "DEFAULT_ONLY"), ("alternate", "ALTERNATE_ONLY")] {
        let publisher = server.project(app);
        let config = fs::read_to_string(publisher.join("tako.toml"))
            .unwrap()
            .replace("name='demo'", &format!("name='{app}'"));
        fs::write(publisher.join("tako.toml"), config).unwrap();
        server.ok(
            &server.owner_home,
            &publisher,
            &["secrets", "set", key, "--env", "production"],
            Some("fixture-value"),
        );
        server.ok(
            &server.owner_home,
            &publisher,
            &["secrets", "sync", "--env", "production"],
            None,
        );
    }
    for command in ["deploy", "secrets"] {
        let project = server.project(command);
        fs::write(project.join("alternate.toml"), format!(
            "name='alternate'\nimage='example/alternate:latest'\n[envs.production]\nnetwork='{}'\nroute='alternate.example.test'\nservers=['missing']\n",
            server.network,
        )).unwrap();
        let args = if command == "deploy" {
            vec![
                "--config",
                "alternate.toml",
                "deploy",
                "--env",
                "production",
            ]
        } else {
            vec!["--config", "alternate.toml", "secrets", "list"]
        };
        let result = server.run(&server.owner_home, &project, &args, None);
        if command == "deploy" {
            // Stop safely at invalid placement, after config/cache loading, without deploying.
            assert!(!result.status.success());
            assert!(
                stderr_str(&result).contains("Server 'missing' is not a trusted server"),
                "{}",
                stderr_str(&result)
            );
        } else {
            assert!(result.status.success(), "{}", stderr_str(&result));
        }
        let state: serde_json::Value =
            serde_json::from_slice(&fs::read(project.join(".tako/secrets.local.json")).unwrap())
                .unwrap();
        let secrets = &state["values"]["production"]["app"];
        assert!(secrets.get("ALTERNATE_ONLY").is_some());
        assert!(secrets.get("DEFAULT_ONLY").is_none());
        // Actual bindings remain protected when a different config is selected later.
        let wrong = server.run(&server.owner_home, &project, &["secrets", "list"], None);
        assert!(!wrong.status.success());
        assert!(stderr_str(&wrong).contains("bound to another app or network"));
        server.ok(
            &server.owner_home,
            &project,
            &[
                "--config",
                "alternate.toml",
                "secrets",
                "sync",
                "--env",
                "production",
            ],
            None,
        );
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
    assert!(server.store.0.lock().unwrap().1.records.is_empty());
    server.ok(
        &server.owner_home,
        &owner,
        &["secrets", "sync", "--env", "production"],
        None,
    );
    let invitation = server.ok(
        &server.owner_home,
        &owner,
        &["network", "client", "add", "second", "--network", "fleet"],
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
        &["network", "join", "--invitation-stdin"],
        Some(&invitation),
    );
    let listed = server.ok(&client_home, &client, &["secrets", "list"], None);
    assert_eq!(listed["secrets"].as_array().unwrap().len(), 2);
    for entry in listed["secrets"].as_array().unwrap() {
        let change = &entry["last_changed"]["production"];
        assert_eq!(change["published"], true);
        assert_eq!(change["device"], "owner");
    }
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
    let before = server.store.0.lock().unwrap().1.records.len();
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
    assert_eq!(state.records.len(), before);
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
    assert!(gamma.is_deletion());
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
        server.store.0.lock().unwrap().1.records.len(),
        state.records.len()
    );
    let scoped_invitation = server.ok(
        &server.owner_home,
        &owner,
        &[
            "network",
            "client",
            "add",
            "scoped",
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
        &["network", "join", "--invitation-stdin"],
        Some(&scoped_invitation),
    );
    let config = scoped_project.join("tako.toml");
    let mut contents = fs::read_to_string(&config).unwrap();
    contents.push_str(
        "backup = { storage = 'archives' }\n[storages.archives]\nprovider='s3'\nbucket='fixture'\nregion='auto'\nendpoint='https://archives.example.test'\n"
    );
    fs::write(config, contents).unwrap();
    server.ok(&scoped_home, &scoped_project, &["backups", "now"], None);
    assert!(!scoped_project.join(".tako/secrets.local.json").exists());
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
            .records
            .iter()
            .all(|record| record.address.kind == tako_network::shared::RecordKind::App)
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
    let count = server.store.0.lock().unwrap().1.records.len();
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
    assert_eq!(server.store.0.lock().unwrap().1.records.len(), count);
    server.ok(
        &server.owner_home,
        &owner,
        &[
            "network",
            "client",
            "revoke",
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
    let clients = |all: bool| {
        let mut args = vec!["network", "client", "list", "--network", "fleet"];
        if all {
            args.push("--all");
        }
        server.ok(&server.owner_home, &owner, &args, None)["clients"]
            .as_array()
            .unwrap()
            .clone()
    };
    let named = |rows: &[serde_json::Value], status: &str| {
        rows.iter()
            .filter(|row| row["name"] == "second" && row["status"] == status)
            .count()
    };
    assert_eq!(named(&clients(false), "active"), 0);
    assert_eq!(named(&clients(true), "revoked"), 1);

    // Adding a revoked client's name brings the same client back with a new key.
    let returned = server.ok(
        &server.owner_home,
        &owner,
        &["network", "client", "add", "second", "--network", "fleet"],
        None,
    );
    assert_eq!(returned["replaced"], serde_json::Value::Null);
    let back_home = server.directory.path().join("back-home");
    let back = server.project("back-checkout");
    server.ok(
        &back_home,
        &back,
        &["network", "join", "--invitation-stdin"],
        Some(returned["invitation"].as_str().unwrap()),
    );
    server.ok(&back_home, &back, &["network", "sync", "fleet"], None);
    // The revoked key now shows as replaced by the new one.
    let rows = clients(true);
    assert_eq!(named(&rows, "active"), 1);
    assert_eq!(named(&rows, "replaced"), 1);

    // An active name needs --yes outside a terminal, and replacing it revokes
    // the current key before the new invitation is shown.
    let refused = server.run(
        &server.owner_home,
        &owner,
        &["network", "client", "add", "second", "--network", "fleet"],
        None,
    );
    assert!(!refused.status.success());
    assert!(
        stderr_str(&refused).contains("--yes"),
        "{}",
        stderr_str(&refused)
    );
    let replaced = server.ok(
        &server.owner_home,
        &owner,
        &[
            "network",
            "client",
            "add",
            "second",
            "--network",
            "fleet",
            "--yes",
        ],
        None,
    );
    assert!(replaced["replaced"].is_string());
    assert!(
        !server
            .run(&back_home, &back, &["network", "sync", "fleet"], None)
            .status
            .success()
    );
    // The new key has not joined yet, so the last key shows as revoked.
    let rows = clients(true);
    assert_eq!(named(&rows, "active"), 0);
    assert_eq!(named(&rows, "replaced"), 1);
    assert_eq!(named(&rows, "revoked"), 1);
    // The owner cannot revoke or replace the client it runs as.
    let own = server.run(
        &server.owner_home,
        &owner,
        &["network", "client", "revoke", "owner", "--network", "fleet"],
        None,
    );
    assert!(!own.status.success());
}

#[test]
fn expired_publication_preserves_current_draft_and_requires_explicit_retry() {
    for legacy in [false, true] {
        let server = ApiServer::new();
        let project = server.project("expired-retry");
        server.ok(
            &server.owner_home,
            &project,
            &["secrets", "set", "TOKEN", "--env", "production"],
            Some("first"),
        );
        server.drop_publication.store(true, Ordering::Relaxed);
        assert!(
            !server
                .run(
                    &server.owner_home,
                    &project,
                    &["secrets", "sync", "--env", "production"],
                    None
                )
                .status
                .success()
        );
        // Simulate returning after the durable request's fixed retry window.
        // The draft is edited since that failed attempt; only its current value
        // should be published when the user explicitly retries.
        let path = project.join(".tako/secrets.local.json");
        let mut state: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let pending = state["environments"]["production"]["pending"]
            .as_array_mut()
            .unwrap();
        assert_eq!(pending.len(), 1);
        if legacy {
            pending[0]["mutation"]
                .as_object_mut()
                .unwrap()
                .remove("expires_at");
        } else {
            pending[0]["mutation"]["expires_at"] = serde_json::json!(0);
        }
        fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
        server.ok(
            &server.owner_home,
            &project,
            &["secrets", "set", "TOKEN", "--env", "production"],
            Some("current-draft"),
        );
        let before = server.store.0.lock().unwrap().1.clone();
        let failed = server.run(
            &server.owner_home,
            &project,
            &["secrets", "sync", "--env", "production"],
            None,
        );
        assert!(!failed.status.success());
        assert!(
            String::from_utf8_lossy(&failed.stderr).contains("retry-expired")
                || String::from_utf8_lossy(&failed.stdout).contains("retry-expired")
        );
        assert_eq!(server.store.0.lock().unwrap().1, before);
        server.ok(
            &server.owner_home,
            &project,
            &["secrets", "sync", "--env", "production", "--retry-expired"],
            None,
        );
        let after = server.store.0.lock().unwrap().1.clone();
        assert_eq!(after.records.len(), 1);
        assert_eq!(after.records[0].revision, 2);
        assert_ne!(after.records[0].id, before.records[0].id);
        let registry = server.store.0.lock().unwrap().0.clone();
        let admin = registry
            .members
            .iter()
            .find(|member| member.device_id == registry.network.initial_admin)
            .unwrap();
        let keys = LocalDeviceKeys::load(
            &server
                .owner_home
                .join("network-keys")
                .join(format!("{}.json", admin.request.request_id)),
        )
        .unwrap();
        let plaintext = after.records[0]
            .open(&after, &keys, &admin.device_id)
            .unwrap()
            .unwrap();
        let content: tako_network::shared::RecordContent =
            serde_json::from_slice(&plaintext).unwrap();
        assert_eq!(&*content.value, "current-draft");
        let disk: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert!(
            disk["environments"]["production"]["pending"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            !serde_json::to_string(&disk)
                .unwrap()
                .contains("current-draft")
        );
    }
}

#[test]
fn clearing_an_environment_publishes_deletions_and_leaves_other_drafts_local() {
    let server = ApiServer::new();
    let owner = server.project("clear-owner");
    let other = server.project("clear-other");
    for name in ["ALPHA", "BETA"] {
        server.ok(
            &server.owner_home,
            &owner,
            &["secrets", "set", name, "--env", "production"],
            Some("published"),
        );
    }
    server.ok(
        &server.owner_home,
        &owner,
        &["secrets", "sync", "--env", "production"],
        None,
    );
    // Published from another worktree, so it is absent from the owner's drafts.
    server.ok(
        &server.owner_home,
        &other,
        &["secrets", "set", "GAMMA", "--env", "production"],
        Some("from-other-worktree"),
    );
    server.ok(
        &server.owner_home,
        &other,
        &["secrets", "sync", "--env", "production"],
        None,
    );
    server.ok(
        &server.owner_home,
        &owner,
        &["secrets", "set", "DRAFT_ONLY", "--env", "production"],
        Some("unpublished"),
    );
    // A draft-only removal of a published value is still reported and deleted.
    server.ok(
        &server.owner_home,
        &owner,
        &["secrets", "rm", "BETA", "--env", "production", "--yes"],
        None,
    );
    server.ok(
        &server.owner_home,
        &owner,
        &["credentials", "set", "postgres_url", "--env", "production"],
        Some("postgres://fixture:example@db.example.test/app"),
    );
    let published = server.store.0.lock().unwrap().1.records.len();
    let expected = ["ALPHA", "BETA", "DRAFT_ONLY", "GAMMA"];

    let unconfirmed = server.run(
        &server.owner_home,
        &owner,
        &["secrets", "clear", "--env", "production"],
        None,
    );
    assert!(!unconfirmed.status.success());
    assert!(
        stderr_str(&unconfirmed).contains("requires --yes"),
        "{}",
        stderr_str(&unconfirmed)
    );

    let preview = server.ok(
        &server.owner_home,
        &owner,
        &["--dry-run", "secrets", "clear", "--env", "production"],
        None,
    );
    assert_eq!(preview["cleared"], serde_json::json!(expected));
    assert_eq!(preview["dry_run"], true);
    assert_eq!(server.store.0.lock().unwrap().1.records.len(), published);

    let cleared = server.ok(
        &server.owner_home,
        &owner,
        &["secrets", "clear", "--env", "production", "--yes"],
        None,
    );
    assert_eq!(cleared["command"], "secrets");
    assert_eq!(cleared["environment"], "production");
    assert_eq!(cleared["cleared"], serde_json::json!(expected));
    assert_eq!(cleared["dry_run"], false);

    let state = server.store.0.lock().unwrap().1.clone();
    let latest = state.latest().unwrap();
    for name in ["ALPHA", "BETA", "GAMMA"] {
        let record = latest
            .values()
            .find(|record| record.address.name == name)
            .unwrap();
        assert!(record.is_deletion(), "{name} was not deleted");
    }
    // Deletion markers replace the published values. The unpublished app draft
    // is discarded, and the credential draft stays local.
    assert_eq!(state.records.len(), published);
    assert!(
        latest
            .values()
            .all(|record| record.address.name != "DRAFT_ONLY"
                && record.address.name != "postgres_url")
    );
    let local: serde_json::Value =
        serde_json::from_slice(&fs::read(owner.join(".tako/secrets.local.json")).unwrap()).unwrap();
    let values = &local["values"]["production"];
    assert!(values["app"].as_object().is_none_or(|app| app.is_empty()));
    assert!(values["credentials"].get("postgres_url").is_some());

    // Other worktrees drop the deleted values on their next sync.
    server.ok(
        &server.owner_home,
        &other,
        &["secrets", "sync", "--env", "production"],
        None,
    );
    let listed = server.ok(&server.owner_home, &other, &["secrets", "list"], None);
    assert_eq!(listed["secrets"], serde_json::json!([]));

    // Clearing again publishes nothing new.
    let again = server.ok(
        &server.owner_home,
        &owner,
        &["secrets", "clear", "--env", "production", "--yes"],
        None,
    );
    assert_eq!(again["cleared"], serde_json::json!([]));
    assert_eq!(server.store.0.lock().unwrap().1, state);
}

#[test]
fn clearing_requires_an_environment_bound_to_a_network() {
    let server = ApiServer::new();
    let project = server.project("clear-unbound");
    fs::write(
        project.join("tako.toml"),
        "name='demo'\n[envs.production]\nroute='demo.example.test'\n",
    )
    .unwrap();
    let result = server.run(
        &server.owner_home,
        &project,
        &["secrets", "clear", "--env", "production", "--yes"],
        None,
    );
    assert!(!result.status.success());
    assert!(
        stderr_str(&result).contains("has no network in tako.toml"),
        "{}",
        stderr_str(&result)
    );
}

#[test]
fn interrupted_clear_keeps_its_deletions_and_finishes_when_rerun() {
    let server = ApiServer::new();
    let project = server.project("clear-interrupted");
    server.ok(
        &server.owner_home,
        &project,
        &["secrets", "set", "TOKEN", "--env", "production"],
        Some("published"),
    );
    server.ok(
        &server.owner_home,
        &project,
        &["secrets", "sync", "--env", "production"],
        None,
    );
    server.drop_publication.store(true, Ordering::Relaxed);
    let failed = server.run(
        &server.owner_home,
        &project,
        &["secrets", "clear", "--env", "production", "--yes"],
        None,
    );
    assert!(!failed.status.success());
    let message = format!("{}{}", stderr_str(&failed), stdout_str(&failed));
    assert!(message.contains("did not finish"), "{message}");
    assert!(
        message.contains("tako secrets clear --env production"),
        "{message}"
    );
    server.ok(
        &server.owner_home,
        &project,
        &["secrets", "clear", "--env", "production", "--yes"],
        None,
    );
    let state = server.store.0.lock().unwrap().1.clone();
    let latest = state.latest().unwrap();
    assert!(latest.values().all(|record| record.is_deletion()));
    let local: serde_json::Value =
        serde_json::from_slice(&fs::read(project.join(".tako/secrets.local.json")).unwrap())
            .unwrap();
    assert!(
        local["environments"]["production"]["pending"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn clear_supersedes_expired_app_secret_commands() {
    let server = ApiServer::new();
    let project = server.project("clear-expired");
    server.ok(
        &server.owner_home,
        &project,
        &["secrets", "set", "TOKEN", "--env", "production"],
        Some("published"),
    );
    server.ok(
        &server.owner_home,
        &project,
        &["secrets", "sync", "--env", "production"],
        None,
    );
    server.ok(
        &server.owner_home,
        &project,
        &["secrets", "set", "TOKEN", "--env", "production"],
        Some("uncertain"),
    );
    server.drop_publication.store(true, Ordering::Relaxed);
    assert!(
        !server
            .run(
                &server.owner_home,
                &project,
                &["secrets", "sync", "--env", "production"],
                None,
            )
            .status
            .success()
    );
    let path = project.join(".tako/secrets.local.json");
    let mut local: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let pending = local["environments"]["production"]["pending"]
        .as_array_mut()
        .unwrap();
    assert_eq!(pending.len(), 1);
    pending[0]["mutation"]["expires_at"] = serde_json::json!(0);
    fs::write(&path, serde_json::to_vec(&local).unwrap()).unwrap();
    server.ok(
        &server.owner_home,
        &project,
        &["credentials", "set", "postgres_url", "--env", "production"],
        Some("postgres://fixture:example@db.example.test/app"),
    );

    let cleared = server.ok(
        &server.owner_home,
        &project,
        &["secrets", "clear", "--env", "production", "--yes"],
        None,
    );
    assert_eq!(cleared["cleared"], serde_json::json!(["TOKEN"]));
    let state = server.store.0.lock().unwrap().1.clone();
    let latest = state.latest().unwrap();
    let token = latest
        .values()
        .find(|record| record.address.name == "TOKEN")
        .unwrap();
    assert!(token.is_deletion());
    // The unrelated credential draft was not published along the way.
    assert!(
        latest
            .values()
            .all(|record| record.address.name != "postgres_url")
    );
}

#[test]
fn clear_leaves_other_pending_publications_queued() {
    let server = ApiServer::new();
    let project = server.project("clear-deferred");
    server.ok(
        &server.owner_home,
        &project,
        &["secrets", "set", "TOKEN", "--env", "production"],
        Some("published"),
    );
    server.ok(
        &server.owner_home,
        &project,
        &["secrets", "sync", "--env", "production"],
        None,
    );
    // An interrupted sync leaves a credential publication pending, now expired.
    server.ok(
        &server.owner_home,
        &project,
        &["credentials", "set", "postgres_url", "--env", "production"],
        Some("postgres://fixture:example@db.example.test/app"),
    );
    server.drop_publication.store(true, Ordering::Relaxed);
    assert!(
        !server
            .run(
                &server.owner_home,
                &project,
                &["secrets", "sync", "--env", "production"],
                None,
            )
            .status
            .success()
    );
    let path = project.join(".tako/secrets.local.json");
    let mut local: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let pending = local["environments"]["production"]["pending"]
        .as_array_mut()
        .unwrap();
    assert_eq!(pending.len(), 1);
    pending[0]["mutation"]["expires_at"] = serde_json::json!(0);
    fs::write(&path, serde_json::to_vec(&local).unwrap()).unwrap();
    let before = server.store.0.lock().unwrap().1.clone();
    let credential = before
        .latest()
        .unwrap()
        .values()
        .find(|record| record.address.name == "postgres_url")
        .map(|record| record.id.clone());

    let cleared = server.ok(
        &server.owner_home,
        &project,
        &["secrets", "clear", "--env", "production", "--yes"],
        None,
    );
    assert_eq!(cleared["cleared"], serde_json::json!(["TOKEN"]));
    let after = server.store.0.lock().unwrap().1.clone();
    let latest = after.latest().unwrap();
    assert!(
        latest
            .values()
            .find(|record| record.address.name == "TOKEN")
            .unwrap()
            .is_deletion()
    );
    // The credential command was neither retried nor dropped by the clear.
    assert_eq!(
        latest
            .values()
            .find(|record| record.address.name == "postgres_url")
            .map(|record| record.id.clone()),
        credential
    );
    let local: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let pending = local["environments"]["production"]["pending"]
        .as_array()
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0]["mutation"]["address"]["name"], "postgres_url");
}
