//! Validation and bounds for the network Raft catalog.
use crate::{
    Ack, Member, RegistrySnapshot, Revocation,
    api::{RuntimeError, RuntimeResult},
};
use serde::Serialize;
use std::io::Write;

pub const MAX_REGISTRY_ROWS: usize = 10_000;
pub const MAX_REGISTRY_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_RECORD_BYTES: usize = 128 * 1024;
/// Trusted server storage only. Never expose these mutation methods as RPCs.
/// Membership and denial records are immutable. ACKs are replaceable boot observations.
/// Snapshot capacity errors must fail closed at the caller, never fall back to old grants.
/// Mutations use `Capacity("registry admissions")` or `Capacity("registry record")`
/// only for candidate rejection before publication. Other capacity errors mean
/// unreadable registry state, including when a mutation encounters them internally.
pub trait RegistryStore {
    fn snapshot(&mut self) -> RuntimeResult<RegistrySnapshot>;
    fn append_member(&mut self, member: &Member) -> RuntimeResult<()>;
    fn append_revocation(&mut self, revocation: &Revocation) -> RuntimeResult<()>;
    fn append_ack(&mut self, ack: &Ack) -> RuntimeResult<()>;

    fn shared_snapshot(&mut self) -> RuntimeResult<crate::shared::SharedSnapshot> {
        Err(RuntimeError::Invalid("shared record store unavailable"))
    }

    fn append_record(&mut self, _record: &crate::shared::SharedRecord) -> RuntimeResult<()> {
        Err(RuntimeError::Invalid("shared record store unavailable"))
    }
}

pub(crate) fn bounded_json_size(value: &impl Serialize) -> RuntimeResult<usize> {
    struct Counter {
        bytes: usize,
        exceeded: bool,
    }
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > MAX_REGISTRY_BYTES - self.bytes {
                self.exceeded = true;
                return Err(std::io::Error::other("registry bytes"));
            }
            self.bytes += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter {
        bytes: 0,
        exceeded: false,
    };
    let result = serde_json::to_writer(&mut counter, value);
    if counter.exceeded {
        return Err(RuntimeError::Capacity("registry bytes"));
    }
    result?;
    Ok(counter.bytes)
}

/// ACKs are evidence of progress, never authority. Omitting one only reports pending.
pub(crate) fn bound_projection(snapshot: &mut RegistrySnapshot) -> RuntimeResult<()> {
    let acks = std::mem::take(&mut snapshot.acks);
    let rows = snapshot.members.len() + snapshot.revocations.len() + 1;
    if rows > MAX_REGISTRY_ROWS {
        return Err(RuntimeError::Capacity("registry projection"));
    }
    let mut bytes = bounded_json_size(snapshot)?;
    for ack in acks {
        if rows + snapshot.acks.len() >= MAX_REGISTRY_ROWS {
            break;
        }
        let size = serde_json::to_vec(&ack)?.len() + usize::from(!snapshot.acks.is_empty());
        if bytes + size <= MAX_REGISTRY_BYTES {
            bytes += size;
            snapshot.acks.push(ack);
        }
    }
    Ok(())
}
