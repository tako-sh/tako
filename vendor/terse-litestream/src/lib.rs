#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

#[cfg(feature = "wal-capture")]
mod database;
mod replica;
mod store;
#[cfg(feature = "wal-capture")]
mod wal;

#[cfg(feature = "wal-capture")]
pub use database::{CheckpointMode, Database, Options, SyncResult};
pub use replica::{compact, replicate, restore, restore_plan, restore_with};
pub use store::{Durability, FileStore, ReplicaStore, Segment, SegmentWriter};
#[cfg(feature = "wal-capture")]
pub use wal::CommitCursor;
