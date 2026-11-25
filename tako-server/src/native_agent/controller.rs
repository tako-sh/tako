use super::catalog::Catalog;
use super::{
    AgentCommand, AgentError, AgentRequest, AgentResponse, Result, authority,
    lifecycle::{Control, WithdrawOnDrop},
    now,
    process::{self, DnsStatus},
    projection,
    services::Publisher,
    store::{MonotonicStore, preserves},
};
use sha2::{Digest, Sha256};
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
    catalog::{MAX_REGISTRY_BYTES, RegistryStore},
    node_config::protected::Directory,
    runtime::{RuntimeConfig, RuntimeLimits, ServerRuntime},
};

pub(super) struct Work {
    pub request: AgentCommand,
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
            if controller.ready {
                if let Err(error) = now().and_then(|time| {
                    controller.service_store.borrow_mut().begin_request()?;
                    let result = controller
                        .runtime
                        .reconcile_record_keys(time)
                        .map_err(Into::into);
                    controller.service_store.borrow_mut().end_request();
                    result
                }) {
                    if let AgentError::Runtime(error) = &error {
                        controller.note_journal_error(error);
                    }
                    tracing::warn!(error = %error, "record key reconciliation pending");
                }
            }
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
                let guard = controller.service_store.borrow_mut().begin_request();
                if let Err(error) = guard {
                    let _ = work.reply.send(controller.response(Err(error.into())));
                    continue;
                }
                let response = match work.request {
                    AgentCommand::Rpc(request) => controller.handle(request),
                    AgentCommand::Recovery(request) => {
                        controller.recover_client(request.recover_client)
                    }
                    AgentCommand::Catalog { catalog } => {
                        let mut reply = AgentResponse::unavailable(&controller.boot);
                        if controller.ready {
                            match controller.service_store.borrow_mut().storage(catalog) {
                                Ok(value) => {
                                    reply.status = 200;
                                    reply.ready = true;
                                    reply.catalog = Some(value);
                                    reply.error = None;
                                }
                                Err(e) => reply.error = Some(e.to_string()),
                            }
                        }
                        reply
                    }
                    AgentCommand::Secrets(request) => controller.scope_secrets(request),
                };
                controller.service_store.borrow_mut().end_request();
                let _ = work.reply.send(response);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => (),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if controller.journal_failed.get() {
            tracing::error!("runtime journal write failed; restarting the agent to reopen it");
            break;
        }
    }
    let journal_failed = controller.journal_failed.get();
    control.withdraw();
    wait_for_broker(&control);
    // The stopped agent can no longer expire grants. Withdraw WireGuard and SSH
    // access while this controller still owns the host state and nonce DB lock.
    let isolation = controller.isolate();
    if let Err(error) = &isolation {
        tracing::error!(error = %error, "agent shutdown isolation not acknowledged");
    }
    isolation?;
    if journal_failed {
        // A failed exit makes the service manager start the agent again.
        return Err(AgentError::Invalid("runtime journal write failed"));
    }
    Ok(())
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
    runtime: ServerRuntime<MonotonicStore<Catalog>>,
    known: Rc<RefCell<RegistrySnapshot>>,
    service_store: Rc<RefCell<Catalog>>,
    publisher: Publisher,
    applied: ApplyCache,
    boot: BootId,
    ready: bool,
    revoked: bool,

    stop: Arc<AtomicBool>,
    renewal: Instant,
    control: Control,
    /// Set once a journal write failed on disk. The worker then exits, so
    /// the agent restarts and reopens it instead of failing every request.
    journal_failed: std::cell::Cell<bool>,
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
        renew_identity(&config, &root, &directory)?;
        for name in config.files.names() {
            directory.read(name, 128 * 1024)?;
        }
        let keys = LocalDeviceKeys::load(&root.join(&config.files.keys))?;
        if keys.device_id(&config.network.id, DeviceKind::Server)? != config.server_id {
            return Err(AgentError::Invalid(
                "configured keys do not identify server",
            ));
        }
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
        if marker && !directory.exists(JOURNAL)? {
            Self::initial_isolate(&config, &directory, &keys)?;
            return Err(AgentError::Invalid(
                "initialized database missing; operator restore required",
            ));
        }
        if marker {
            verify_existing_databases(&directory, &root)?;
        }
        let current_time = now()?;
        let desired = projection::recovery(&config, &keys, &known, current_time)?;
        directory.write("desired.json", &desired)?;
        process::apply(&config, &desired)?;
        let publisher = Publisher::open(&config, &data_dir)?;
        let startup = (|| -> Result<_> {
            let store = Catalog::start(&config, &known, &keys, &root, &directory, &stop)?;
            let shared = Rc::new(RefCell::new(known.clone()));
            let mut guarded =
                MonotonicStore::new(store, Directory::open(&root, true)?, shared.clone())
                    .with_stop(stop.clone());
            guarded.snapshot()?;
            if !directory.exists(JOURNAL)? {
                directory.write(JOURNAL, b"")?;
            }
            if !directory.exists("runtime.runtime-lock")? {
                directory.write("runtime.runtime-lock", b"")?;
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
                &root.join(JOURNAL),
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
            applied: ApplyCache::default(),
            boot,
            ready: false,
            revoked: false,
            stop,
            renewal: Instant::now() + RENEWAL_INTERVAL,
            control,
            journal_failed: std::cell::Cell::new(false),
        })
    }
    fn note_journal_error(&self, error: &RuntimeError) {
        if error.journal_needs_reopen() {
            self.journal_failed.set(true);
        }
    }
    fn initial_isolate(
        config: &NodeConfig,
        directory: &Directory,
        keys: &LocalDeviceKeys,
    ) -> Result<()> {
        let bytes = projection::desired(config, keys, &config.bootstrap, now()?, true)?;
        directory.write("desired.json", &bytes)?;
        process::apply(config, &bytes).map(|_| ())
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
        if self.applied.hash != Some(Sha256::digest(bytes).into()) {
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
        // A locally journaled denial may precede the catalog's voter change.
        // Withdraw management and app routes; keep the committed server-only
        // overlay available for the old quorum to finish the change.
        if snapshot
            .revocations
            .iter()
            .any(|r| !self.known.borrow().revocations.contains(r))
        {
            return Err(AgentError::Invalid(
                "waiting for committed network revocation",
            ));
        }
        if !self.control.matches(&snapshot) {
            self.withdraw();
        }
        let desired = projection::desired(&self.config, &self.keys, &snapshot, now, false)?;
        self.apply(&desired)?;
        if !serving(&self.config, &snapshot, now)? {
            // Empty desired state has already removed peers and SSH access.
            self.withdraw();
            self.service_store.borrow_mut().withdraw();
            self.revoked = true;
            self.isolate()?;
            return Ok(());
        }
        self.service_store
            .borrow_mut()
            .reconcile(&self.keys, &snapshot, now)?;
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
        let result = (|| {
            if Instant::now() >= self.renewal {
                renew_identity(
                    &self.config,
                    &self.config.network_dir(&self.data_dir),
                    &self.directory,
                )?;
                self.renewal = Instant::now() + RENEWAL_INTERVAL;
            }
            if self.revoked {
                return self.isolate();
            }
            self.reconcile()
        })();
        if let Err(error) = result {
            self.withdraw();
            tracing::warn!(%error,"Network catalog reconciliation pending");
            let result = if self.revoked {
                self.isolate()
            } else {
                let peers = now().and_then(|time| {
                    projection::recovery(&self.config, &self.keys, &self.known.borrow(), time)
                });
                peers.and_then(|bytes| self.apply(&bytes))
            };
            if let Err(error) = result {
                tracing::error!(%error,"Network recovery projection failed")
            }
        }
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
                &mut self.service_store.borrow_mut(),
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
    fn recheck_authority(&mut self, signed: &tako_network::api::SignedRequest) -> AgentResponse {
        if let Err(error) = authority::validate(&self.config, signed) {
            return self.response(Err(error));
        }
        if !self.ready || self.revoked || self.stop.load(Ordering::Acquire) {
            return AgentResponse::unavailable(&self.boot);
        }
        // Read through runtime's durable denial/clock guard, without replaying
        // handle(), consuming another nonce, or creating a management grant.
        let current = (|| -> Result<_> {
            let read_at = now()?;
            let snapshot = self.runtime.desired_host_state(read_at)?.snapshot;
            // Store I/O can outlast a member's remaining lifetime. Evaluate
            // expiry after that read, not against its earlier start timestamp.
            let time = now()?;
            if time < read_at {
                return Err(AgentError::Invalid(
                    "clock moved backwards during authority recheck",
                ));
            }
            // With zero remaining peers, expiry of this server may leave the
            // helper bytes unchanged. Serving authority is an independent gate.
            if !serving(&self.config, &snapshot, time)? {
                return Err(RuntimeError::Denied.into());
            }
            let desired = projection::desired(&self.config, &self.keys, &snapshot, time, false)?;
            if self.applied.hash != Some(Sha256::digest(&desired).into())
                || !self.control.matches(&snapshot)
                || self.stop.load(Ordering::Acquire)
            {
                return Err(AgentError::Invalid(
                    "management authority is not currently enforced",
                ));
            }
            Ok((snapshot, time))
        })();
        let (snapshot, time) = match current {
            Ok(value) => value,
            Err(error) => {
                self.withdraw();
                if let Err(isolation) = self.isolate() {
                    tracing::error!(error = %isolation, "management authority recheck isolation not acknowledged");
                }
                return self.response(Err(error));
            }
        };
        self.response(authority::check(
            &self.config,
            signed,
            &snapshot,
            time,
            self.ready,
            &self.control,
        ))
    }
    fn recover_client(
        &mut self,
        grant: tako_network::invitation::CreateInvitation,
    ) -> AgentResponse {
        self.reconcile_or_isolate();
        if !self.ready {
            return AgentResponse::unavailable(&self.boot);
        }
        let result = now().and_then(|time| {
            self.runtime
                .create_recovery_invitation(grant, time)
                .map_err(Into::into)
        });
        self.response(result)
    }

    fn scope_secrets(&mut self, request: super::SecretsRequest) -> AgentResponse {
        self.reconcile_or_isolate();
        if !self.ready {
            return AgentResponse::unavailable(&self.boot);
        }
        match self
            .runtime
            .scope_secrets(&request.secrets_scope, &request.published)
        {
            Ok(secrets) => AgentResponse {
                status: 200,
                response: None,
                catalog: None,
                secrets: Some(secrets),
                error: None,
                boot: self.boot.clone(),
                ready: self.ready,
            },
            Err(error) => {
                tracing::warn!("Could not read app environment secrets: {error}");
                self.response(Err(error.into()))
            }
        }
    }

    fn handle(&mut self, request: AgentRequest) -> AgentResponse {
        if authority::reserved_route(&request.method, &request.path) {
            if request.method == authority::METHOD && request.path == authority::PATH {
                return self.recheck_authority(&request.signed);
            }
            return self.response(Err(AgentError::Runtime(RuntimeError::Authentication)));
        }
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
                            | RuntimeError::Journal(_)
                            | RuntimeError::Catalog(_)
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
        self.response(result)
    }
    fn response(&self, result: Result<tako_network::api::RpcResponse>) -> AgentResponse {
        if let Err(AgentError::Runtime(error)) = &result {
            self.note_journal_error(error);
        }
        match result {
            Ok(response) => AgentResponse {
                status: 200,
                response: Some(response),
                catalog: None,
                secrets: None,
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
                    catalog: None,
                    secrets: None,
                    error: Some(message.into()),
                    boot: self.boot.clone(),
                    ready: self.ready,
                }
            }
        }
    }
}
/// No durable hash: each process boot must enforce its first snapshot.
/// Host DNS has separate ACK status and a completion-relative retry deadline.
const DNS_RETRY_INITIAL: Duration = Duration::from_secs(5);
const DNS_RETRY_MAX: Duration = Duration::from_secs(60);
#[derive(Default)]
struct ApplyCache {
    hash: Option<[u8; 32]>,
    dns_status: Option<DnsStatus>,
    dns_retry_at: Option<Instant>,
    dns_backoff: Duration,
}
impl ApplyCache {
    fn enforce_with_membership(
        &mut self,
        bytes: &[u8],
        control: &Control,
        apply: impl FnOnce() -> Result<DnsStatus>,
    ) -> Result<()> {
        self.enforce_with_membership_clock(bytes, control, Instant::now, apply)
    }

