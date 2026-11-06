//! One admission gate shared by the blocking controller and async supervisor.
use super::{AgentError, Result};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tako_network::RegistrySnapshot;
use tokio::sync::watch;

pub(super) const DRAIN_DEADLINE: Duration = Duration::from_secs(120);

#[derive(Clone)]
pub(super) struct Control {
    pub stop: Arc<AtomicBool>,
    pub isolate: Arc<AtomicBool>,
    pub broker_drained: Arc<AtomicBool>,
    pub services_drained: Arc<AtomicBool>,
    services: Option<Arc<super::services::Shared>>,
    membership: watch::Sender<Option<RegistrySnapshot>>,
    shutdown: watch::Sender<bool>,
}
impl Control {
    pub fn new() -> (
        Self,
        watch::Receiver<Option<RegistrySnapshot>>,
        watch::Receiver<bool>,
    ) {
        let (membership, members) = watch::channel(None);
        let (shutdown, shutdown_rx) = watch::channel(false);
        (
            Self {
                stop: Arc::new(AtomicBool::new(false)),
                isolate: Arc::new(AtomicBool::new(false)),
                broker_drained: Arc::new(AtomicBool::new(false)),
                services_drained: Arc::new(AtomicBool::new(true)),
                services: None,
                membership,
                shutdown,
            },
            members,
            shutdown_rx,
        )
    }
    pub fn attach_services(&mut self, services: Arc<super::services::Shared>) {
        self.services = Some(services);
        self.services_drained.store(false, Ordering::Release);
    }
    pub fn clear_services(&self) {
        if let Some(services) = &self.services {
            if let Err(error) = services.clear() {
                tracing::error!(error = %error, "service view cleared; proxy file withdrawal failed (old file expires within 15 seconds)");
            }
        }
    }
    pub fn publish_services(
        &self,
        snapshot: &crate::native_services::ServiceSnapshot,
        now: u64,
    ) -> Result<()> {
        let membership = self.membership.borrow();
        if self.stop.load(Ordering::Acquire)
            || membership.as_ref() != Some(&authority(&snapshot.registry))
        {
            return Err(AgentError::Invalid(
                "service membership is no longer enforced",
            ));
        }
        // Keep the read gate until publication completes: withdrawal waits, then
        // clears the directory/file. A late result cannot resurrect availability.
        self.services
            .as_ref()
            .ok_or(AgentError::Invalid("service directory absent"))?
            .publish(snapshot, now)
    }
    pub fn withdraw(&self) {
        self.membership
            .send_if_modified(|current| current.take().is_some());
        self.clear_services();
    }
    pub fn shutdown(&self, failed: bool) {
        if failed {
            self.isolate.store(true, Ordering::Release);
        }
        // Publication checks this flag under watch's write lock. Either a racing
        // publication precedes our None, or it sees stop and cannot republish.
        self.stop.store(true, Ordering::Release);
        self.withdraw();
        self.shutdown.send_replace(true);
    }
    pub fn broker_finished(&self, failed: bool) {
        if failed {
            self.shutdown(true);
        }
        self.broker_drained.store(true, Ordering::Release);
    }
    pub fn matches(&self, snapshot: &RegistrySnapshot) -> bool {
        let candidate = authority(snapshot);
        self.membership.borrow().as_ref() == Some(&candidate)
    }
    /// Call only after helper enforcement, ACK and the final current-state check.
    /// Return false if shutdown raced with the completed reconciliation.
    pub fn publish_enforced(&self, snapshot: &RegistrySnapshot) -> bool {
        let candidate = authority(snapshot);
        let mut admitted = false;
        self.membership.send_if_modified(|current| {
            if self.stop.load(Ordering::Acquire) {
                return false;
            }
            admitted = true;
            if current.as_ref() == Some(&candidate) {
                return false;
            }
            *current = Some(candidate);
            true
        });
        admitted
    }
}
fn authority(snapshot: &RegistrySnapshot) -> RegistrySnapshot {
    let mut snapshot = snapshot.clone();
    // Progress evidence never changes the enforced channel roster. In particular,
    // new boots, compaction and ACK omission must not reset broker generations.
    snapshot.acks.clear();
    snapshot
        .members
        .sort_by(|a, b| a.device_id.cmp(&b.device_id));
    snapshot.revocations.sort_by(|a, b| a.id.cmp(&b.id));
    snapshot
}

/// Publish broker completion even on panic/cancellation. A normal completion
/// is unexpected unless the supervisor had already requested shutdown.
pub(super) struct BrokerOwner {
    control: Control,
    finished: bool,
    services: bool,
}
impl BrokerOwner {
    pub fn new(control: Control) -> Self {
        Self {
            control,
            finished: false,
            services: false,
        }
    }
    pub fn services(control: Control) -> Self {
        Self {
            control,
            finished: false,
            services: true,
        }
    }
    fn finished(&self, failed: bool) {
        if self.services {
            if failed {
                self.control.shutdown(true);
            }
            self.control.services_drained.store(true, Ordering::Release);
        } else {
            self.control.broker_finished(failed);
        }
    }
    pub fn complete(&mut self, failed: bool) {
        let unexpected = !self.control.stop.load(Ordering::Acquire);
        self.finished(failed || unexpected);
        self.finished = true;
    }
}
impl Drop for BrokerOwner {
    fn drop(&mut self) {
        if !self.finished {
            self.finished(true);
        }
    }
}

pub(super) struct WithdrawOnDrop(pub Control);
impl Drop for WithdrawOnDrop {
    fn drop(&mut self) {
        self.0.withdraw();
    }
}

