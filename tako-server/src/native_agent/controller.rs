use super::{
    AgentError, AgentRequest, AgentResponse, Result, now,
    process::{self, OwnedChild},
    projection,
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
    BootId, DeviceKind, LocalDeviceKeys, MemberKind, NodeConfig, RegistrySnapshot,
    api::RuntimeError,
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
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let mut controller = match Controller::start(config.clone(), data_dir.clone(), boot) {
        Ok(controller) => controller,
        Err(error) => {
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
    controller.child.stop()
}

struct Controller {
    config: NodeConfig,
    data_dir: PathBuf,
    directory: Directory,
    keys: LocalDeviceKeys,
    // This wrapper is required: runtime performs several reads within one RPC.
    runtime: ServerRuntime<MonotonicStore<CorrosionStore>>,
    known: Rc<RefCell<RegistrySnapshot>>,
    child: OwnedChild,
    token: Zeroizing<String>,
    gossip_hash: [u8; 32],
    applied: ApplyCache,
    boot: BootId,
    ready: bool,
    revoked: bool,
}
impl Controller {
    fn start(config: NodeConfig, data_dir: PathBuf, boot: BootId) -> Result<Self> {
        config.validate()?;
        let root = config.network_dir(&data_dir);
        let directory = Directory::open(&root, true)?;
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
        validate_gossip_leaf(&config, &directory, &known)?;
        let current_time = now()?;
        // Bring up the overlay before binding gossip to its derived WG address.
        let desired = projection::desired(&config, &keys, &known, current_time, true)?;
        directory.write("desired.json", &desired)?;
        process::apply(&config, &desired)?;
        let (bundle, seeds) = projection::gossip(&config, &known, current_time)?;
        projection::write_corrosion(
            &config, &data_dir, &directory, &keys, &bundle, &seeds, &token,
        )?;
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
                    store.append_member(member)?;
                }
                for revocation in &config.bootstrap.revocations {
                    store.append_revocation(revocation)?;
                }
                for ack in &config.bootstrap.acks {
                    store.append_ack(ack)?;
                }
            }
            let shared = Rc::new(RefCell::new(known.clone()));
            let mut guarded =
                MonotonicStore::new(store, Directory::open(&root, true)?, shared.clone());
            guarded.snapshot()?;
            for name in ["runtime.db", "runtime.runtime-lock"] {
                if !directory.exists(name)? {
                    directory.write(name, b"")?;
                }
            }
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
            Ok((runtime, shared))
        })();
        let (runtime, shared) = match startup {
            Ok(value) => value,
            Err(error) => {
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
            child,
            token,
            gossip_hash: gossip_hash(&bundle, &seeds)?,
            applied: ApplyCache::default(),
            boot,
            ready: false,
            revoked: false,
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
    fn isolate(&mut self) -> Result<()> {
        self.ready = false;
        let bytes =
            projection::desired(&self.config, &self.keys, &self.known.borrow(), now()?, true)?;
        self.apply(&bytes)
    }
    fn apply(&mut self, bytes: &[u8]) -> Result<()> {
        self.applied.enforce(bytes, || {
            self.directory.write("desired.json", bytes)?;
            process::apply(&self.config, bytes)
        })
    }
    fn reconcile(&mut self) -> Result<()> {
        self.ready = false;
        let now = now()?;
        let snapshot = self.runtime.snapshot()?;
        let desired = projection::desired(&self.config, &self.keys, &snapshot, now, false)?;
        self.apply(&desired)?;
        let effective = snapshot.effective_members(now)?;
        if !effective
            .get(&self.config.server_id)
            .is_some_and(|m| matches!(m.kind, MemberKind::Server(_)) && m.permissions.server_admin)
        {
            // Empty desired state has already removed peers and SSH access.
            self.revoked = true;
            self.child.stop()?;
            return Err(AgentError::Invalid(
                "local server is revoked or ineffective",
            ));
        }
        let (bundle, seeds) = projection::gossip(&self.config, &snapshot, now)?;
        let hash = gossip_hash(&bundle, &seeds)?;
        if hash != self.gossip_hash || !self.child.running()? {
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
        self.ready = true;
        Ok(())
    }
    fn reconcile_or_isolate(&mut self) {
        // Restart a reaped child before reading the HTTP store. Never reopen the
        // runtime, so the nonce DB lock remains held through child restarts.
        let result = (|| {
            if self.revoked {
                return Err(AgentError::Invalid(
                    "local server is revoked or ineffective",
                ));
            }
            if !self.child.running()? {
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
            }
            self.reconcile()
        })();
        if let Err(error) = result {
            tracing::warn!(error = %error, "native agent reconciliation failed closed");
            if let Err(error) = self.isolate() {
                tracing::error!(error = %error, "native agent host isolation not acknowledged");
            }
        }
    }
    fn handle(&mut self, request: AgentRequest) -> AgentResponse {
        self.reconcile_or_isolate();
        if !self.ready {
            return AgentResponse::unavailable(&self.boot);
        }
        let result = now().and_then(|time| {
            self.runtime
                .handle(request.signed, &request.method, &request.path, time)
                .map_err(Into::into)
        });
        // A successful mutation may introduce revocations, including our own.
        // Never return stale readiness or acknowledge unapplied records.
        self.reconcile_or_isolate();
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