    fn enforce_with_membership_clock(
        &mut self,
        bytes: &[u8],
        control: &Control,
        clock: impl Fn() -> Instant,
        apply: impl FnOnce() -> Result<DnsStatus>,
    ) -> Result<()> {
        if self.hash != Some(Sha256::digest(bytes).into()) {
            control.withdraw();
        }
        let result = self.enforce_with_clock(bytes, clock, apply);
        if result.is_err() {
            // A retry that loses the membership ACK is a full enforcement error,
            // unlike an exact ACK carrying only degraded DNS status.
            control.withdraw();
        }
        result
    }

    #[cfg(test)]
    fn enforce(&mut self, bytes: &[u8], apply: impl FnOnce() -> Result<DnsStatus>) -> Result<()> {
        self.enforce_with_clock(bytes, Instant::now, apply)
    }

    fn enforce_with_clock(
        &mut self,
        bytes: &[u8],
        clock: impl Fn() -> Instant,
        apply: impl FnOnce() -> Result<DnsStatus>,
    ) -> Result<()> {
        let hash: [u8; 32] = Sha256::digest(bytes).into();
        let unchanged = self.hash == Some(hash);
        if unchanged
            && self.dns_status.as_ref().is_some_and(|dns| {
                dns.dns_supported || self.dns_retry_at.is_some_and(|deadline| clock() < deadline)
            })
        {
            return Ok(());
        }
        if !unchanged {
            self.dns_backoff = Duration::ZERO;
        }
        self.hash = None;
        self.dns_status = None;
        self.dns_retry_at = None;
        let dns = apply()?;
        if dns.dns_supported {
            self.dns_backoff = Duration::ZERO;
        } else {
            self.dns_backoff = if self.dns_backoff.is_zero() {
                DNS_RETRY_INITIAL
            } else {
                (self.dns_backoff * 2).min(DNS_RETRY_MAX)
            };
            // A slow helper must not make its own retry immediately overdue.
            self.dns_retry_at = Some(clock() + self.dns_backoff);
        }
        self.hash = Some(hash);
        self.dns_status = Some(dns);
        Ok(())
    }
}

