//! Serialized native authorization and enrollment state. The parent owns TLS,
//! the clock, local-operator bootstrap, privileged reconciliation, and readiness.
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    net::Ipv6Addr,
    path::Path,
    time::Duration,
};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::{
    Ack, Approval, BootId, DeviceId, DeviceKind, LocalDeviceKeys, Member, MemberKind,
    NetworkDescriptor, NetworkPublicProfile, PendingApprovalProfile, PendingWgChallenge,
    Permissions, RegistrySnapshot, RequestId, ServerInfo, SshPublicKey, SshUser, WgChallenge,
    WgPublicKey,
    api::{ApproveRequest, RpcRequest, RpcResponse, RuntimeError, RuntimeResult, SignedRequest},
    corrosion::{
        MAX_RECORD_BYTES, MAX_REGISTRY_BYTES, MAX_REGISTRY_ROWS, RegistryStore, bound_projection,
        bounded_json_size,
    },
};

#[derive(Clone, Debug)]
pub struct RuntimeLimits {
    pub max_pending: usize,
    pub max_challenges: usize,
    /// Each replay pool holds at most this many nonces. Revocations use a separate
    /// pool, and each signer gets at most ceil(max_replays / 16) in either pool.
    pub max_replays: usize,
    pub auth_window_seconds: u64,
    pub challenge_seconds: u64,
    pub entitlement_seconds: u64,
}

impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            max_pending: 1024,
            max_challenges: 256,
            max_replays: 20_000,
            auth_window_seconds: 120,
            challenge_seconds: 120,
            entitlement_seconds: 7 * 86400,
        }
    }
}

