//! Durable, per-resource Raft storage. Both engines replicate the bytes they
//! committed so followers never re-execute application SQL or recalculate TTLs.
mod catalog;
pub use catalog::Claim;
mod disk;
mod engines;
pub use engines::{RedbFile, SqliteWal};
mod writer;
pub use writer::{Resource, Writer};
mod log;
mod machine;
mod network;
pub use log::LogStore;
pub use machine::{Replica, SnapshotFile};
pub use network::{Peer, Progress};

use openraft::async_runtime::WatchReceiver;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One transaction's final page images. Page numbers start at zero. The
/// base revision identifies the exact database from which the writer started.
#[derive(Clone, Serialize, Deserialize)]
pub struct Commit {
    pub base_revision: u64,
    pub page_size: u32,
    pub file_size: u64,
    pub pages: BTreeMap<u64, Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Applied {
    Committed(u64),
    Stale { current_revision: u64 },
    Invalid,
    NoChange,
    Claimed(Claim),
    EngineConflict,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Proposal {
    Pages(Commit),
    Claim(Claim),
}

impl std::fmt::Display for Proposal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pages(commit) => std::fmt::Display::fmt(commit, f),
            Self::Claim(claim) => write!(f, "claim {}", claim.name),
        }
    }
}

openraft::declare_raft_types!(
    pub Types: D = Proposal, R = Applied, NodeId = u128, Node = std::net::SocketAddr,
);

impl std::fmt::Display for Commit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} pages based on revision {}",
            self.pages.len(),
            self.base_revision
        )
    }
}

/// Reconcile the union of the servers that need a resource. Keep current
/// voters where possible to avoid elections during an unrelated deploy.
pub async fn reconcile(
    raft: &openraft::Raft<Types, Replica>,
    targets: &BTreeMap<u128, std::net::SocketAddr>,
) -> std::io::Result<()> {
    use std::collections::BTreeSet;
    if targets.is_empty() {
        return Err(disk::error(
            "removing the final replica requires resource deletion",
        ));
    }
    let membership = raft.metrics().borrow_watched().membership_config.clone();
    let before: BTreeSet<_> = membership.membership().voter_ids().collect();
    let desired = targets.keys().copied().collect::<BTreeSet<_>>();
    let mut voters = before
        .intersection(&desired)
        .copied()
        .take(3)
        .collect::<BTreeSet<_>>();
    for id in desired.iter().copied() {
        if voters.len() == 3 {
            break;
        }
        voters.insert(id);
    }
    for (&id, &address) in targets {
        if membership.membership().get_node(&id) != Some(&address) || !before.contains(&id) {
            raft.add_learner(id, address, true)
                .await
                .map_err(disk::error)?;
        }
    }
    if before != voters {
        raft.change_membership(voters, true)
            .await
            .map_err(disk::error)?;
    }
    let retired: BTreeSet<_> = membership
        .membership()
        .nodes()
        .map(|(id, _)| *id)
        .filter(|id| !targets.contains_key(id))
        .collect();
    if !retired.is_empty() {
        raft.change_membership(openraft::ChangeMembers::RemoveNodes(retired), false)
            .await
            .map_err(disk::error)?;
    }
    Ok(())
}

impl std::fmt::Debug for Commit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}
