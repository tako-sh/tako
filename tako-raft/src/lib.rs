//! Durable, per-resource Raft storage. Both engines replicate the bytes they
//! committed so followers never re-execute application SQL or recalculate TTLs.
pub use openraft::{Config, Raft};
pub type Node = Raft<Types, Replica>;
mod catalog;
mod placement;
pub use catalog::Claim;
mod disk;
mod engines;
pub use engines::RedbFile;
mod writer;
pub use writer::{Resource, Writer};
mod log;
mod machine;
mod network;
pub use log::LogStore;
pub use machine::{BackupPosition, Replica, SnapshotFile};
pub use network::{Peer, Progress};

use openraft::async_runtime::WatchReceiver;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One transaction's final page images. Page numbers start at zero. The
/// base revision identifies the exact database from which the writer started.
#[derive(Clone, Serialize, Deserialize)]
pub struct Commit {
    pub base_revision: u64,
    pub replace: bool,
    pub captured_at_ms: i64,
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
    Placement(tako_core::ResourcePlacement),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Proposal {
    Pages(Commit),
    CatalogUpdate {
        shared: bool,
        expected: u64,
        #[serde(with = "serde_bytes")]
        value: Vec<u8>,
    },
    Placement(tako_core::PlacementChange),
    Retirement {
        generation: String,
        targets: std::collections::BTreeSet<String>,
    },
    RetireName {
        name: String,
        generation: String,
        expected: u64,
        #[serde(with = "serde_bytes")]
        shared: Vec<u8>,
    },
    BackupSettings {
        track: bool,
        sealed: Vec<u8>,
    },
    PublishClaim {
        candidate: Claim,
        expected: u64,
        #[serde(with = "serde_bytes")]
        shared: Vec<u8>,
    },
    Grant {
        scope: String,
        key_hash: Option<[u8; 32]>,
    },
    CommitPart {
        base: u64,
        nonce: [u8; 16],
        index: u64,
        last: bool,
        #[serde(with = "serde_bytes")]
        bytes: Vec<u8>,
    },
}

impl std::fmt::Display for Proposal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CatalogUpdate { .. } => f.write_str("update network catalog"),
            Self::Placement(_) => f.write_str("placement"),
            Self::Retirement { generation, .. } => write!(f, "retirement {generation}"),
            Self::RetireName { name, .. } => write!(f, "retire {name}"),
            Self::CommitPart { index, .. } => write!(f, "commit part {index}"),
            Self::Pages(commit) => std::fmt::Display::fmt(commit, f),
            Self::BackupSettings { track: enabled, .. } => write!(f, "track backups {enabled}"),
            Self::Grant { scope, .. } => write!(f, "grant {scope}"),
            Self::PublishClaim {
                candidate: claim, ..
            } => write!(f, "claim {}", claim.name),
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
    reconcile_voters(raft, targets).await?;
    let retired: std::collections::BTreeSet<_> = raft
        .metrics()
        .borrow_watched()
        .membership_config
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

/// Keep departing servers as learners until the catalog commits their denial.
/// This lets the old voting majority acknowledge its own replacement.
pub async fn reconcile_voters(
    raft: &Node,
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
    if before != voters
        || targets.len() != membership.membership().nodes().count()
        || targets
            .iter()
            .any(|(id, addr)| membership.membership().get_node(id) != Some(addr))
    {
        // OpenRaft permits membership changes after the new leader has
        // committed its first entry for this term.
        raft.ensure_linearizable(openraft::ReadPolicy::ReadIndex)
            .await
            .map_err(disk::error)?;
    }
    for (&id, &address) in targets {
        if membership.membership().get_node(&id) != Some(&address)
            || (voters.contains(&id) && !before.contains(&id))
        {
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
    Ok(())
}

impl std::fmt::Debug for Commit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

/// Update routes on elections. Ordinary log application does not rebuild
/// an application's token table.
pub async fn watch_leader(node: &Node, mut update: impl FnMut()) {
    let mut receiver = node.metrics();
    let mut previous = receiver.borrow_watched().current_leader;
    update();
    while receiver.changed().await.is_ok() {
        let leader = receiver.borrow_watched().current_leader;
        if previous != leader {
            previous = leader;
            update();
        }
    }
}

/// Add deployment targets while retaining replicas used by other applications
/// and their rollback releases. The caller serializes changes on this leader.
pub async fn add_members(
    raft: &Node,
    mut targets: BTreeMap<u128, std::net::SocketAddr>,
) -> std::io::Result<()> {
    raft.ensure_linearizable(openraft::ReadPolicy::ReadIndex)
        .await
        .map_err(disk::error)?;
    let config = raft.metrics().borrow_watched().membership_config.clone();
    for (&id, &addr) in config.membership().nodes() {
        targets.entry(id).or_insert(addr);
    }
    reconcile(raft, &targets).await
}

pub async fn remove_member(node: &Node, id: u128) -> std::io::Result<()> {
    node.ensure_linearizable(openraft::ReadPolicy::ReadIndex)
        .await
        .map_err(disk::error)?;
    let membership = node.metrics().borrow_watched().membership_config.clone();
    let mut members = membership
        .membership()
        .nodes()
        .map(|(id, addr)| (*id, *addr))
        .collect::<BTreeMap<_, _>>();
    if members.remove(&id).is_none() {
        return Ok(());
    }
    reconcile(node, &members).await
}