impl RuntimeLimits {
    fn validate(&self) -> RuntimeResult<()> {
        if self.max_pending == 0
            || self.max_pending > 10_000
            || self.max_challenges == 0
            || self.max_challenges > 4096
            || self.max_replays == 0
            || self.max_replays > 100_000
            || self.auth_window_seconds == 0
            || self.auth_window_seconds > 300
            || self.challenge_seconds == 0
            || self.challenge_seconds > 300
            || self.entitlement_seconds == 0
            || self.entitlement_seconds > 30 * 86400
        {
            return Err(RuntimeError::Invalid("runtime limits"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    pub network: NetworkDescriptor,
    pub server_id: DeviceId,
    pub boot_id: BootId,
    pub limits: RuntimeLimits,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entitlement {
    grant: ApproveRequest,
    approver: DeviceId,
    candidate: Option<Member>,
}

struct ChallengeState {
    pending_id: RequestId,
    expires_at: u64,
    proof: PendingWgChallenge,
}

enum RegistryOperation {
    Snapshot,
    Mutation,
}

pub struct ServerRuntime<S: RegistryStore> {
    store: S,
    keys: LocalDeviceKeys,
    config: RuntimeConfig,
    db: Connection,
    challenges: BTreeMap<RequestId, ChallengeState>,
    _lock: File,
}

pub struct HostDesiredState {
    pub private_key: Zeroizing<String>,
    pub address: Ipv6Addr,
    pub peers: Vec<HostPeer>,
    pub ssh_authorizations: Vec<SshAuthorization>,
    pub snapshot: RegistrySnapshot,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostPeer {
    pub device_id: DeviceId,
    pub public_key: WgPublicKey,
    pub allowed_ip: String,
    pub endpoint: Option<std::net::SocketAddr>,
    pub permissions: Permissions,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshAuthorization {
    pub device_id: DeviceId,
    pub user: SshUser,
    pub public_key: SshPublicKey,
}

impl<S: RegistryStore> ServerRuntime<S> {
    /// `state_path` must be inside a protected, owner-only server state directory.
    /// Keep this runtime on a blocking worker; its process lock lasts until drop.
    pub fn open(
        store: S,
        keys: LocalDeviceKeys,
        config: RuntimeConfig,
        state_path: &Path,
    ) -> RuntimeResult<Self> {
        config.limits.validate()?;
        if keys.device_id(&config.network.id, DeviceKind::Server)? != config.server_id {
            return Err(RuntimeError::Invalid("server keys do not match config"));
        }
        let lock_path = state_path.with_extension("runtime-lock");
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
        }
        let lock = options.open(lock_path)?;
        lock.try_lock()
            .map_err(|_| RuntimeError::Invalid("runtime database already in use"))?;
        let database_file = options.open(state_path)?;
        if !database_file.metadata()?.is_file() {
            return Err(RuntimeError::Invalid(
                "runtime database is not a regular file",
            ));
        }
        let db = Connection::open(state_path)?;
        db.busy_timeout(Duration::from_secs(3))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS runtime_identity (singleton INTEGER PRIMARY KEY CHECK(singleton=1), network TEXT NOT NULL, server TEXT NOT NULL, last_time INTEGER NOT NULL, auth_window INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS consumed_nonces (signer TEXT NOT NULL, nonce TEXT NOT NULL, expires INTEGER NOT NULL, PRIMARY KEY(signer, nonce));
            CREATE INDEX IF NOT EXISTS nonce_expiry ON consumed_nonces(expires);
            CREATE TABLE IF NOT EXISTS observed_records (kind TEXT NOT NULL, id TEXT NOT NULL, payload TEXT NOT NULL, PRIMARY KEY(kind, id));
            CREATE TABLE IF NOT EXISTS registry_failure (singleton INTEGER PRIMARY KEY CHECK(singleton=1));
            CREATE TABLE IF NOT EXISTS revocation_nonces (signer TEXT NOT NULL, nonce TEXT NOT NULL, expires INTEGER NOT NULL, PRIMARY KEY(signer, nonce));
            CREATE INDEX IF NOT EXISTS revocation_nonce_expiry ON revocation_nonces(expires);
            CREATE TABLE IF NOT EXISTS pending_entitlements (id TEXT PRIMARY KEY, expires INTEGER NOT NULL, payload TEXT NOT NULL);")?;
        let network = serde_json::to_string(&config.network)?;
        db.execute(
            "INSERT OR IGNORE INTO runtime_identity VALUES (1, ?1, ?2, 0, ?3)",
            params![
                network,
                config.server_id.to_string(),
                sql_time(config.limits.auth_window_seconds)?
            ],
        )?;
        let identity: (String, String, i64) = db.query_row(
            "SELECT network, server, auth_window FROM runtime_identity WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        if identity
            != (
                network,
                config.server_id.to_string(),
                sql_time(config.limits.auth_window_seconds)?,
            )
        {
            return Err(RuntimeError::Conflict("local runtime identity"));
        }
        let mut runtime = Self {
            store,
            keys,
            config,
            db,
            challenges: BTreeMap::new(),
            _lock: lock,
        };
        runtime.snapshot()?;
        Ok(runtime)
    }

    pub fn snapshot(&mut self) -> RuntimeResult<RegistrySnapshot> {
        self.with_registry(RegistryOperation::Snapshot, |runtime| {
            let snapshot = runtime.store.snapshot()?;
            runtime.observe(snapshot)
        })
    }

    fn with_registry<T>(
        &mut self,
        operation: RegistryOperation,
        action: impl FnOnce(&mut Self) -> RuntimeResult<T>,
    ) -> RuntimeResult<T> {
        let failed: bool =
            self.db
                .query_row("SELECT EXISTS(SELECT 1 FROM registry_failure)", [], |r| {
                    r.get(0)
                })?;
        if failed {
            return Err(RuntimeError::Capacity("registry requires local recovery"));
        }
        let result = action(self);
        if matches!(result, Err(RuntimeError::Capacity(reason))
            if matches!(operation, RegistryOperation::Snapshot)
                || (reason != "registry admissions" && reason != "registry record"))
        {
            self.db
                .execute("INSERT OR IGNORE INTO registry_failure VALUES (1)", [])?;
        }
        result
    }

    fn observe(&mut self, mut snapshot: RegistrySnapshot) -> RuntimeResult<RegistrySnapshot> {
        bound_projection(&mut snapshot)?;
        validate_snapshot(&snapshot, &self.config.network)?;
        let tx = self.db.transaction()?;
        for record in snapshot
            .members
            .iter()
            .map(|m| -> RuntimeResult<_> {
                Ok(("member", m.device_id.to_string(), serde_json::to_string(m)?))
            })
            .chain(
                snapshot
                    .revocations
                    .iter()
                    .map(|r| Ok(("revocation", r.id.to_string(), serde_json::to_string(r)?))),
            )
        {
            let (kind, id, payload) = record?;
            let previous: Option<String> = tx
                .query_row(
                    "SELECT payload FROM observed_records WHERE kind=?1 AND id=?2",
                    params![kind, id],
                    |r| r.get(0),
                )
                .optional()?;
            if previous.as_ref().is_some_and(|old| old != &payload) {
                return Err(RuntimeError::Conflict("observed registry record"));
            }
            tx.execute(
                "INSERT OR IGNORE INTO observed_records VALUES (?1, ?2, ?3)",
                params![kind, id, payload],
            )?;
        }
        snapshot.members.clear();
        snapshot.revocations.clear();
        {
            let mut statement =
                tx.prepare("SELECT kind, payload FROM observed_records ORDER BY kind, id")?;
            let mut rows = statement.query([])?;
            let mut bytes = 0usize;
            let mut count = 0usize;
            while let Some(row) = rows.next()? {
                let kind: String = row.get(0)?;
                let payload: String = row.get(1)?;
                bytes += payload.len();
                count += 1;
                if bytes > MAX_REGISTRY_BYTES || count > MAX_REGISTRY_ROWS {
                    return Err(RuntimeError::Capacity("observed registry"));
                }
                match kind.as_str() {
                    "member" => snapshot.members.push(serde_json::from_str(&payload)?),
                    "revocation" => snapshot.revocations.push(serde_json::from_str(&payload)?),
                    _ => return Err(RuntimeError::Invalid("observed registry kind")),
                }
            }
        }
        bound_projection(&mut snapshot)?;
        validate_snapshot(&snapshot, &self.config.network)?;
        tx.commit()?;
        Ok(snapshot)
    }

    fn advance_clock(&self, now: u64) -> RuntimeResult<()> {
        let now = sql_time(now)?;
        let updated = self.db.execute(
            "UPDATE runtime_identity SET last_time=?1 WHERE singleton=1 AND last_time>=0 AND last_time<=?1",
            [now],
        )?;
        if updated != 1 {
            return Err(RuntimeError::Invalid("server clock moved backwards"));
        }
        Ok(())
    }

    pub fn handle(
        &mut self,
        signed: SignedRequest,
        method: &str,
        path: &str,
        now: u64,
    ) -> RuntimeResult<RpcResponse> {
        signed.check_size()?;
        if signed.network_id != self.config.network.id
            || signed.server_id != self.config.server_id
            || method != "POST"
            || path != signed.request.path()
            || now > i64::MAX as u64 - 600
            || signed.timestamp.abs_diff(now) > self.config.limits.auth_window_seconds
        {
            return Err(RuntimeError::Authentication);
        }
        self.advance_clock(now)?;
        let snapshot = self.snapshot()?;
        // Re-publication of a durable denial needs identity proof, not restored
        // effective authority. It can only publish the exact stored record.
        if let RpcRequest::Revoke { id, target } = &signed.request
            && let Some(previous) = snapshot.revocations.iter().find(|r| &r.id == id)
        {
            if previous.by != signed.signer || &previous.target != target {
                return Err(RuntimeError::Conflict("revocation ID"));
            }
            let member = snapshot
                .members
                .iter()
                .find(|m| m.device_id == signed.signer)
                .ok_or(RuntimeError::Denied)?;
            signed.verify(&member.request.ssh_public_key)?;
            if !member.wg_verified
                || !member.permissions.network_admin
                || (snapshot
                    .members
                    .iter()
                    .any(|m| &m.device_id == target && matches!(m.kind, MemberKind::Server(_)))
                    && !member.permissions.server_admin)
            {
                return Err(RuntimeError::Denied);
            }
            self.consume_nonce(&signed, now)?;
            self.with_registry(RegistryOperation::Mutation, |runtime| {
                runtime.store.append_revocation(previous)
            })?;
            return Ok(RpcResponse::Revoked(previous.clone()));
        }
        self.require_server(&snapshot, now)?;
        let entitlement = match &signed.request {
            RpcRequest::Begin { pending_id } | RpcRequest::Complete { pending_id, .. } => {
                let entitlement = self.pending(pending_id, now)?;
                if entitlement
                    .grant
                    .request
                    .device_id_for(&self.config.network.id)?
                    != signed.signer
                {
                    return Err(RuntimeError::Authentication);
                }
                signed.verify(&entitlement.grant.request.ssh_public_key)?;
                Some(entitlement)
            }
            _ => {
                let effective = snapshot.effective_members(now)?;
                let member = effective.get(&signed.signer).ok_or(RuntimeError::Denied)?;
                signed.verify(&member.request.ssh_public_key)?;
                if matches!(
                    signed.request,
                    RpcRequest::Approve(_) | RpcRequest::Revoke { .. }
                ) && !member.permissions.network_admin
                {
                    return Err(RuntimeError::Denied);
                }
                None
            }
        };
        self.consume_nonce(&signed, now)?;
        self.challenges
            .retain(|_, challenge| challenge.expires_at > now);
        match signed.request {
            RpcRequest::Authorize { action } => {
                action.validate()?;
                let permissions = snapshot.permissions(&signed.signer, now)?.clone();
                Ok(RpcResponse::Authorized {
                    signer: signed.signer,
                    permissions,
                    action,
                })
            }
            RpcRequest::Snapshot => Ok(RpcResponse::Snapshot(snapshot)),
            RpcRequest::Approve(grant) => {
                self.approve_pending(*grant, signed.signer, snapshot, now)
            }
            RpcRequest::Begin { pending_id } => self.begin(
                pending_id,
                entitlement.ok_or(RuntimeError::PendingUnavailable)?,
                snapshot,
                now,
            ),
            RpcRequest::Complete {
                pending_id,
                challenge_id,
                proof,
            } => self.complete(
                pending_id,
                challenge_id,
                proof,
                entitlement.ok_or(RuntimeError::PendingUnavailable)?,
                snapshot,
                now,
            ),
            RpcRequest::Revoke { id, target } => {
                let revocation = snapshot.plan_revocation(id, &signed.signer, &target, now)?;
                let mut observed = snapshot;
                if !observed.revocations.contains(&revocation) {
                    observed.revocations.push(revocation.clone());
                }
                self.with_registry(RegistryOperation::Mutation, |runtime| {
                    runtime.observe(observed)?;
                    runtime.store.append_revocation(&revocation)
                })?;
                Ok(RpcResponse::Revoked(revocation))
            }
        }
    }

    fn require_server(&self, snapshot: &RegistrySnapshot, now: u64) -> RuntimeResult<()> {
        let effective = snapshot.effective_members(now)?;
        if !effective.get(&self.config.server_id).is_some_and(|member| {
            matches!(member.kind, MemberKind::Server(_)) && member.permissions.server_admin
        }) {
            return Err(RuntimeError::Denied);
        }
        Ok(())
    }

    fn consume_nonce(&mut self, signed: &SignedRequest, now: u64) -> RuntimeResult<()> {
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM consumed_nonces WHERE expires < ?1",
            [sql_time(now)?],
        )?;
        tx.execute(
            "DELETE FROM revocation_nonces WHERE expires < ?1",
            [sql_time(now)?],
        )?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM consumed_nonces WHERE signer=?1 AND nonce=?2 UNION ALL SELECT 1 FROM revocation_nonces WHERE signer=?1 AND nonce=?2)",
            params![signed.signer.to_string(), signed.nonce.to_string()],
            |r| r.get(0),
        )?;
        if exists {
            return Err(RuntimeError::Replay);
        }
        let emergency = matches!(signed.request, RpcRequest::Revoke { .. });
        let table = if emergency {
            "revocation_nonces"
        } else {
            "consumed_nonces"
        };
        let (count, principal): (i64, i64) = tx.query_row(
            &format!("SELECT count(*), coalesce(sum(signer=?1), 0) FROM {table}"),
            [signed.signer.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let principal_limit = self.config.limits.max_replays.div_ceil(16);
        if count >= self.config.limits.max_replays as i64 || principal >= principal_limit as i64 {
            return Err(RuntimeError::Capacity("replays"));
        }
        tx.execute(
            &format!("INSERT INTO {table} VALUES (?1, ?2, ?3)"),
            params![
                signed.signer.to_string(),
                signed.nonce.to_string(),
                sql_time(signed.timestamp + self.config.limits.auth_window_seconds)?
            ],
        )?;
        tx.execute(
            "DELETE FROM pending_entitlements WHERE expires <= ?1",
            [sql_time(now)?],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn pending(&self, id: &RequestId, now: u64) -> RuntimeResult<Entitlement> {
        let payload: Option<String> = self
            .db
            .query_row(
                "SELECT payload FROM pending_entitlements WHERE id=?1 AND expires>?2",
                params![id.to_string(), sql_time(now)?],
                |r| r.get(0),
            )
            .optional()?;
        let payload = payload.ok_or(RuntimeError::PendingUnavailable)?;
        if payload.len() > MAX_RECORD_BYTES * 2 {
            return Err(RuntimeError::Capacity("pending record"));
        }
        Ok(serde_json::from_str(&payload)?)
    }

    fn approve_pending(
        &mut self,
        grant: ApproveRequest,
        approver: DeviceId,
        snapshot: RegistrySnapshot,
        now: u64,
    ) -> RuntimeResult<RpcResponse> {
        let entitlement = Entitlement {
            grant,
            approver,
            candidate: None,
        };
        self.check_grant(&entitlement, &snapshot, now)?;
        if entitlement.grant.expires_at - now > self.config.limits.entitlement_seconds {
            return Err(RuntimeError::Invalid(
                "entitlement exceeds maximum lifetime",
            ));
        }
        let pins = server_pins(&snapshot, now)?;
        if !pins
            .get(&self.config.server_id)
            .is_some_and(|s| s.https_certificate_pem.is_some())
        {
            return Err(RuntimeError::Invalid(
                "issuer requires a public HTTPS certificate",
            ));
        }
        let existing = self
            .db
            .query_row(
                "SELECT payload FROM pending_entitlements WHERE id=?1",
                [entitlement.grant.pending_id.to_string()],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            let previous: Entitlement = serde_json::from_str(&existing)?;
            if previous.grant != entitlement.grant || previous.approver != entitlement.approver {
                return Err(RuntimeError::Conflict("pending ID"));
            }
        } else {
            let count: i64 =
                self.db
                    .query_row("SELECT count(*) FROM pending_entitlements", [], |r| {
                        r.get(0)
                    })?;
            if count >= self.config.limits.max_pending as i64 {
                return Err(RuntimeError::Capacity("pending entitlements"));
            }
            self.save_pending(&entitlement)?;
        }
        Ok(RpcResponse::Pending(PendingApprovalProfile {
            request: entitlement.grant.request,
            pending_id: entitlement.grant.pending_id,
            issuer: self.config.server_id.clone(),
            expires_at: entitlement.grant.expires_at,
            trusted_servers: pins,
            snapshot,
        }))
    }

    fn save_pending(&self, entitlement: &Entitlement) -> RuntimeResult<()> {
        let payload = serde_json::to_string(entitlement)?;
        if payload.len() > MAX_RECORD_BYTES * 2 {
            return Err(RuntimeError::Capacity("pending record"));
        }
        self.db.execute("INSERT INTO pending_entitlements VALUES (?1, ?2, ?3) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload", params![entitlement.grant.pending_id.to_string(), sql_time(entitlement.grant.expires_at)?, payload])?;
        Ok(())
    }

    fn check_grant(
        &self,
        entitlement: &Entitlement,
        snapshot: &RegistrySnapshot,
        now: u64,
    ) -> RuntimeResult<()> {
        let grant = &entitlement.grant;
        grant.request.verify()?;
        grant.permissions.validate()?;
        let id = grant.request.device_id_for(&self.config.network.id)?;
        if grant.kind.device_kind() != grant.request.kind || id == entitlement.approver {
            return Err(RuntimeError::Invalid("grant kind or self-approval"));
        }
        if now >= grant.expires_at
            || grant.expires_at > i64::MAX as u64
            || grant
                .request
                .expires_at
                .is_some_and(|e| e <= now || grant.expires_at > e)
            || grant.member_expires_at.is_some_and(|e| e <= now)
        {
            return Err(RuntimeError::PendingUnavailable);
        }
        if let MemberKind::Server(info) = &grant.kind {
            info.validate()?;
        }
        let effective = snapshot.effective_members(now)?;
        let parent = effective
            .get(&entitlement.approver)
            .ok_or(RuntimeError::Denied)?;
        let rights = &parent.permissions;
        if !rights.network_admin
            || (grant.permissions.server_admin && !rights.server_admin)
            || (matches!(grant.kind, MemberKind::Server(_)) && !rights.server_admin)
            || (grant.permissions.ssh_user.is_some()
                && !rights.server_admin
                && grant.permissions.ssh_user != rights.ssh_user)
            || (!rights.server_admin
                && !grant.permissions.deployments.is_subset(&rights.deployments))
            || parent
                .expires_at
                .is_some_and(|e| grant.member_expires_at.is_none_or(|child| child > e))
        {
            return Err(RuntimeError::Denied);
        }
        let wg_fingerprint = grant.request.wg_public_key.fingerprint();
        let ssh_fingerprint = grant.request.ssh_public_key.fingerprint()?;
        if snapshot.revocations.iter().any(|revocation| {
            revocation.target == id
                || revocation.wg_fingerprint == wg_fingerprint
                || revocation.ssh_fingerprint == ssh_fingerprint
        }) {
            return Err(RuntimeError::Denied);
        }
        Ok(())
    }

    fn active_profile(
        &self,
        entitlement: &Entitlement,
        snapshot: &RegistrySnapshot,
        now: u64,
    ) -> RuntimeResult<Option<NetworkPublicProfile>> {
        let Some(candidate) = &entitlement.candidate else {
            return Ok(None);
        };
        let Some(existing) = snapshot
            .members
            .iter()
            .find(|m| m.device_id == candidate.device_id)
        else {
            return Ok(None);
        };
        if existing != candidate {
            return Err(RuntimeError::Conflict("activated member"));
        }
        if !snapshot
            .effective_members(now)?
            .contains_key(&candidate.device_id)
        {
            return Err(RuntimeError::Denied);
        }
        Ok(Some(NetworkPublicProfile {
            request: entitlement.grant.request.clone(),
            trusted_servers: server_pins(snapshot, now)?,
            snapshot: snapshot.clone(),
        }))
    }

    fn begin(
        &mut self,
        pending_id: RequestId,
        entitlement: Entitlement,
        snapshot: RegistrySnapshot,
        now: u64,
    ) -> RuntimeResult<RpcResponse> {
        self.check_grant(&entitlement, &snapshot, now)?;
        if let Some(profile) = self.active_profile(&entitlement, &snapshot, now)? {
            return Ok(RpcResponse::Active(profile));
        }
        self.challenges
            .retain(|_, state| state.pending_id != pending_id);
        if self.challenges.len() >= self.config.limits.max_challenges {
            return Err(RuntimeError::Capacity("WG challenges"));
        }
        let expires =
            (now + self.config.limits.challenge_seconds).min(entitlement.grant.expires_at);
        let (challenge, proof) = WgChallenge::begin(
            &entitlement.grant.request,
            self.config.network.id.clone(),
            now,
            expires,
        )?;
        let challenge_id = RequestId::random()?;
        self.challenges.insert(
            challenge_id.clone(),
            ChallengeState {
                pending_id,
                expires_at: expires,
                proof,
            },
        );
        Ok(RpcResponse::Challenge {
            challenge_id,
            challenge,
        })
    }

    fn complete(
        &mut self,
        pending_id: RequestId,
        challenge_id: RequestId,
        proof: crate::WgProof,
        mut entitlement: Entitlement,
        snapshot: RegistrySnapshot,
        now: u64,
    ) -> RuntimeResult<RpcResponse> {
        self.check_grant(&entitlement, &snapshot, now)?;
        if let Some(profile) = self.active_profile(&entitlement, &snapshot, now)? {
            return Ok(RpcResponse::Active(profile));
        }
        // Check ownership before consuming another applicant's challenge.
        let state = self
            .challenges
            .get(&challenge_id)
            .ok_or(RuntimeError::ChallengeUnavailable)?;
        if state.pending_id != pending_id {
            return Err(RuntimeError::ChallengeUnavailable);
        }
        let state = self
            .challenges
            .remove(&challenge_id)
            .ok_or(RuntimeError::ChallengeUnavailable)?;
        let verified = state.proof.verify(&proof, now)?;
        let candidate = if let Some(candidate) = &entitlement.candidate {
            let approved = snapshot.approve(
                verified,
                Approval {
                    kind: candidate.kind.clone(),
                    permissions: candidate.permissions.clone(),
                    approved_by: candidate.approved_by.clone(),
                    independent_admin: false,
                    created_at: candidate.created_at,
                    expires_at: candidate.expires_at,
                },
            )?;
            if &approved != candidate {
                return Err(RuntimeError::Conflict("activation journal"));
            }
            candidate.clone()
        } else {
            snapshot.approve(
                verified,
                Approval {
                    kind: entitlement.grant.kind.clone(),
                    permissions: entitlement.grant.permissions.clone(),
                    approved_by: Some(entitlement.approver.clone()),
                    independent_admin: false,
                    created_at: now,
                    expires_at: entitlement.grant.member_expires_at,
                },
            )?
        };
        // FULL synchronous SQLite commit precedes the remote write. An uncertain
        // HTTP outcome can therefore only retry this exact immutable admission.
        entitlement.candidate = Some(candidate.clone());
        self.save_pending(&entitlement)?;
        self.with_registry(RegistryOperation::Mutation, |runtime| {
            runtime.store.append_member(&candidate)
        })?;
        let updated = self.snapshot()?;
        self.require_server(&updated, now)?;
        self.check_grant(&entitlement, &updated, now)?;
        let profile = self
            .active_profile(&entitlement, &updated, now)?
            .ok_or(RuntimeError::Corrosion("admission absent after write"))?;
        Ok(RpcResponse::Active(profile))
    }

    /// On any error the parent must remove peers and SSH grants and withdraw readiness.
    pub fn desired_host_state(&mut self, now: u64) -> RuntimeResult<HostDesiredState> {
        self.advance_clock(now)?;
        let snapshot = self.snapshot()?;
        let serving = self.require_server(&snapshot, now).is_ok();
        let mut peers = Vec::new();
        let mut ssh_authorizations = Vec::new();
        for (id, member) in snapshot.effective_members(now)? {
            if !serving || id == self.config.server_id {
                continue;
            }
            if let Some(user) = &member.permissions.ssh_user {
                ssh_authorizations.push(SshAuthorization {
                    device_id: id.clone(),
                    user: user.clone(),
                    public_key: member.request.ssh_public_key.clone(),
                });
            }
            peers.push(HostPeer {
                device_id: id,
                public_key: member.request.wg_public_key.clone(),
                allowed_ip: member.allowed_ip(),
                endpoint: match &member.kind {
                    MemberKind::Laptop => None,
                    MemberKind::Server(server) => Some(server.wireguard_endpoint),
                },
                permissions: member.permissions.clone(),
            });
        }
        Ok(HostDesiredState {
            private_key: self.keys.wireguard_private_key()?,
            address: self.config.network.id.address(&self.keys.wg_public_key()?),
            peers,
            ssh_authorizations,
            snapshot,
        })
    }

    /// Call only after successful enforcement of this exact snapshot. This does
    /// not assert readiness, freshness, or enforcement of subsequently seen rows.
    pub fn acknowledge_applied(&mut self, applied: &RegistrySnapshot) -> RuntimeResult<()> {
        validate_snapshot(applied, &self.config.network)?;
        let current = self.snapshot()?;
        for revocation in &applied.revocations {
            if !current.revocations.contains(revocation) {
                return Err(RuntimeError::Conflict("applied revocation"));
            }
        }
        for revocation in &applied.revocations {
            self.with_registry(RegistryOperation::Mutation, |runtime| {
                runtime.store.append_ack(&Ack {
                    revocation: revocation.id.clone(),
                    server: runtime.config.server_id.clone(),
                    boot: runtime.config.boot_id.clone(),
                })
            })?;
        }
        Ok(())
    }
}

fn sql_time(time: u64) -> RuntimeResult<i64> {
    i64::try_from(time).map_err(|_| RuntimeError::Invalid("timestamp exceeds SQLite integer range"))
}

fn validate_snapshot(
    snapshot: &RegistrySnapshot,
    network: &NetworkDescriptor,
) -> RuntimeResult<()> {
    if &snapshot.network != network {
        return Err(RuntimeError::Conflict("network descriptor"));
    }
    if snapshot.members.len() + snapshot.revocations.len() + snapshot.acks.len() + 1
        > MAX_REGISTRY_ROWS
    {
        return Err(RuntimeError::Capacity("registry snapshot"));
    }
    bounded_json_size(snapshot)?;
    snapshot.validate()?;
    Ok(())
}

fn server_pins(
    snapshot: &RegistrySnapshot,
    now: u64,
) -> RuntimeResult<BTreeMap<DeviceId, ServerInfo>> {
    Ok(snapshot
        .effective_members(now)?
        .into_iter()
        .filter_map(|(id, member)| match &member.kind {
            MemberKind::Server(server) => Some((id, server.clone())),
            MemberKind::Laptop => None,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NetworkId, Revocation, RevocationId};
    use std::{cell::RefCell, rc::Rc};

    #[derive(Clone)]
    struct MemoryStore(Rc<RefCell<MemoryState>>);
    struct MemoryState {
        snapshot: RegistrySnapshot,
        fail_before_member: bool,
        fail_after_member: bool,
        fail_ack: bool,
    }
    impl RegistryStore for MemoryStore {
        fn snapshot(&mut self) -> RuntimeResult<RegistrySnapshot> {
            Ok(self.0.borrow().snapshot.clone())
        }
        fn append_member(&mut self, member: &Member) -> RuntimeResult<()> {
            let mut state = self.0.borrow_mut();
            if std::mem::take(&mut state.fail_before_member) {
                return Err(RuntimeError::Transport);
            }
            if let Some(old) = state
                .snapshot
                .members
                .iter()
                .find(|m| m.device_id == member.device_id)
            {
                if old != member {
                    return Err(RuntimeError::Conflict("member"));
                }
            } else {
                state.snapshot.members.push(member.clone());
            }
            state.snapshot.validate()?;
            if std::mem::take(&mut state.fail_after_member) {
                return Err(RuntimeError::Transport);
            }
            Ok(())
        }
        fn append_revocation(&mut self, revocation: &Revocation) -> RuntimeResult<()> {
            let mut state = self.0.borrow_mut();
            if !state.snapshot.revocations.contains(revocation) {
                state.snapshot.revocations.push(revocation.clone());
            }
            Ok(())
        }
        fn append_ack(&mut self, ack: &Ack) -> RuntimeResult<()> {
            let mut state = self.0.borrow_mut();
            if state.fail_ack {
                return Err(RuntimeError::Transport);
            }
            if !state.snapshot.acks.contains(ack) {
                state.snapshot.acks.push(ack.clone());
            }
            Ok(())
        }
    }

    fn admin() -> Permissions {
        Permissions {
            network_admin: true,
            server_admin: true,
            ..Permissions::default()
        }
    }
    fn verified(
        keys: &LocalDeviceKeys,
        network: &NetworkId,
        kind: DeviceKind,
    ) -> crate::VerifiedEnrollment {
        let request = keys
            .enrollment_request(None, kind, "fixture".parse().unwrap())
            .unwrap();
        let (challenge, pending) = WgChallenge::begin(&request, network.clone(), 1, 100).unwrap();
        pending
            .verify(&keys.answer_challenge(&challenge, &request, 1).unwrap(), 1)
            .unwrap()
    }
    fn certificate() -> String {
        use openssl::{
            asn1::{Asn1Integer, Asn1Time},
            bn::BigNum,
            hash::MessageDigest,
            pkey::PKey,
            x509::{X509, X509NameBuilder},
        };
        let key = PKey::generate_ed25519().unwrap();
        let mut name = X509NameBuilder::new().unwrap();
        name.append_entry_by_text("CN", "runtime fixture").unwrap();
        let name = name.build();
        let mut cert = X509::builder().unwrap();
        cert.set_version(2).unwrap();
        cert.set_serial_number(&Asn1Integer::from_bn(&BigNum::from_u32(1).unwrap()).unwrap())
            .unwrap();
        cert.set_subject_name(&name).unwrap();
        cert.set_issuer_name(&name).unwrap();
        cert.set_pubkey(&key).unwrap();
        cert.set_not_before(&Asn1Time::days_from_now(0).unwrap())
            .unwrap();
        cert.set_not_after(&Asn1Time::days_from_now(1).unwrap())
            .unwrap();
        cert.sign(&key, MessageDigest::null()).unwrap();
        String::from_utf8(cert.build().to_pem().unwrap()).unwrap()
    }

    #[test]
    fn management_authorization_binds_action_and_rejects_replay_and_revocation() {
        use crate::api::ManagementAction;
        let fixture = Fixture::new();
        let mut runtime = fixture.open();
        let action = ManagementAction::Rpc {
            scope: Some("demo/production".parse().unwrap()),
            sha256: "ab".repeat(32),
        };
        let signed = fixture.signed(
            &fixture.admin,
            RpcRequest::Authorize {
                action: action.clone(),
            },
            10,
        );
        let path = signed.request.path();
        let mut wrong = signed.clone();
        wrong.server_id = wrong.signer.clone();
        assert!(runtime.handle(wrong, "POST", path, 10).is_err());
        let mut changed = signed.clone();
        changed.request = RpcRequest::Authorize {
            action: ManagementAction::Rpc {
                scope: Some("other/production".parse().unwrap()),
                sha256: "ab".repeat(32),
            },
        };
        assert!(runtime.handle(changed, "POST", path, 10).is_err());
        let response = runtime.handle(signed.clone(), "POST", path, 10).unwrap();
        assert_eq!(
            response,
            RpcResponse::Authorized {
                signer: signed.signer.clone(),
                permissions: admin(),
                action,
            }
        );
        assert!(matches!(
            runtime.handle(signed.clone(), "POST", path, 10),
            Err(RuntimeError::Replay)
        ));
        let revoke = fixture
            .store
            .0
            .borrow()
            .snapshot
            .plan_revocation(
                RevocationId::random().unwrap(),
                &signed.signer,
                &signed.signer,
                11,
            )
            .unwrap();
        fixture
            .store
            .0
            .borrow_mut()
            .snapshot
            .revocations
            .push(revoke);
        let fresh = fixture.signed(&fixture.admin, signed.request, 12);
        assert!(runtime.handle(fresh, "POST", path, 12).is_err());
    }

    struct Fixture {
        dir: tempfile::TempDir,
        store: MemoryStore,
        admin: LocalDeviceKeys,
        config: RuntimeConfig,
    }
    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let admin_keys = LocalDeviceKeys::generate().unwrap();
            let network = NetworkId::random().unwrap();
            let admin_id = admin_keys.device_id(&network, DeviceKind::Laptop).unwrap();
            let descriptor = NetworkDescriptor {
                id: network.clone(),
                initial_admin: admin_id,
            };
            let mut snapshot = RegistrySnapshot {
                network: descriptor.clone(),
                members: vec![],
                revocations: vec![],
                acks: vec![],
            };
            let root = snapshot
                .approve(
                    verified(&admin_keys, &network, DeviceKind::Laptop),
                    Approval {
                        kind: MemberKind::Laptop,
                        permissions: admin(),
                        approved_by: None,
                        independent_admin: false,
                        created_at: 1,
                        expires_at: None,
                    },
                )
                .unwrap();
            snapshot.members.push(root);
            let server_keys = LocalDeviceKeys::generate().unwrap();
            server_keys.save_new(&dir.path().join("keys")).unwrap();
            let server_id = server_keys.device_id(&network, DeviceKind::Server).unwrap();
            let server = snapshot
                .approve_independent_admin(
                    verified(&server_keys, &network, DeviceKind::Server),
                    Approval {
                        kind: MemberKind::Server(ServerInfo {
                            wireguard_endpoint: "203.0.113.9:51820".parse().unwrap(),
                            ssh_host_key: server_keys.ssh_public_key().unwrap(),
                            ssh_port: 22,
                            https_port: 443,
                            https_certificate_pem: Some(certificate()),
                            corrosion_ca_certificate_pem: None,
                            https_spki_sha256: None,
                        }),
                        permissions: admin(),
                        approved_by: None,
                        independent_admin: true,
                        created_at: 1,
                        expires_at: None,
                    },
                )
                .unwrap();
            snapshot.members.push(server);
            Self {
                dir,
                store: MemoryStore(Rc::new(RefCell::new(MemoryState {
                    snapshot,
                    fail_before_member: false,
                    fail_after_member: false,
                    fail_ack: false,
                }))),
                admin: admin_keys,
                config: RuntimeConfig {
                    network: descriptor,
                    server_id,
                    boot_id: BootId::random().unwrap(),
                    limits: RuntimeLimits::default(),
                },
            }
        }
        fn open(&self) -> ServerRuntime<MemoryStore> {
            ServerRuntime::open(
                self.store.clone(),
                LocalDeviceKeys::load(&self.dir.path().join("keys")).unwrap(),
                self.config.clone(),
                &self.dir.path().join("runtime.db"),
            )
            .unwrap()
        }
        fn signed(&self, keys: &LocalDeviceKeys, request: RpcRequest, now: u64) -> SignedRequest {
            SignedRequest::sign(
                keys,
                self.config.network.id.clone(),
                self.config.server_id.clone(),
                DeviceKind::Laptop,
                now,
                request,
            )
            .unwrap()
        }
        fn call(
            &self,
            runtime: &mut ServerRuntime<MemoryStore>,
            keys: &LocalDeviceKeys,
            request: RpcRequest,
            now: u64,
        ) -> RuntimeResult<RpcResponse> {
            let path = request.path();
            runtime.handle(self.signed(keys, request, now), "POST", path, now)
        }
        fn grant(&self, keys: &LocalDeviceKeys) -> ApproveRequest {
            ApproveRequest {
                pending_id: RequestId::random().unwrap(),
                request: keys
                    .enrollment_request(None, DeviceKind::Laptop, "new-laptop".parse().unwrap())
                    .unwrap(),
                kind: MemberKind::Laptop,
                permissions: Permissions::default(),
                expires_at: 1000,
                member_expires_at: None,
            }
        }
        fn approve(&self, runtime: &mut ServerRuntime<MemoryStore>, grant: &ApproveRequest) {
            let RpcResponse::Pending(profile) = self
                .call(
                    runtime,
                    &self.admin,
                    RpcRequest::Approve(Box::new(grant.clone())),
                    10,
                )
                .unwrap()
            else {
                panic!("expected pending");
            };
            assert_eq!(profile.pending_id, grant.pending_id);
            assert!(
                !profile
                    .snapshot
                    .members
                    .iter()
                    .any(|m| m.request == grant.request)
            );
        }
        fn challenge(
            &self,
            runtime: &mut ServerRuntime<MemoryStore>,
            keys: &LocalDeviceKeys,
            grant: &ApproveRequest,
            now: u64,
        ) -> RpcRequest {
            let RpcResponse::Challenge {
                challenge_id,
                challenge,
            } = self
                .call(
                    runtime,
                    keys,
                    RpcRequest::Begin {
                        pending_id: grant.pending_id.clone(),
                    },
                    now,
                )
                .unwrap()
            else {
                panic!("expected challenge");
            };
            RpcRequest::Complete {
                pending_id: grant.pending_id.clone(),
                challenge_id,
                proof: keys
                    .answer_challenge(&challenge, &grant.request, now)
                    .unwrap(),
            }
        }
    }

    #[test]
    fn pending_has_no_authority_then_wg_proof_activates_and_nonce_survives_restart() {
        let fixture = Fixture::new();
        let mut runtime = fixture.open();
        let keys = LocalDeviceKeys::generate().unwrap();
        let grant = fixture.grant(&keys);
        fixture.approve(&mut runtime, &grant);
        assert!(
            fixture
                .call(&mut runtime, &keys, RpcRequest::Snapshot, 10)
                .is_err()
        );
        let command = fixture.challenge(&mut runtime, &keys, &grant, 11);
        let signed = fixture.signed(&keys, command.clone(), 12);
        let RpcResponse::Active(profile) = runtime
            .handle(signed.clone(), "POST", command.path(), 12)
            .unwrap()
        else {
            panic!("expected active");
        };
        profile.validate_for(&keys, 12).unwrap();
        assert!(matches!(
            runtime.handle(signed.clone(), "POST", command.path(), 12),
            Err(RuntimeError::Replay)
        ));
        drop(runtime);
        let mut runtime = fixture.open();
        assert!(matches!(
            runtime.handle(signed, "POST", command.path(), 12),
            Err(RuntimeError::Replay)
        ));
        assert!(matches!(
            fixture
                .call(
                    &mut runtime,
                    &keys,
                    RpcRequest::Begin {
                        pending_id: grant.pending_id
                    },
                    13
                )
                .unwrap(),
            RpcResponse::Active(_)
        ));
        assert!(matches!(
            fixture
                .call(&mut runtime, &keys, RpcRequest::Snapshot, 13)
                .unwrap(),
            RpcResponse::Snapshot(_)
        ));
        assert!(
            fixture
                .call(
                    &mut runtime,
                    &keys,
                    RpcRequest::Approve(Box::new(fixture.grant(&keys))),
                    13
                )
                .is_err()
        );
    }

    #[test]
    fn activation_journal_recovers_before_and_after_remote_commit() {
        for after_commit in [false, true] {
            let fixture = Fixture::new();
            let mut runtime = fixture.open();
            let keys = LocalDeviceKeys::generate().unwrap();
            let grant = fixture.grant(&keys);
            fixture.approve(&mut runtime, &grant);
            let command = fixture.challenge(&mut runtime, &keys, &grant, 11);
            if after_commit {
                fixture.store.0.borrow_mut().fail_after_member = true;
            } else {
                fixture.store.0.borrow_mut().fail_before_member = true;
            }
            assert!(matches!(
                fixture.call(&mut runtime, &keys, command, 12),
                Err(RuntimeError::Transport)
            ));
            let journal = runtime
                .pending(&grant.pending_id, 12)
                .unwrap()
                .candidate
                .unwrap();
            drop(runtime);
            let mut runtime = fixture.open();
            let response = if after_commit {
                fixture
                    .call(
                        &mut runtime,
                        &keys,
                        RpcRequest::Begin {
                            pending_id: grant.pending_id.clone(),
                        },
                        13,
                    )
                    .unwrap()
            } else {
                let command = fixture.challenge(&mut runtime, &keys, &grant, 13);
                fixture.call(&mut runtime, &keys, command, 14).unwrap()
            };
            let RpcResponse::Active(profile) = response else {
                panic!("expected active");
            };
            assert_eq!(
                profile
                    .snapshot
                    .members
                    .iter()
                    .find(|m| m.device_id == journal.device_id)
                    .unwrap(),
                &journal
            );
            assert_eq!(journal.created_at, 12);
        }
    }

    #[test]
    fn revocation_between_begin_and_complete_denies_activation_and_acks_require_helper_success() {
        let fixture = Fixture::new();
        let mut runtime = fixture.open();
        let keys = LocalDeviceKeys::generate().unwrap();
        let grant = fixture.grant(&keys);
        fixture.approve(&mut runtime, &grant);
        let command = fixture.challenge(&mut runtime, &keys, &grant, 11);
        let target = fixture.config.network.initial_admin.clone();
        let id = RevocationId::random().unwrap();
        let RpcResponse::Revoked(revocation) = fixture
            .call(
                &mut runtime,
                &fixture.admin,
                RpcRequest::Revoke { id, target },
                12,
            )
            .unwrap()
        else {
            panic!("expected revocation");
        };
        assert!(fixture.call(&mut runtime, &keys, command, 13).is_err());
        let desired = runtime.desired_host_state(13).unwrap();
        assert!(desired.peers.is_empty());
        assert!(desired.ssh_authorizations.is_empty());
        assert!(desired.snapshot.acks.is_empty());
        fixture.store.0.borrow_mut().fail_ack = true;
        assert!(runtime.acknowledge_applied(&desired.snapshot).is_err());
        assert!(runtime.snapshot().unwrap().acks.is_empty());
        fixture.store.0.borrow_mut().fail_ack = false;
        runtime.acknowledge_applied(&desired.snapshot).unwrap();
        let snapshot = runtime.snapshot().unwrap();
        assert_eq!(
            snapshot.acks,
            vec![Ack {
                revocation: revocation.id,
                server: fixture.config.server_id.clone(),
                boot: fixture.config.boot_id.clone()
            }]
        );
    }

    #[test]
    fn current_delegation_and_explicit_ssh_permissions_bound_pending_approval() {
        let fixture = Fixture::new();
        fixture.store.0.borrow_mut().snapshot.members[0]
            .permissions
            .server_admin = false;
        let mut runtime = fixture.open();
        let keys = LocalDeviceKeys::generate().unwrap();
        let mut grant = fixture.grant(&keys);
        grant
            .permissions
            .deployments
            .insert("app/prod".parse().unwrap());
        assert!(
            fixture
                .call(
                    &mut runtime,
                    &fixture.admin,
                    RpcRequest::Approve(Box::new(grant.clone())),
                    10
                )
                .is_err()
        );
        grant.permissions.deployments.clear();
        grant.permissions.ssh_user = Some("root".parse().unwrap());
        assert!(
            fixture
                .call(
                    &mut runtime,
                    &fixture.admin,
                    RpcRequest::Approve(Box::new(grant.clone())),
                    10
                )
                .is_err()
        );
        grant.permissions = Permissions::default();
        fixture.approve(&mut runtime, &grant);
        let mut changed = grant.clone();
        changed.permissions.network_admin = true;
        assert!(matches!(
            fixture.call(
                &mut runtime,
                &fixture.admin,
                RpcRequest::Approve(Box::new(changed)),
                10
            ),
            Err(RuntimeError::Conflict(_))
        ));
        let command = fixture.challenge(&mut runtime, &keys, &grant, 11);
        fixture.store.0.borrow_mut().snapshot.members[0]
            .permissions
            .network_admin = false;
        assert!(fixture.call(&mut runtime, &keys, command, 12).is_err());
        assert!(matches!(
            runtime.snapshot(),
            Err(RuntimeError::Conflict("observed registry record"))
        ));
    }

    #[test]
    fn auth_window_audience_clock_and_capacity_fail_closed() {
        let mut fixture = Fixture::new();
        fixture.config.limits.max_replays = 32;
        let mut runtime = fixture.open();
        let signed = fixture.signed(&fixture.admin, RpcRequest::Snapshot, 10);
        for (method, path, now) in [
            ("GET", "/v1/network/snapshot", 10),
            ("POST", "/v1/network/approve", 10),
            ("POST", "/v1/network/snapshot", 131),
        ] {
            assert!(matches!(
                runtime.handle(signed.clone(), method, path, now),
                Err(RuntimeError::Authentication)
            ));
        }
        runtime
            .handle(signed, "POST", "/v1/network/snapshot", 10)
            .unwrap();
        fixture
            .call(&mut runtime, &fixture.admin, RpcRequest::Snapshot, 11)
            .unwrap();
        assert!(matches!(
            fixture.call(&mut runtime, &fixture.admin, RpcRequest::Snapshot, 12),
            Err(RuntimeError::Capacity("replays"))
        ));
        assert!(
            fixture
                .call(&mut runtime, &fixture.admin, RpcRequest::Snapshot, 9)
                .is_err()
        );
        fixture
            .call(&mut runtime, &fixture.admin, RpcRequest::Snapshot, 132)
            .unwrap();
    }

    #[test]
    fn challenges_are_bounded_expire_and_refresh_without_new_approval() {
        let mut fixture = Fixture::new();
        fixture.config.limits.max_challenges = 1;
        fixture.config.limits.max_pending = 2;
        fixture.config.limits.challenge_seconds = 2;
        let mut runtime = fixture.open();
        let keys = LocalDeviceKeys::generate().unwrap();
        let grant = fixture.grant(&keys);
        let other_keys = LocalDeviceKeys::generate().unwrap();
        let other = fixture.grant(&other_keys);
        fixture.approve(&mut runtime, &grant);
        fixture.approve(&mut runtime, &other);
        assert!(matches!(
            fixture.call(
                &mut runtime,
                &fixture.admin,
                RpcRequest::Approve(Box::new(fixture.grant(&keys))),
                10
            ),
            Err(RuntimeError::Capacity("pending entitlements"))
        ));
        let expired = fixture.challenge(&mut runtime, &keys, &grant, 11);
        assert!(matches!(
            fixture.call(
                &mut runtime,
                &other_keys,
                RpcRequest::Begin {
                    pending_id: other.pending_id.clone()
                },
                11
            ),
            Err(RuntimeError::Capacity("WG challenges"))
        ));
        assert!(fixture.call(&mut runtime, &keys, expired, 13).is_err());
        let command = fixture.challenge(&mut runtime, &keys, &grant, 14);
        drop(runtime);
        let mut runtime = fixture.open();
        assert!(fixture.call(&mut runtime, &keys, command, 15).is_err());
        let command = fixture.challenge(&mut runtime, &keys, &grant, 16);
        assert!(fixture.call(&mut runtime, &keys, command, 17).is_ok());
    }

    #[test]
    fn database_identity_and_exclusive_runtime_lock_are_persistent() {
        let fixture = Fixture::new();
        let runtime = fixture.open();
        let open = |config| {
            ServerRuntime::open(
                fixture.store.clone(),
                LocalDeviceKeys::load(&fixture.dir.path().join("keys")).unwrap(),
                config,
                &fixture.dir.path().join("runtime.db"),
            )
        };
        assert!(open(fixture.config.clone()).is_err());
        drop(runtime);
        let mut wrong = fixture.config.clone();
        wrong.network.initial_admin = fixture.config.server_id.clone();
        fixture.store.0.borrow_mut().snapshot.network = wrong.network.clone();
        assert!(open(wrong).is_err());
    }

    #[test]
    fn invalid_proofs_and_cross_applicant_challenges_never_activate_members() {
        let fixture = Fixture::new();
        let mut runtime = fixture.open();
        let keys = LocalDeviceKeys::generate().unwrap();
        let grant = fixture.grant(&keys);
        let stranger = LocalDeviceKeys::generate().unwrap();
        let other = fixture.grant(&stranger);
        fixture.approve(&mut runtime, &grant);
        fixture.approve(&mut runtime, &other);
        assert!(
            fixture
                .call(
                    &mut runtime,
                    &stranger,
                    RpcRequest::Begin {
                        pending_id: grant.pending_id.clone()
                    },
                    11
                )
                .is_err()
        );
        let valid = fixture.challenge(&mut runtime, &keys, &grant, 11);
        let RpcRequest::Complete {
            challenge_id,
            proof,
            ..
        } = valid.clone()
        else {
            unreachable!()
        };
        let wrong_owner = RpcRequest::Complete {
            pending_id: other.pending_id.clone(),
            challenge_id,
            proof,
        };
        assert!(matches!(
            fixture.call(&mut runtime, &stranger, wrong_owner, 11),
            Err(RuntimeError::ChallengeUnavailable)
        ));
        let mut invalid = valid.clone();
        if let RpcRequest::Complete { proof, .. } = &mut invalid {
            proof.response = crate::KeyFingerprint::of_bytes(b"wrong WG secret");
        }
        assert!(fixture.call(&mut runtime, &keys, invalid, 12).is_err());
        assert!(matches!(
            fixture.call(&mut runtime, &keys, valid, 12),
            Err(RuntimeError::ChallengeUnavailable)
        ));
        assert_eq!(runtime.snapshot().unwrap().members.len(), 2);
        let fresh = fixture.challenge(&mut runtime, &keys, &grant, 13);
        assert!(matches!(
            fixture.call(&mut runtime, &keys, fresh, 14).unwrap(),
            RpcResponse::Active(_)
        ));
    }

    #[test]
    fn future_nonce_is_retained_through_its_last_valid_second() {
        let fixture = Fixture::new();
        let mut runtime = fixture.open();
        let signed = fixture.signed(&fixture.admin, RpcRequest::Snapshot, 130);
        let mut invalid = signed.clone();
        invalid.signature.push('\n');
        assert!(matches!(
            runtime.handle(invalid, "POST", signed.request.path(), 10),
            Err(RuntimeError::Authentication)
        ));
        runtime
            .handle(signed.clone(), "POST", signed.request.path(), 10)
            .unwrap();
        assert!(matches!(
            runtime.handle(signed.clone(), "POST", signed.request.path(), 250),
            Err(RuntimeError::Replay)
        ));
        assert!(matches!(
            runtime.handle(signed.clone(), "POST", signed.request.path(), 251),
            Err(RuntimeError::Authentication)
        ));
        fixture
            .call(&mut runtime, &fixture.admin, RpcRequest::Snapshot, 251)
            .unwrap();
        let count: i64 = runtime
            .db
            .query_row("SELECT count(*) FROM consumed_nonces", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn revoked_server_projects_empty_host_and_new_boot_needs_new_ack() {
        let mut fixture = Fixture::new();
        let mut runtime = fixture.open();
        let id = RevocationId::random().unwrap();
        fixture
            .call(
                &mut runtime,
                &fixture.admin,
                RpcRequest::Revoke {
                    id: id.clone(),
                    target: fixture.config.server_id.clone(),
                },
                10,
            )
            .unwrap();
        assert!(
            fixture
                .call(&mut runtime, &fixture.admin, RpcRequest::Snapshot, 11)
                .is_err()
        );
        let desired = runtime.desired_host_state(11).unwrap();
        assert!(desired.peers.is_empty());
        assert!(desired.ssh_authorizations.is_empty());
        runtime.acknowledge_applied(&desired.snapshot).unwrap();
        drop(runtime);
        fixture.config.boot_id = BootId::random().unwrap();
        let mut runtime = fixture.open();
        let boots = BTreeMap::from([(
            fixture.config.server_id.clone(),
            fixture.config.boot_id.clone(),
        )]);
        assert!(
            runtime
                .snapshot()
                .unwrap()
                .revocation_progress(&id, &boots)
                .unwrap()
                .pending_servers
                .contains(&fixture.config.server_id)
        );
        runtime.acknowledge_applied(&desired.snapshot).unwrap();
        assert!(
            runtime
                .snapshot()
                .unwrap()
                .revocation_progress(&id, &boots)
                .unwrap()
                .pending_servers
                .is_empty()
        );
    }

    #[test]
    fn revoked_approver_cannot_resume_an_uncommitted_activation_journal() {
        let fixture = Fixture::new();
        let mut runtime = fixture.open();
        let keys = LocalDeviceKeys::generate().unwrap();
        let grant = fixture.grant(&keys);
        fixture.approve(&mut runtime, &grant);
        let command = fixture.challenge(&mut runtime, &keys, &grant, 11);
        fixture.store.0.borrow_mut().fail_before_member = true;
        assert!(fixture.call(&mut runtime, &keys, command, 12).is_err());
        fixture
            .call(
                &mut runtime,
                &fixture.admin,
                RpcRequest::Revoke {
                    id: RevocationId::random().unwrap(),
                    target: fixture.config.network.initial_admin.clone(),
                },
                13,
            )
            .unwrap();
        drop(runtime);
        let mut runtime = fixture.open();
        assert!(
            fixture
                .call(
                    &mut runtime,
                    &keys,
                    RpcRequest::Begin {
                        pending_id: grant.pending_id
                    },
                    14
                )
                .is_err()
        );
        assert_eq!(runtime.snapshot().unwrap().members.len(), 2);
    }

    #[test]
    fn authentication_window_cannot_expand_after_nonce_garbage_collection() {
        let mut fixture = Fixture::new();
        fixture.config.limits.auth_window_seconds = 1;
        let mut runtime = fixture.open();
        fixture
            .call(&mut runtime, &fixture.admin, RpcRequest::Snapshot, 10)
            .unwrap();
        fixture
            .call(&mut runtime, &fixture.admin, RpcRequest::Snapshot, 12)
            .unwrap();
        drop(runtime);
        fixture.config.limits.auth_window_seconds = 120;
        let result = ServerRuntime::open(
            fixture.store.clone(),
            LocalDeviceKeys::load(&fixture.dir.path().join("keys")).unwrap(),
            fixture.config.clone(),
            &fixture.dir.path().join("runtime.db"),
        );
        assert!(matches!(
            result,
            Err(RuntimeError::Conflict("local runtime identity"))
        ));
    }
}
