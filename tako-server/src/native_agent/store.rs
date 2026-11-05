//! Every runtime read passes through this monotonic guard, including reads
//! inside handle/ACK. A preflight-only guard would miss rollback between reads.
use std::{
    cell::RefCell,
    collections::BTreeSet,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tako_network::{
    Ack, Member, RegistrySnapshot, Revocation,
    api::{RuntimeError, RuntimeResult},
    corrosion::{MAX_REGISTRY_BYTES, MAX_REGISTRY_ROWS, RegistryStore},
    node_config::protected::Directory,
};

pub(super) struct MonotonicStore<S> {
    inner: Rc<RefCell<S>>,
    stop: Arc<AtomicBool>,
    directory: Directory,
    pub(super) known: Rc<RefCell<RegistrySnapshot>>,
}
impl<S: RegistryStore> MonotonicStore<S> {
    pub(super) fn new(
        inner: S,
        directory: Directory,
        known: Rc<RefCell<RegistrySnapshot>>,
    ) -> Self {
        Self {
            inner: Rc::new(RefCell::new(inner)),
            stop: Arc::new(AtomicBool::new(false)),
            directory,
            known,
        }
    }
    pub(super) fn shared(&self) -> Rc<RefCell<S>> {
        self.inner.clone()
    }
    pub(super) fn with_stop(mut self, stop: Arc<AtomicBool>) -> Self {
        self.stop = stop;
        self
    }
}

fn records(snapshot: &RegistrySnapshot) -> RuntimeResult<BTreeSet<[u8; 32]>> {
    let mut result = BTreeSet::new();
    for member in &snapshot.members {
        result.insert(openssl::sha::sha256(&serde_json::to_vec(&(
            "member", member,
        ))?));
    }
    for revocation in &snapshot.revocations {
        result.insert(openssl::sha::sha256(&serde_json::to_vec(&(
            "revocation",
            revocation,
        ))?));
    }
    // ACKs are replaceable progress evidence. Corrosion may compact or omit
    // them; only immutable membership and denial facts are monotonic.
    Ok(result)
}
pub(super) fn preserves(
    known: &RegistrySnapshot,
    candidate: &RegistrySnapshot,
) -> RuntimeResult<()> {
    candidate.validate()?;
    if candidate.network != known.network {
        return Err(RuntimeError::Conflict("network descriptor"));
    }
    if candidate.members.len() + candidate.revocations.len() + candidate.acks.len() + 1
        > MAX_REGISTRY_ROWS
        || serde_json::to_vec(candidate)?.len() > MAX_REGISTRY_BYTES
    {
        return Err(RuntimeError::Capacity("monotonic registry"));
    }
    if !records(known)?.is_subset(&records(candidate)?) {
        return Err(RuntimeError::Corrosion(
            "known records missing; operator restore required",
        ));
    }
    Ok(())
}
impl<S: RegistryStore> RegistryStore for MonotonicStore<S> {
    fn snapshot(&mut self) -> RuntimeResult<RegistrySnapshot> {
        if self.stop.load(Ordering::Acquire) {
            return Err(RuntimeError::Transport);
        }
        let candidate = self.inner.borrow_mut().snapshot()?;
        let mut known = self.known.borrow_mut();
        preserves(&known, &candidate)?;
        if *known != candidate {
            self.directory
                .write("known-registry.json", &serde_json::to_vec(&candidate)?)?;
            *known = candidate.clone();
        }
        Ok(candidate)
    }
    fn append_member(&mut self, member: &Member) -> RuntimeResult<()> {
        self.snapshot()?;
        self.inner.borrow_mut().append_member(member)?;
        self.snapshot()?;
        Ok(())
    }
    fn append_revocation(&mut self, revocation: &Revocation) -> RuntimeResult<()> {
        self.snapshot()?;
        self.inner.borrow_mut().append_revocation(revocation)?;
        self.snapshot()?;
        Ok(())
    }
    fn append_ack(&mut self, ack: &Ack) -> RuntimeResult<()> {
        if self.stop.load(Ordering::Acquire) {
            return Err(RuntimeError::Transport);
        }
        // A fresh snapshot is checked by acknowledge_applied before this loop.
        // This is only the latest incoming ACK projection, never historical
        // progress merged from disk. Re-posting an exact current ACK is needless.
        if self.known.borrow().acks.contains(ack) {
            return Ok(());
        }
        self.snapshot()?;
        self.inner.borrow_mut().append_ack(ack)?;
        self.snapshot()?;
        Ok(())
    }
}