const RENEWAL_INTERVAL: Duration = Duration::from_secs(3600);
fn renew_identity(config: &NodeConfig, root: &Path, _directory: &Directory) -> Result<()> {
    let identity = ServerIdentity::renew(root)?;
    let https = identity.tls_paths();
    // NodeFiles stay explicit. Accept either managed leaf or fullchain references,
    // but never renew one identity while actually serving unrelated files.
    let https_file = root.join(&config.files.https_certificate);
    if root.join(&config.files.keys) != identity.keys_path()
        || (https_file != https.certificate && https_file != https.fullchain)
        || root.join(&config.files.https_private_key) != https.private_key
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
    Ok(())
}

fn serving(config: &NodeConfig, snapshot: &RegistrySnapshot, now: u64) -> Result<bool> {
    Ok(snapshot
        .effective_members(now)?
        .get(&config.server_id)
        .is_some_and(|member| {
            matches!(member.kind, MemberKind::Server(_)) && member.permissions.is_full()
        }))
}

fn allowed_while_unready(request: &RpcRequest) -> bool {
    matches!(request, RpcRequest::Snapshot | RpcRequest::Revoke { .. })
}

const JOURNAL: &str = "runtime.redb";

fn verify_existing_databases(directory: &Directory, root: &Path) -> Result<()> {
    // Refuses a journal file that is linked, foreign or not owner-only.
    directory.exists(JOURNAL)?;
    // Runtime::open creates an empty journal for first bootstrap; do not let
    // that recreate an erased nonce journal on a previously initialized node.
    tako_network::runtime::verify_journal(&root.join(JOURNAL)).map_err(|error| {
        tracing::error!(error = %error, "runtime journal failed verification");
        AgentError::Invalid("runtime journal requires operator restore")
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy_dns() -> DnsStatus {
        DnsStatus {
            dns_supported: true,
            dns_error: None,
        }
    }
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
                Ok(healthy_dns())
            })
            .unwrap();
        // Even a helper ACK is insufficient until runtime's final state check.
        assert!(members.borrow().is_none());
        assert!(control.publish_enforced(&fixture.config.bootstrap));
        assert!(members.borrow().is_some());
    }
    #[test]
    fn inactive_resolver_recovers_by_retrying_identical_desired_state() {
        use std::cell::Cell;
        let fixture = super::super::tests::Fixture::new();
        let desired = projection::desired(
            &fixture.config,
            &fixture.keys,
            &fixture.config.bootstrap,
            2,
            false,
        )
        .unwrap();
        let state: serde_json::Value = serde_json::from_slice(&desired).unwrap();
        let network = fixture.config.network.id.to_string();
        let mut ack = serde_json::json!({
            "ok": true, "action": "apply", "network_id": network,
            "interface": format!("tako{}", &network[..10]),
            "address": state["address"].as_str().unwrap().trim_end_matches("/128"),
            "applied_device_ids": state["peers"].as_array().unwrap().iter()
                .map(|peer| peer["device_id"].clone()).collect::<Vec<_>>(),
            "dns_supported": false,
            "dns_error": "native service DNS requires active systemd-resolved"
        });
        let (control, mut members, _) = Control::new();
        let mut cache = ApplyCache::default();
        let clock = Cell::new(Instant::now());
        let calls = Cell::new(0);
        let apply = |cache: &mut ApplyCache, ack: &serde_json::Value| {
            cache
                .enforce_with_membership_clock(
                    &desired,
                    &control,
                    || clock.get(),
                    || {
                        calls.set(calls.get() + 1);
                        process::validate_helper_response(
                            &fixture.config,
                            &desired,
                            &serde_json::to_vec(ack).unwrap(),
                        )
                    },
                )
                .unwrap();
        };
        apply(&mut cache, &ack);
        assert_eq!(calls.get(), 1);
        let dns = cache.dns_status.as_ref().unwrap();
        assert!(!dns.dns_supported);
        assert_eq!(
            dns.dns_error.as_deref(),
            Some("native service DNS requires active systemd-resolved")
        );
        assert!(control.publish_enforced(&fixture.config.bootstrap));
        assert!(members.borrow_and_update().is_some());
        let due = cache.dns_retry_at.unwrap();
        // Resolved recovers without any membership/config change. RPC-triggered
        // reconciles cannot bypass the DNS backoff or withdraw healthy membership.
        ack["dns_supported"] = serde_json::json!(true);
        ack.as_object_mut().unwrap().remove("dns_error");
        clock.set(due - Duration::from_millis(1));
        apply(&mut cache, &ack);
        assert_eq!(calls.get(), 1);
        assert!(cache.dns_status.as_ref().unwrap().dns_error.is_some());
        clock.set(due);
        apply(&mut cache, &ack);
        assert_eq!(calls.get(), 2);
        assert_eq!(cache.dns_status, Some(healthy_dns()));
        assert!(cache.dns_retry_at.is_none());
        assert_eq!(cache.dns_backoff, Duration::ZERO);
        assert!(members.borrow().is_some());
        assert!(!members.has_changed().unwrap());
        clock.set(due + Duration::from_secs(3600));
        apply(&mut cache, &ack);
        assert_eq!(calls.get(), 2); // stable successful DNS can now be cached
    }

    #[test]
    fn dns_retry_backoff_is_capped_completion_relative_and_resets_for_new_policy() {
        use std::cell::Cell;
        let mut cache = ApplyCache::default();
        let clock = Cell::new(Instant::now());
        for seconds in [5, 10, 20, 40, 60, 60] {
            cache
                .enforce_with_clock(
                    b"policy",
                    || clock.get(),
                    || {
                        clock.set(clock.get() + Duration::from_secs(45)); // slow helper
                        Ok(DnsStatus {
                            dns_supported: false,
                            dns_error: Some("resolver unavailable".into()),
                        })
                    },
                )
                .unwrap();
            assert_eq!(
                cache.dns_retry_at,
                Some(clock.get() + Duration::from_secs(seconds))
            );
            clock.set(cache.dns_retry_at.unwrap());
        }
        cache
            .enforce_with_clock(
                b"new-policy",
                || clock.get(),
                || {
                    Ok(DnsStatus {
                        dns_supported: false,
                        dns_error: Some("resolver unavailable".into()),
                    })
                },
            )
            .unwrap();
        assert_eq!(cache.dns_retry_at, Some(clock.get() + DNS_RETRY_INITIAL));
    }

    #[test]
    fn failed_dns_retry_invalidates_membership_ack_and_does_not_cache_failure() {
        let fixture = super::super::tests::Fixture::new();
        let (control, members, _) = Control::new();
        let mut cache = ApplyCache::default();
        let start = Instant::now();
        cache
            .enforce_with_membership_clock(
                b"policy",
                &control,
                || start,
                || {
                    Ok(DnsStatus {
                        dns_supported: false,
                        dns_error: Some("resolver unavailable".into()),
                    })
                },
            )
            .unwrap();
        control.publish_enforced(&fixture.config.bootstrap);
        let due = cache.dns_retry_at.unwrap();
        assert!(
            cache
                .enforce_with_membership_clock(
                    b"policy",
                    &control,
                    || due,
                    || Err(AgentError::Timeout)
                )
                .is_err()
        );
        assert!(members.borrow().is_none());
        assert!(cache.hash.is_none());
        assert!(cache.dns_status.is_none());
        cache
            .enforce_with_membership_clock(b"policy", &control, || due, || Ok(healthy_dns()))
            .unwrap();
        // Helper success alone cannot republish until runtime's final checks.
        assert!(members.borrow().is_none());
        assert_eq!(cache.dns_status, Some(healthy_dns()));
    }

    #[test]
    fn startup_and_periodic_renewal_validate_paths_pins_and_fullchain() {
        let fixture = super::super::tests::Fixture::new();
        let mut config = fixture.config;
        let temp = crate::isolation::fixture::private_tempdir();
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
        let before = std::fs::metadata(&identity.tls_paths().fullchain)
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(first, renew_identity(&config, &root, &directory).unwrap());
        assert_eq!(
            before,
            std::fs::metadata(&identity.tls_paths().fullchain)
                .unwrap()
                .modified()
                .unwrap()
        );
        let mut wrong = config.clone();
        wrong.files.https_certificate = "unmanaged.pem".into();
        assert!(renew_identity(&wrong, &root, &directory).is_err());
        let MemberKind::Server(info) = &mut wrong.bootstrap.members[1].kind else {
            unreachable!()
        };
        info.https_port = 8443;
        wrong.files.https_certificate = config.files.https_certificate.clone();
        assert!(renew_identity(&wrong, &root, &directory).is_err());
        directory
            .write(&config.files.https_certificate, b"corrupt fullchain")
            .unwrap();
        assert!(renew_identity(&config, &root, &directory).is_err());
    }
    #[test]
    fn serving_expiry_is_checked_even_when_empty_host_projection_is_unchanged() {
        let fixture = super::super::tests::Fixture::new();
        let mut snapshot = fixture.config.bootstrap.clone();
        snapshot.members[0].expires_at = Some(2);
        snapshot.members[1].expires_at = Some(3);
        let before =
            projection::desired(&fixture.config, &fixture.keys, &snapshot, 2, false).unwrap();
        let after =
            projection::desired(&fixture.config, &fixture.keys, &snapshot, 3, false).unwrap();
        assert_eq!(&*before, &*after);
        assert!(serving(&fixture.config, &snapshot, 2).unwrap());
        assert!(!serving(&fixture.config, &snapshot, 3).unwrap());
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
    fn helper_failure_is_not_cached_and_each_boot_applies() {
        let mut cache = ApplyCache::default();
        assert!(
            cache
                .enforce(b"snapshot-a", || Err(AgentError::Timeout))
                .is_err()
        );
        assert!(cache.hash.is_none());
        cache.enforce(b"snapshot-a", || Ok(healthy_dns())).unwrap();
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
                Ok(healthy_dns())
            })
            .unwrap();
        assert!(called);
        let mut reboot = ApplyCache::default();
        called = false;
        reboot
            .enforce(b"snapshot-a", || {
                called = true;
                Ok(healthy_dns())
            })
            .unwrap();
        assert!(called);
    }
}
