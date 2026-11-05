use super::{
    AgentError, AgentRequest, AgentResponse, Result,
    lifecycle::{Control, WithdrawOnDrop},
    now,
    process::{self, OwnedChild},
    projection,
    services::Publisher,
    store::{MonotonicStore, preserves},
};
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use tako_network::{
    BootId, DeviceKind, LocalDeviceKeys, MemberKind, NodeConfig, RegistrySnapshot, ServerIdentity,
    api::{RpcRequest, RuntimeError},
    corrosion::{BearerToken, CorrosionStore, MAX_REGISTRY_BYTES, RegistryStore},
    node_config::protected::Directory,
    runtime::{RuntimeConfig, RuntimeLimits, ServerRuntime},
};
use zeroize::Zeroizing;

pub(super) struct Work {
    pub request: AgentRequest,
    pub reply: tokio::sync::oneshot::Sender<AgentResponse>,
    pub deadline: Instant,
}

pub(super) fn worker(
    config: NodeConfig,
    data_dir: PathBuf,
    boot: BootId,
    rx: mpsc::Receiver<Work>,
    control: Control,
) -> Result<()> {
    let _membership_lease = WithdrawOnDrop(control.clone());
    let stop = control.stop.clone();
    let mut controller =
        match Controller::start(config.clone(), data_dir.clone(), boot, control.clone()) {
            Ok(controller) => controller,
            Err(error) => {
                if stop.load(Ordering::Acquire) {
                    wait_for_broker(&control);
                    if !control.isolate.load(Ordering::Acquire) {
                        return Err(error);
                    }
                }
                // Even errors before child spawn must revoke previously installed peers.
                let isolation = (|| -> Result<()> {
                    let root = config.network_dir(&data_dir);
                    let directory = Directory::open(&root, true)?;
                    let keys = LocalDeviceKeys::load(&root.join(&config.files.keys))?;
                    Controller::initial_isolate(&config, &directory, &keys)
                })();
                if let Err(isolation) = isolation {
                    tracing::error!(error = %isolation, "startup host isolation not acknowledged");
                }
                return Err(error);
            }
        };
    let mut next = Instant::now();
    while !stop.load(Ordering::Acquire) {
        if Instant::now() >= next {
            controller.reconcile_or_isolate();
            controller.refresh_services();
            next = Instant::now() + Duration::from_secs(2);
        }
        match rx.recv_timeout(
            next.saturating_duration_since(Instant::now())
                .min(Duration::from_millis(100)),
        ) {
            Ok(work) => {
                if work.reply.is_closed() || Instant::now() >= work.deadline {
                    continue;
                }
                // The request path is never reconstructed from the signed body.
                let response = controller.handle(work.request);
                let _ = work.reply.send(response);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => (),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    control.withdraw();
    // A late broker shutdown error must still isolate while this thread owns
    // the identity, host state and nonce DB lock. Never release early.
    wait_for_broker(&control);
    let isolation = if control.isolate.load(Ordering::Acquire) {
        controller.isolate()
    } else {
        Ok(())
    };
    if let Err(error) = &isolation {
        tracing::error!(error = %error, "agent failure isolation not acknowledged");
    }
    let child = controller.child.stop();
    if let Err(error) = &child {
        tracing::error!(error = %error, "agent child shutdown failed");
    }
    isolation.and(child)
}

fn wait_for_broker(control: &Control) {
    while !control.broker_drained.load(Ordering::Acquire)
        || !control.services_drained.load(Ordering::Acquire)
    {
        thread::sleep(Duration::from_millis(25));
    }
}

struct Controller {
    config: NodeConfig,
    data_dir: PathBuf,
    directory: Directory,
    keys: LocalDeviceKeys,
    // This wrapper is required: runtime performs several reads within one RPC.
    runtime: ServerRuntime<MonotonicStore<CorrosionStore>>,
    known: Rc<RefCell<RegistrySnapshot>>,
    service_store: Rc<RefCell<CorrosionStore>>,
    publisher: Publisher,
    child: OwnedChild,
    token: Zeroizing<String>,
    gossip_hash: [u8; 32],
    applied: ApplyCache,
    boot: BootId,
    ready: bool,
    revoked: bool,
    repair_mode: bool,
    stop: Arc<AtomicBool>,
    renewal: Renewal,
    control: Control,
}
impl Controller {
    fn start(
        config: NodeConfig,
        data_dir: PathBuf,
        boot: BootId,
        control: Control,
    ) -> Result<Self> {
        let stop = control.stop.clone();
        config.validate()?;
        let root = config.network_dir(&data_dir);
        let directory = Directory::open(&root, true)?;
        // Recover any committed leaf journal before loading keys/certificates.
        // Renewal validates immutable keys/issuers and never changes their pins.
        let gossip_leaf_hash = renew_identity(&config, &root, &directory)?;
        for name in config.files.names() {
            directory.read(name, 128 * 1024)?;
        }
        let keys = LocalDeviceKeys::load(&root.join(&config.files.keys))?;
        if keys.device_id(&config.network.id, DeviceKind::Server)? != config.server_id {
            return Err(AgentError::Invalid(
                "configured keys do not identify server",
            ));
        }
        let token_bytes = directory.read(&config.files.api_token, 4096)?;
        let token = Zeroizing::new(
            std::str::from_utf8(&token_bytes)
                .map_err(|_| AgentError::Invalid("invalid API credential encoding"))?
                .to_owned(),
        );
        BearerToken::new(token.to_string())?;
        let marker = directory.exists("bootstrap-complete")?;
        let known = if directory.exists("known-registry.json")? {
            let known: RegistrySnapshot = serde_json::from_slice(
                &directory.read("known-registry.json", MAX_REGISTRY_BYTES)?,
            )?;
            preserves(&config.bootstrap, &known)?;
            known
        } else if marker {
            Self::initial_isolate(&config, &directory, &keys)?;
            return Err(AgentError::Invalid(
                "bootstrap marker without known records; restore required",
            ));
        } else {
            directory.write(
                "known-registry.json",
                &serde_json::to_vec(&config.bootstrap)?,
            )?;
            config.bootstrap.clone()
        };
        if marker && (!directory.exists("corrosion.db")? || !directory.exists("runtime.db")?) {
            Self::initial_isolate(&config, &directory, &keys)?;
            return Err(AgentError::Invalid(
                "initialized database missing; operator restore required",
            ));
        }
        if marker {
            verify_existing_databases(&directory, &root)?;
        }
        validate_gossip_leaf(&config, &directory, &known)?;
        let current_time = now()?;
        // Bring up the overlay before binding gossip to its derived WG address.
        let desired = projection::desired(&config, &keys, &known, current_time, true)?;
        directory.write("desired.json", &desired)?;
        process::apply(&config, &desired)?;
        let repair_mode = !serving(&config, &known, current_time)?;
        let (bundle, seeds) = if repair_mode {
            projection::write_repair_corrosion(&config, &data_dir, &directory, &token)?;
            (String::new(), Vec::new())
        } else {
            let (bundle, seeds) = projection::gossip(&config, &known, current_time)?;
            projection::write_corrosion(
                &config, &data_dir, &directory, &keys, &bundle, &seeds, &token,
            )?;
            (bundle, seeds)
        };
        let publisher = Publisher::open(&config, &data_dir)?;
        let mut child = OwnedChild::corrosion(&config, &data_dir)?;
        let startup = (|| -> Result<_> {
            let mut store = CorrosionStore::new(
                "http://127.0.0.1:9834",
                BearerToken::new(token.to_string())?,
                config.network.clone(),
            )?;
            let deadline = Instant::now() + Duration::from_secs(30);
            // Only transport failures are retryable. An empty/malformed existing
            // registry is not permission to bootstrap a replacement database.
            loop {
                check_stop(&stop)?;
                if !child.running()? {
                    return Err(AgentError::Invalid("Corrosion exited during startup"));
                }
                let result = if marker {
                    store.snapshot().map(|_| ())
                } else {
                    store.initialize()
                };
                match result {
                    Ok(()) => break,
                    Err(RuntimeError::Transport) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(100))
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            if !marker {
                for member in &config.bootstrap.members {
                    check_stop(&stop)?;
                    store.append_member(member)?;
                }
                for revocation in &config.bootstrap.revocations {
                    check_stop(&stop)?;
                    store.append_revocation(revocation)?;
                }
                // Historical bootstrap ACKs are not evidence of this boot's enforcement.
            }
            let shared = Rc::new(RefCell::new(known.clone()));
            let mut guarded =
                MonotonicStore::new(store, Directory::open(&root, true)?, shared.clone())
                    .with_stop(stop.clone());
            guarded.snapshot()?;
            for name in ["runtime.db", "runtime.runtime-lock"] {
                if !directory.exists(name)? {
                    directory.write(name, b"")?;
                }
            }
            let service_store = guarded.shared();
            let runtime = ServerRuntime::open(
                guarded,
                LocalDeviceKeys::load(&root.join(&config.files.keys))?,
                RuntimeConfig {
                    network: config.network.clone(),
                    server_id: config.server_id.clone(),
                    boot_id: boot.clone(),
                    limits: RuntimeLimits::default(),
                },
                &root.join("runtime.db"),
            )?;
            directory.write("bootstrap-complete", b"1\n")?;
            Ok((runtime, shared, service_store))
        })();
        let (runtime, shared, service_store) = match startup {
            Ok(value) => value,
            Err(error) => {
                if stop.load(Ordering::Acquire) {
                    return Err(error);
                }
                Self::initial_isolate(&config, &directory, &keys)?;
                return Err(error);
            }
        };
        Ok(Self {
            config,
            data_dir,
            directory,
            keys,
            runtime,
            known: shared,
            service_store,
            publisher,
            child,
            token,
            gossip_hash: gossip_hash(&bundle, &seeds)?,
            applied: ApplyCache::default(),
            boot,
            ready: false,
            revoked: repair_mode,
            repair_mode,
            stop,
            renewal: Renewal::new(gossip_leaf_hash),
            control,
        })
    }
    fn initial_isolate(
        config: &NodeConfig,
        directory: &Directory,
        keys: &LocalDeviceKeys,
    ) -> Result<()> {
        let bytes = projection::desired(config, keys, &config.bootstrap, now()?, true)?;
        directory.write("desired.json", &bytes)?;
        process::apply(config, &bytes)
    }
    fn withdraw(&mut self) {
        self.ready = false;
        self.control.withdraw();
    }
    fn isolate(&mut self) -> Result<()> {
        self.withdraw();
        let bytes =
            projection::desired(&self.config, &self.keys, &self.known.borrow(), now()?, true)?;
        self.apply(&bytes)
    }
    fn apply(&mut self, bytes: &[u8]) -> Result<()> {
        if self.applied.hash != Some(openssl::sha::sha256(bytes)) {
            self.withdraw();
        }
        self.applied
            .enforce_with_membership(bytes, &self.control, || {
                self.directory.write("desired.json", bytes)?;
                process::apply(&self.config, bytes)
            })
    }
    fn reconcile(&mut self) -> Result<()> {
        let now = now()?;
        // Runtime projection advances its durable clock floor and includes
        // local denial intent even when remote publication failed.
        let snapshot = self.runtime.desired_host_state(now)?.snapshot;
        if !self.control.matches(&snapshot) {
            self.withdraw();
        }
        let desired = projection::desired(&self.config, &self.keys, &snapshot, now, false)?;
        self.apply(&desired)?;
        if !serving(&self.config, &snapshot, now)? {
            // Empty desired state has already removed peers and SSH access.
            self.withdraw();
            self.revoked = true;
            self.maintain_repair()?;
            return Ok(());
        }
        let (bundle, seeds) = projection::gossip(&self.config, &snapshot, now)?;
        let hash = gossip_hash(&bundle, &seeds)?;
        if hash != self.gossip_hash || self.renewal.restart || !self.child.running()? {
            self.withdraw();
            self.child.stop()?;
            projection::write_corrosion(
                &self.config,
                &self.data_dir,
                &self.directory,
                &self.keys,
                &bundle,
                &seeds,
                &self.token,
            )?;
            self.child = OwnedChild::corrosion(&self.config, &self.data_dir)?;
            self.gossip_hash = hash;
            self.renewal.restart = false;
            // The new child must answer a validated read before readiness/ACK.
            self.runtime.snapshot()?;
        }
        self.runtime.acknowledge_applied(&snapshot)?;
        // ACK creates rows; reject rollback observed while writing those ACKs.
        let after = self.runtime.snapshot()?;
        if after.members != snapshot.members || after.revocations != snapshot.revocations {
            return Err(AgentError::Invalid(
                "registry changed while applying snapshot",
            ));
        }
        // Publish the already enforced current authority before any RPC can
        // report ready. A simultaneous broker failure/shutdown wins this gate.
        self.ready = self.control.publish_enforced(&after);
        Ok(())
    }
    fn reconcile_or_isolate(&mut self) {
        if self.stop.load(Ordering::Acquire) {
            self.withdraw();
            return;
        }
        // Restart a reaped child before reading the HTTP store. Never reopen the
        // runtime, so the nonce DB lock remains held through child restarts.
        let result = (|| {
            if Instant::now() >= self.renewal.next {
                let root = self.config.network_dir(&self.data_dir);
                let hash = renew_identity(&self.config, &root, &self.directory)?;
                self.renewal.observe(hash);
            }
            if self.revoked {
                return self.maintain_repair();
            }
            if !self.child.running()? {
                self.withdraw();
                // A forced-but-reaped stop must never restart stale CA trust.
                let (bundle, seeds) =
                    projection::gossip(&self.config, &self.known.borrow(), now()?)?;
                projection::write_corrosion(
                    &self.config,
                    &self.data_dir,
                    &self.directory,
                    &self.keys,
                    &bundle,
                    &seeds,
                    &self.token,
                )?;
                self.child = OwnedChild::corrosion(&self.config, &self.data_dir)?;
                self.gossip_hash = gossip_hash(&bundle, &seeds)?;
                self.renewal.restart = false;
            }
            self.reconcile()
        })();
        if let Err(error) = result {
            self.withdraw();
            if self.stop.load(Ordering::Acquire) {
                return;
            }
            tracing::warn!(error = %error, "native agent reconciliation failed closed");
            if self.revoked {
                if let Err(error) = self.child.stop() {
                    tracing::error!(error = %error, "isolated repair child stop failed");
                }
            }
            if let Err(error) = self.isolate() {
                tracing::error!(error = %error, "native agent host isolation not acknowledged");
            }
        }
    }
    fn maintain_repair(&mut self) -> Result<()> {
        // A revoked latch is terminal for positive authority, including across
        // child restarts. Keep zero peers/SSH and all admission watches withdrawn.
        self.isolate()?;
        if !self.repair_mode || self.renewal.restart {
            self.child.stop()?;
        }
        if !self.child.running()? {
            validate_gossip_leaf(&self.config, &self.directory, &self.known.borrow())?;
            projection::write_repair_corrosion(
                &self.config,
                &self.data_dir,
                &self.directory,
                &self.token,
            )?;
            self.child = OwnedChild::corrosion(&self.config, &self.data_dir)?;
            self.repair_mode = true;
            self.renewal.restart = false;
        }
        // This enables only local SQL persistence. No ACK/readiness or claim of
        // peer dissemination follows a successful loopback child restart.
        Ok(())
    }
    fn refresh_services(&mut self) {
        if !self.ready || self.stop.load(Ordering::Acquire) {
            return;
        }
        let result = (|| -> Result<()> {
            let time = now()?;
            let snapshot = self.runtime.desired_host_state(time)?.snapshot;
            if !self.control.matches(&snapshot) {
                self.withdraw();
                return Err(AgentError::Invalid(
                    "service membership changed before read",
                ));
            }
            let leases = self.publisher.refresh(
                &self.config,
                &self.keys,
                &self.service_store.borrow(),
                &snapshot,
                time,
            )?;
            // Do not re-stamp cached data after failure or a slow helper. Capture
            // this read's timestamp before enforcing/checking the current state.
            let observed_at = now()?;
            self.reconcile_or_isolate();
            if !self.ready {
                return Err(AgentError::Invalid("service enforcement unavailable"));
            }
            self.control.publish_services(
                &crate::native_services::ServiceSnapshot {
                    observed_at,
                    registry: snapshot,
                    leases,
                },
                now()?,
            )
        })();
        if let Err(error) = result {
            self.control.clear_services();
            tracing::warn!(error = %error, "native service refresh failed closed");
            // Registry errors also need authoritative denial/host reconciliation;
            // an unavailable service table alone must not disable admin repair.
            self.reconcile_or_isolate();
        }
    }
    fn handle(&mut self, request: AgentRequest) -> AgentResponse {
        let repair = allowed_while_unready(&request.signed.request);
        let was_ready = self.ready;
        if self.ready {
            self.reconcile_or_isolate();
        }
        if !self.ready && !repair {
            return AgentResponse::unavailable(&self.boot);
        }
        let mutates_authority = matches!(
            &request.signed.request,
            RpcRequest::Complete { .. } | RpcRequest::Revoke { .. }
        );
        let result = now().and_then(|time| {
            self.runtime
                .handle(request.signed, &request.method, &request.path, time)
                .map_err(Into::into)
        });
        if mutates_authority
            && matches!(
                &result,
                Ok(_)
                    | Err(AgentError::Runtime(
                        RuntimeError::Transport
                            | RuntimeError::Database(_)
                            | RuntimeError::Corrosion(_)
                            | RuntimeError::Capacity(_)
                    ))
            )
        {
            self.withdraw();
        }
        // A successful mutation may introduce revocations, including our own.
        // Never return stale readiness or acknowledge unapplied records.
        // Unready repair RPCs still pass through runtime authentication. The
        // scheduled reconciler retries enforcement; another blocking helper
        // attempt here would prevent an administrator from inspecting/repairing
        // a capacity or helper failure. These responses retain ready=false.
        if was_ready {
            self.reconcile_or_isolate();
        }
        match result {
            Ok(response) => AgentResponse {
                status: 200,
                response: Some(response),
                error: None,
                boot: self.boot.clone(),
                ready: self.ready,
            },
            Err(error) => {
                let (status, message) = match error {
                    AgentError::Runtime(RuntimeError::Authentication) => {
                        (401, "native request authentication failed")
                    }
                    AgentError::Runtime(RuntimeError::Denied) => (403, "native request denied"),
                    AgentError::Runtime(RuntimeError::Replay | RuntimeError::Conflict(_)) => {
                        (409, "native request conflicts with recorded state")
                    }
                    AgentError::Runtime(RuntimeError::Capacity(_)) => {
                        (429, "native runtime capacity exceeded")
                    }
                    AgentError::Runtime(
                        RuntimeError::PendingUnavailable | RuntimeError::ChallengeUnavailable,
                    ) => (410, "native enrollment unavailable"),
                    _ => (503, "native request could not be completed"),
                };
                AgentResponse {
                    status,
                    response: None,
                    error: Some(message.into()),
                    boot: self.boot.clone(),
                    ready: self.ready,
                }
            }
        }
    }
}
/// No durable hash: each process boot must enforce its first snapshot.
#[derive(Default)]
struct ApplyCache {
    hash: Option<[u8; 32]>,
}
impl ApplyCache {
    fn enforce_with_membership(
        &mut self,
        bytes: &[u8],
        control: &Control,
        apply: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        if self.hash != Some(openssl::sha::sha256(bytes)) {
            control.withdraw();
        }
        self.enforce(bytes, apply)
    }

    fn enforce(&mut self, bytes: &[u8], apply: impl FnOnce() -> Result<()>) -> Result<()> {
        let hash = openssl::sha::sha256(bytes);
        if self.hash == Some(hash) {
            return Ok(());
        }
        self.hash = None;
        apply()?;
        self.hash = Some(hash);
        Ok(())
    }
}

const RENEWAL_INTERVAL: Duration = Duration::from_secs(3600);
struct Renewal {
    next: Instant,
    gossip: [u8; 32],
    restart: bool,
}
impl Renewal {
    fn new(gossip: [u8; 32]) -> Self {
        Self {
            next: Instant::now() + RENEWAL_INTERVAL,
            gossip,
            restart: false,
        }
    }
    fn observe(&mut self, gossip: [u8; 32]) {
        // Keep an outstanding restart after a forced/failed stop. A later no-op
        // renewal is not evidence that the existing child loaded the new leaf.
        self.restart |= gossip != self.gossip;
        self.gossip = gossip;
        self.next = Instant::now() + RENEWAL_INTERVAL;
    }
}

fn renew_identity(config: &NodeConfig, root: &Path, directory: &Directory) -> Result<[u8; 32]> {
    let identity = ServerIdentity::renew(root)?;
    let https = identity.tls_paths();
    let gossip = identity.gossip_paths();
    // NodeFiles stay explicit. Accept either managed leaf or fullchain references,
    // but never renew one identity while actually serving unrelated files.
    let https_file = root.join(&config.files.https_certificate);
    let gossip_file = root.join(&config.files.gossip_certificate);
    if root.join(&config.files.keys) != identity.keys_path()
        || (https_file != https.certificate && https_file != https.fullchain)
        || root.join(&config.files.https_private_key) != https.private_key
        || (gossip_file != gossip.certificate && gossip_file != gossip.fullchain)
        || root.join(&config.files.gossip_private_key) != gossip.private_key
        || root.join(&config.files.gossip_ca_certificate) != gossip.ca_certificate
    {
        return Err(AgentError::Invalid(
            "configured files differ from prepared server identity",
        ));
    }
    let member = config
        .bootstrap
        .members
        .iter()
        .find(|m| m.device_id == config.server_id)
        .ok_or(AgentError::Invalid(
            "prepared server is absent from bootstrap",
        ))?;
    let MemberKind::Server(info) = &member.kind else {
        return Err(AgentError::Invalid("prepared identity is not a server"));
    };
    if identity.request() != &member.request || identity.server_info() != info {
        return Err(AgentError::Invalid(
            "prepared identity differs from admitted keys or pins",
        ));
    }
    let name = gossip
        .fullchain
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(AgentError::Invalid("invalid managed gossip fullchain path"))?;
    Ok(openssl::sha::sha256(&directory.read(name, 128 * 1024)?))
}

fn serving(config: &NodeConfig, snapshot: &RegistrySnapshot, now: u64) -> Result<bool> {
    Ok(snapshot
        .effective_members(now)?
        .get(&config.server_id)
        .is_some_and(|member| {
            matches!(member.kind, MemberKind::Server(_)) && member.permissions.server_admin
        }))
}

fn allowed_while_unready(request: &RpcRequest) -> bool {
    matches!(request, RpcRequest::Snapshot | RpcRequest::Revoke { .. })
}

fn verify_existing_databases(directory: &Directory, root: &Path) -> Result<()> {
    use std::io::Read;
    for name in ["corrosion.db", "runtime.db"] {
        let mut file = directory.open_file(name)?;
        let mut header = [0u8; 16];
        if file.read_exact(&mut header).is_err() || &header != b"SQLite format 3\0" {
            return Err(AgentError::Invalid(
                "initialized database empty or invalid; operator restore required",
            ));
        }
    }
    // Runtime::open creates its schema for first bootstrap; do not let that
    // recreate an erased nonce journal on a previously initialized node.
    let db = rusqlite::Connection::open_with_flags(
        root.join("runtime.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| AgentError::Invalid("runtime journal requires operator restore"))?;
    let identity: i64 = db
        .query_row(
            "SELECT count(*) FROM runtime_identity WHERE singleton=1",
            [],
            |r| r.get(0),
        )
        .map_err(|_| AgentError::Invalid("runtime identity missing; operator restore required"))?;
    db.query_row("SELECT count(*) FROM consumed_nonces", [], |r| {
        r.get::<_, i64>(0)
    })
    .map_err(|_| AgentError::Invalid("nonce journal missing; operator restore required"))?;
    if identity != 1 {
        return Err(AgentError::Invalid(
            "runtime identity missing; operator restore required",
        ));
    }
    Ok(())
}

fn check_stop(stop: &AtomicBool) -> Result<()> {
    if stop.load(Ordering::Acquire) {
        return Err(AgentError::Invalid("agent stopping"));
    }
    Ok(())
}

fn gossip_hash(bundle: &str, seeds: &[String]) -> Result<[u8; 32]> {
    Ok(openssl::sha::sha256(&serde_json::to_vec(&(bundle, seeds))?))
}
fn validate_gossip_leaf(
    config: &NodeConfig,
    directory: &Directory,
    known: &RegistrySnapshot,
) -> Result<()> {
    use openssl::{pkey::PKey, x509::X509};
    let pem = directory.read(&config.files.gossip_ca_certificate, 16 * 1024)?;
    let member = known
        .members
        .iter()
        .find(|m| m.device_id == config.server_id)
        .ok_or(AgentError::Invalid("server not present in known records"))?;
    let MemberKind::Server(info) = &member.kind else {
        return Err(AgentError::Invalid("local identity is not a server"));
    };
    if info
        .corrosion_ca_certificate_pem
        .as_deref()
        .map(str::as_bytes)
        != Some(pem.as_slice())
    {
        return Err(AgentError::Invalid(
            "prepared gossip CA differs from admitted server",
        ));
    }
    let check = || -> std::result::Result<bool, openssl::error::ErrorStack> {
        let issuer = X509::from_pem(&pem)?;
        let leaf_pem = directory
            .read(&config.files.gossip_certificate, 128 * 1024)
            .map_err(|_| openssl::error::ErrorStack::get())?;
        let leaf = X509::from_pem(&leaf_pem)?;
        let key_pem = directory
            .read(&config.files.gossip_private_key, 128 * 1024)
            .map_err(|_| openssl::error::ErrorStack::get())?;
        let key = PKey::private_key_from_pem(&key_pem)?;
        let issuer_key = issuer.public_key()?;
        let time = openssl::asn1::Asn1Time::days_from_now(0)?;
        let address = config
            .network
            .id
            .address(&member.request.wg_public_key)
            .octets();
        let san_matches = leaf.subject_alt_names().is_some_and(|names| {
            names
                .iter()
                .any(|name| name.ipaddress() == Some(address.as_slice()))
        });
        Ok(leaf.verify(&issuer_key)?
            && leaf.public_key()?.public_eq(&key)
            && leaf.not_before() <= time.as_ref()
            && leaf.not_after() > time.as_ref()
            && issuer.not_before() <= time.as_ref()
            && issuer.not_after() > time.as_ref()
            && san_matches)
    };
    if !check().map_err(|_| AgentError::Invalid("invalid prepared gossip certificate"))? {
        return Err(AgentError::Invalid("prepared gossip leaf/key mismatch"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn membership_is_withdrawn_before_helper_and_stays_absent_after_failure() {
        let fixture = super::super::tests::Fixture::new();
        let (control, members, _) = Control::new();
        let mut applied = ApplyCache::default();
        control.publish_enforced(&fixture.config.bootstrap);
        assert!(
            applied
                .enforce_with_membership(b"new-policy", &control, || {
                    assert!(members.borrow().is_none());
                    Err(AgentError::Timeout)
                })
                .is_err()
        );
        assert!(members.borrow().is_none());
        applied
            .enforce_with_membership(b"new-policy", &control, || {
                assert!(members.borrow().is_none());
                Ok(())
            })
            .unwrap();
        // Even a helper ACK is insufficient until runtime's final state check.
        assert!(members.borrow().is_none());
        assert!(control.publish_enforced(&fixture.config.bootstrap));
        assert!(members.borrow().is_some());
    }
    #[test]
    fn renewal_restart_requires_changed_gossip_bytes_and_survives_retry() {
        let mut renewal = Renewal::new([1; 32]);
        renewal.observe([1; 32]);
        assert!(!renewal.restart);
        renewal.observe([2; 32]);
        assert!(renewal.restart);
        renewal.observe([2; 32]);
        assert!(renewal.restart); // no-op after a failed restart cannot clear it
        renewal.restart = false;
        renewal.observe([2; 32]);
        assert!(!renewal.restart);
    }
    #[test]
    fn startup_and_periodic_renewal_validate_paths_pins_and_fullchain() {
        let fixture = super::super::tests::Fixture::new();
        let mut config = fixture.config;
        let temp = tempfile::tempdir().unwrap();
        let root = config.network_dir(temp.path());
        std::fs::create_dir_all(root.parent().unwrap()).unwrap();
        let identity = ServerIdentity::prepare(
            &root,
            config.network.id.clone(),
            "renewal-fixture".parse().unwrap(),
            "203.0.113.7:51820".parse().unwrap(),
            fixture.keys.ssh_public_key().unwrap(),
            22,
            443,
        )
        .unwrap();
        let name = |path: &Path| path.file_name().unwrap().to_str().unwrap().to_owned();
        config.files.keys = name(identity.keys_path());
        config.files.https_certificate = name(&identity.tls_paths().fullchain);
        config.files.https_private_key = name(&identity.tls_paths().private_key);
        config.files.gossip_ca_certificate = name(&identity.gossip_paths().ca_certificate);
        config.files.gossip_certificate = name(&identity.gossip_paths().fullchain);
        config.files.gossip_private_key = name(&identity.gossip_paths().private_key);
        config.server_id = identity
            .keys()
            .device_id(&config.network.id, DeviceKind::Server)
            .unwrap();
        let member = &mut config.bootstrap.members[1];
        member.device_id = config.server_id.clone();
        member.request = identity.request().clone();
        member.kind = MemberKind::Server(identity.server_info().clone());
        config.validate().unwrap();
        let directory = Directory::open(&root, true).unwrap();
        let first = renew_identity(&config, &root, &directory).unwrap();
        let before = std::fs::metadata(&identity.gossip_paths().fullchain)
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(first, renew_identity(&config, &root, &directory).unwrap());
        assert_eq!(
            before,
            std::fs::metadata(&identity.gossip_paths().fullchain)
                .unwrap()
                .modified()
                .unwrap()
        );
        let mut wrong = config.clone();
        wrong.files.gossip_certificate = "unmanaged.pem".into();
        assert!(renew_identity(&wrong, &root, &directory).is_err());
        let MemberKind::Server(info) = &mut wrong.bootstrap.members[1].kind else {
            unreachable!()
        };
        info.https_port = 8443;
        wrong.files.gossip_certificate = config.files.gossip_certificate.clone();
        assert!(renew_identity(&wrong, &root, &directory).is_err());
        directory
            .write(&config.files.gossip_certificate, b"corrupt fullchain")
            .unwrap();
        assert!(renew_identity(&config, &root, &directory).is_err());
    }
    #[test]
    fn unready_gate_allows_only_authenticated_repair_methods() {
        assert!(allowed_while_unready(&RpcRequest::Snapshot));
        let fixture = super::super::tests::Fixture::new();
        assert!(allowed_while_unready(&RpcRequest::Revoke {
            id: tako_network::RevocationId::random().unwrap(),
            target: fixture.config.server_id
        }));
        assert!(!allowed_while_unready(&RpcRequest::Begin {
            pending_id: tako_network::RequestId::random().unwrap()
        }));
    }
    #[test]
    fn initialized_empty_database_cannot_rebootstrap() {
        let temp = tempfile::tempdir().unwrap();
        let directory = Directory::open(temp.path(), true).unwrap();
        directory.write("corrosion.db", b"").unwrap();
        directory.write("runtime.db", b"").unwrap();
        assert!(verify_existing_databases(&directory, temp.path()).is_err());
        let db = rusqlite::Connection::open(temp.path().join("corrosion.db")).unwrap();
        db.execute_batch("CREATE TABLE fixture(x)").unwrap();
        let db = rusqlite::Connection::open(temp.path().join("runtime.db")).unwrap();
        db.execute_batch(
            "CREATE TABLE runtime_identity(singleton); INSERT INTO runtime_identity VALUES (1)",
        )
        .unwrap();
        assert!(verify_existing_databases(&directory, temp.path()).is_err());
        db.execute_batch("CREATE TABLE consumed_nonces(nonce)")
            .unwrap();
        verify_existing_databases(&directory, temp.path()).unwrap();
    }
    #[test]
    fn helper_failure_is_not_cached_and_each_boot_applies() {
        let mut cache = ApplyCache::default();
        assert!(
            cache
                .enforce(b"snapshot-a", || Err(AgentError::Timeout))
                .is_err()
        );
        assert!(cache.hash.is_none());
        cache.enforce(b"snapshot-a", || Ok(())).unwrap();
        cache
            .enforce(b"snapshot-a", || {
                panic!("unchanged desired state reapplied")
            })
            .unwrap();
        assert!(
            cache
                .enforce(b"snapshot-b", || Err(AgentError::Timeout))
                .is_err()
        );
        // A partial failed update invalidates even the old applied hash.
        let mut called = false;
        cache
            .enforce(b"snapshot-a", || {
                called = true;
                Ok(())
            })
            .unwrap();
        assert!(called);
        let mut reboot = ApplyCache::default();
        called = false;
        reboot
            .enforce(b"snapshot-a", || {
                called = true;
                Ok(())
            })
            .unwrap();
        assert!(called);
    }
}