/// Borrowed join handles remain owned by the caller if the deadline elapses.
/// Production fail-stops the dedicated process in that case, never returns and
/// detaches threads/tasks holding the runtime or channel database writer locks.
pub(super) async fn drain<F: std::future::Future>(
    duration: Duration,
    joins: F,
) -> Result<F::Output> {
    tokio::time::timeout(duration, joins)
        .await
        .map_err(|_| AgentError::Timeout)
}
pub(super) fn drain_timeout() -> ! {
    tracing::error!(
        "native agent shutdown deadline exceeded; forcing process exit with database owners still attached"
    );
    // Corrosion/helper children have PR_SET_PDEATHSIG(SIGKILL), so forced agent
    // exit cannot orphan them. The separately activated root helper is not ours.
    eprintln!(
        "native agent shutdown deadline exceeded; forced process termination (not a clean shutdown)"
    );
    std::process::exit(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tako_network::{Ack, BootId, RevocationId};
    #[test]
    fn only_enforcement_publishes_and_ack_churn_does_not_notify() {
        let fixture = super::super::tests::Fixture::new();
        let (control, mut members, _) = Control::new();
        assert!(members.borrow().is_none());
        assert!(control.publish_enforced(&fixture.config.bootstrap));
        drop(members.borrow_and_update());
        let mut progress = fixture.config.bootstrap.clone();
        progress.acks.push(Ack {
            revocation: RevocationId::random().unwrap(),
            server: fixture.config.server_id,
            boot: BootId::random().unwrap(),
        });
        progress.members.reverse();
        assert!(control.matches(&progress));
        assert!(control.publish_enforced(&progress));
        assert!(!members.has_changed().unwrap());
        control.withdraw();
        assert!(members.borrow_and_update().is_none());
        control.withdraw();
        assert!(!members.has_changed().unwrap());
    }
    #[test]
    fn failure_shutdown_withdraws_and_prevents_late_publication() {
        let fixture = super::super::tests::Fixture::new();
        let (control, members, shutdown) = Control::new();
        control.publish_enforced(&fixture.config.bootstrap);
        control.shutdown(true);
        assert!(members.borrow().is_none());
        assert!(*shutdown.borrow());
        assert!(control.isolate.load(Ordering::Acquire));
        assert!(!control.publish_enforced(&fixture.config.bootstrap));
        assert!(members.borrow().is_none());
    }
    #[test]
    fn dropped_controller_withdraws_even_during_unwind() {
        let fixture = super::super::tests::Fixture::new();
        let (control, members, _) = Control::new();
        let lease = WithdrawOnDrop(control.clone());
        control.publish_enforced(&fixture.config.bootstrap);
        drop(lease);
        assert!(members.borrow().is_none());
    }
    #[test]
    fn publication_racing_shutdown_cannot_restore_membership() {
        let fixture = super::super::tests::Fixture::new();
        for _ in 0..16 {
            let (control, members, _) = Control::new();
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let publisher = control.clone();
            let snapshot = fixture.config.bootstrap.clone();
            let started = barrier.clone();
            let thread = std::thread::spawn(move || {
                started.wait();
                publisher.publish_enforced(&snapshot);
            });
            barrier.wait();
            control.shutdown(false);
            thread.join().unwrap();
            assert!(members.borrow().is_none());
        }
    }
    #[tokio::test]
    async fn late_broker_failure_requests_isolation_before_releasing_controller() {
        let fixture = super::super::tests::Fixture::new();
        let (control, members, mut shutdown) = Control::new();
        control.publish_enforced(&fixture.config.bootstrap);
        let owner_control = control.clone();
        let (release, wait) = tokio::sync::oneshot::channel();
        let broker = tokio::spawn(async move {
            let mut owner = BrokerOwner::new(owner_control);
            shutdown.changed().await.unwrap();
            wait.await.unwrap(); // outstanding database fence, still owned
            owner.complete(true);
        });
        control.shutdown(false);
        assert!(members.borrow().is_none());
        assert!(!control.broker_drained.load(Ordering::Acquire));
        assert!(!control.isolate.load(Ordering::Acquire));
        release.send(()).unwrap();
        broker.await.unwrap();
        assert!(control.isolate.load(Ordering::Acquire));
        assert!(control.broker_drained.load(Ordering::Acquire));
    }
    #[test]
    fn unexpected_broker_exit_and_panic_are_failures() {
        for panic in [false, true] {
            let (control, members, _) = Control::new();
            let mut owner = BrokerOwner::new(control.clone());
            if !panic {
                owner.complete(false);
            } // unexpected successful exit
            drop(owner); // models task unwind for the other case
            assert!(control.stop.load(Ordering::Acquire));
            assert!(control.isolate.load(Ordering::Acquire));
            assert!(control.broker_drained.load(Ordering::Acquire));
            assert!(members.borrow().is_none());
        }
    }
    #[tokio::test]
    async fn deadline_does_not_detach_or_release_database_owner() {
        let (release, wait) = tokio::sync::oneshot::channel();
        let held = Arc::new(AtomicBool::new(true));
        let owned = held.clone();
        let mut task = tokio::spawn(async move {
            let _ = wait.await;
            owned.store(false, Ordering::Release);
            Ok::<(), std::io::Error>(())
        });
        assert!(drain(Duration::from_millis(10), &mut task).await.is_err());
        assert!(held.load(Ordering::Acquire));
        assert!(!task.is_finished());
        release.send(()).unwrap();
        drain(Duration::from_secs(1), &mut task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!held.load(Ordering::Acquire));
    }
}
