//! Admission and transport shutdown with accounting tied to request lifetime.
use std::{
    collections::BTreeMap,
    os::fd::{AsRawFd, OwnedFd},
    sync::{Arc, Mutex, MutexGuard},
};

// Bounds registry memory and duplicate descriptors, not throughput: long-lived
// SSE and WebSocket streams hold a slot for their whole lifetime, so this must
// stay well above realistic concurrent connections. Each slot costs one duplicate
// descriptor; the service unit grants LimitNOFILE=1048576.
const MAX_ACTIVE_REQUESTS: usize = 65_536;

enum Admission {
    Open,
    Draining,
}

enum Transport {
    Socket(OwnedFd),
    #[cfg(test)]
    Fake(Box<dyn Fn() + Send + Sync>),
}

impl Transport {
    fn shutdown(&self) {
        match self {
            Self::Socket(socket) => {
                // SAFETY: this entry owns the duplicate until its request guard drops.
                // shutdown changes the socket state, but never closes/recycles its descriptor.
                // An already closed peer may return an error; accounting still waits for Drop.
                unsafe { libc::shutdown(socket.as_raw_fd(), libc::SHUT_RDWR) };
            }
            #[cfg(test)]
            Self::Fake(close) => close(),
        }
    }
}

struct Entry {
    transport: Option<Transport>,
    long_lived: bool,
    shutdown_requested: bool,
}

impl Entry {
    fn shutdown(&mut self) -> bool {
        if self.shutdown_requested {
            return false;
        }
        let Some(transport) = &self.transport else {
            return false;
        };
        self.shutdown_requested = true;
        transport.shutdown();
        true
    }
}

struct State {
    admission: Admission,
    next_id: u64,
    active: BTreeMap<u64, Entry>,
}

pub(crate) struct RequestDrain {
    state: Mutex<State>,
}

impl RequestDrain {
    /// Construct the shared registry. Guards retain it through request completion.
    pub(crate) fn default() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                admission: Admission::Open,
                next_id: 0,
                active: BTreeMap::new(),
            }),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Reject after draining starts or when the bounded registry is full.
    /// A socketless request is still counted and must keep its guard until it exits.
    pub(crate) fn register(self: &Arc<Self>, socket: Option<OwnedFd>) -> Option<RequestGuard> {
        self.register_transport(socket.map(Transport::Socket))
    }

    fn register_transport(self: &Arc<Self>, transport: Option<Transport>) -> Option<RequestGuard> {
        let mut state = self.state();
        if matches!(state.admission, Admission::Draining)
            || state.active.len() >= MAX_ACTIVE_REQUESTS
        {
            return None;
        }
        // Never reuse IDs, including across resume: an old guard cannot remove a new request.
        let id = state.next_id.checked_add(1)?;
        state.next_id = id;
        state.active.insert(
            id,
            Entry {
                transport,
                long_lived: false,
                shutdown_requested: false,
            },
        );
        Some(RequestGuard {
            registry: self.clone(),
            id,
        })
    }

    pub(crate) fn active_count(&self) -> usize {
        self.state().active.len()
    }

    /// Stop admission and shut down already identified long-lived connections.
    pub(crate) fn begin_drain(&self) {
        let mut state = self.state();
        state.admission = Admission::Draining;
        for entry in state.active.values_mut().filter(|entry| entry.long_lived) {
            entry.shutdown();
        }
    }

    /// Request shutdown on every remaining transport, without changing active accounting.
    /// Returns newly requested shutdowns, not completed requests or successful kernel calls.
    pub(crate) fn force_drain(&self) -> usize {
        let mut state = self.state();
        state.admission = Admission::Draining;
        state
            .active
            .values_mut()
            .map(|entry| usize::from(entry.shutdown()))
            .sum()
    }

    /// Reopen admission only after all previous request guards have been released.
    pub(crate) fn resume(&self) -> bool {
        let mut state = self.state();
        if !state.active.is_empty() {
            return false;
        }
        state.admission = Admission::Open;
        true
    }
}

/// Hold for the complete application request, including any relay or upgraded stream.
/// The registry retains its OwnedFd until this guard drops; shutdown alone never removes it.
pub(crate) struct RequestGuard {
    registry: Arc<RequestDrain>,
    id: u64,
}

impl RequestGuard {
    pub(crate) fn mark_long_lived(&self) {
        let mut state = self.registry.state();
        let draining = matches!(state.admission, Admission::Draining);
        if let Some(entry) = state.active.get_mut(&self.id) {
            entry.long_lived = true;
            // The same lock orders registration, begin, and late upgrade/SSE classification.
            if draining {
                entry.shutdown();
            }
        }
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.registry.state().active.remove(&self.id);
    }
}

#[cfg(test)]
#[path = "request_drain/tests.rs"]
mod tests;
